// Copyright (c) 2026 chulingera2025
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! ACME issuance orchestration and the HTTP-01 challenge responder (M4).
//!
//! Per ADR-003, certificates come from an ACME server via `instant-acme` and
//! land in the [`crate::tls::CertStore`] (in memory) plus a disk cache in
//! `cert_dir`. HTTP-01 challenges are answered on the plain-HTTP listener at
//! `/.well-known/acme-challenge/<token>` (wired into the proxy handler through
//! the [`ChallengeStore`]). On-demand TLS (SNI miss) is authorized by an `ask`
//! callback that only admits hostnames configured on this instance.
//!
//! M8 adds renewal: a background scheduler scans the store and queues a re-issue
//! for any certificate inside [`RENEW_WINDOW`] of expiry. A failed renewal keeps
//! the previous certificate in service and is retried on the next scan
//! (ARCHITECTURE §5).

use crate::tls::CertStore;
use instant_acme::{
    Account, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
    RetryPolicy,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

/// Re-issue a certificate once it is within this window of its expiry.
const RENEW_WINDOW: Duration = Duration::from_secs(30 * 24 * 3600);

/// An issuance request queued to the background worker.
#[derive(Debug)]
pub enum Issuance {
    /// Issue only if the host has no certificate yet (startup + on-demand).
    New(String),
    /// Re-issue even though a (soon-to-expire) certificate exists (renewal).
    Renew(String),
}

/// In-memory HTTP-01 challenge registry: token -> key authorization.
///
/// Populated when an order is created, read by the proxy handler to answer
/// `/.well-known/acme-challenge/<token>`.
#[derive(Debug, Default)]
pub struct ChallengeStore {
    challenges: RwLock<HashMap<String, String>>,
}

impl ChallengeStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or overwrite) the response for a challenge token.
    pub fn register(&self, token: &str, key_authorization: &str) {
        self.challenges
            .write()
            .expect("challenge store lock poisoned")
            .insert(token.to_string(), key_authorization.to_string());
    }

    /// Look up the response for a challenge token (if registered).
    pub fn lookup(&self, token: &str) -> Option<String> {
        self.challenges
            .read()
            .expect("challenge store lock poisoned")
            .get(token)
            .cloned()
    }
}

/// Orchestrates ACME issuance against one directory, on one account.
pub struct AcmeManager {
    store: Arc<CertStore>,
    challenges: Arc<ChallengeStore>,
    /// The ACME directory URL (Let's Encrypt production, or a Pebble test
    /// server such as `https://localhost:14000/dir`).
    directory_url: String,
    /// PEM of a custom root CA for the ACME server (required for Pebble whose
    /// CA is not publicly trusted), or `None` for the default trust roots.
    acme_root_pem: Option<String>,
    /// Directory for persisted certificates and account credentials.
    cert_dir: PathBuf,
    /// Contact email from `acme_email` (required by Let's Encrypt).
    email: Option<String>,
    /// Serializes issuance so account creation/order processing is never
    /// concurrent (a v0.1 simplification; `instant-acme` supports concurrent
    /// orders per account, but account creation races are messy on first run).
    issuance_lock: tokio::sync::Mutex<()>,
}

impl AcmeManager {
    /// Create a manager bound to one directory/account/cert dir.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<CertStore>,
        challenges: Arc<ChallengeStore>,
        directory_url: String,
        acme_root_pem: Option<String>,
        cert_dir: PathBuf,
        email: Option<String>,
    ) -> Self {
        Self {
            store,
            challenges,
            directory_url,
            acme_root_pem,
            cert_dir,
            email,
            issuance_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Load cached certificates from `cert_dir` into the store.
    pub fn load_persisted_certs(&self) {
        load_persisted_certs(&self.store, &self.cert_dir);
    }

    /// Spawn a background worker thread that processes issuance requests
    /// serially, and return the queue sender. The startup batch, the on-demand
    /// SNI-miss path, and the renewal scheduler all push requests into this
    /// queue (renewals with `Issuance::Renew` bypass the has-certificate check).
    pub fn spawn_issuance_worker(self: &Arc<Self>) -> std::sync::mpsc::Sender<Issuance> {
        let (tx, rx) = std::sync::mpsc::channel::<Issuance>();
        let manager = self.clone();
        std::thread::spawn(move || {
            // `instant-acme` is async; give this worker its own runtime so it
            // can block on each issuance without touching the server's runtime.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build ACME worker runtime");
            while let Ok(request) = rx.recv() {
                let (host, force) = match request {
                    Issuance::New(host) => (host, false),
                    Issuance::Renew(host) => (host, true),
                };
                if let Err(e) = rt.block_on(manager.issue_for(&host, force)) {
                    tracing::error!("ACME issuance failed for {host}: {e}");
                }
            }
        });
        tx
    }

    /// Spawn a background thread that periodically scans the store and queues a
    /// renewal for any certificate inside [`RENEW_WINDOW`] of expiry. Failures
    /// are logged by the issuance worker and retried on the next scan; the old
    /// certificate keeps serving until a renewal succeeds (ARCHITECTURE §5).
    pub fn spawn_renewal_scheduler(
        self: &Arc<Self>,
        issuance_tx: std::sync::mpsc::Sender<Issuance>,
        check_interval: Duration,
    ) {
        let manager = self.clone();
        std::thread::spawn(move || loop {
            let deadline = SystemTime::now() + RENEW_WINDOW;
            for host in manager.store.hosts_due_renewal(deadline) {
                tracing::info!("certificate for {host} is due for renewal; queuing reissue");
                let _ = issuance_tx.send(Issuance::Renew(host));
            }
            std::thread::sleep(check_interval);
        });
    }

    /// Issue a certificate for a single hostname via HTTP-01 and publish it.
    ///
    /// With `force` (renewal) the existing certificate, if any, is replaced.
    pub async fn issue_for(&self, host: &str, force: bool) -> Result<(), String> {
        if !force && self.store.has(host) {
            return Ok(());
        }
        let _guard = self.issuance_lock.lock().await;
        if !force && self.store.has(host) {
            return Ok(());
        }

        let account = self.account().await?;
        let identifiers = vec![Identifier::Dns(host.to_string())];
        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(|e| format!("new_order for {host}: {e}"))?;

        // Register the HTTP-01 response for every authorization, then notify
        // the server the challenge is ready.
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.map_err(|e| format!("authorization for {host}: {e}"))?;
            if authz.status == AuthorizationStatus::Valid {
                continue;
            }
            let mut challenge = authz
                .challenge(ChallengeType::Http01)
                .ok_or_else(|| format!("no HTTP-01 challenge offered for {host}"))?;
            let token = challenge.token.clone();
            let key_authorization = challenge.key_authorization().as_str().to_string();
            self.challenges.register(&token, &key_authorization);
            challenge
                .set_ready()
                .await
                .map_err(|e| format!("set_ready for {host}: {e}"))?;
        }

        let status = order
            .poll_ready(&RetryPolicy::default())
            .await
            .map_err(|e| format!("poll_ready for {host}: {e}"))?;
        if status != OrderStatus::Ready {
            return Err(format!("order for {host} not ready: {status:?}"));
        }
        let private_key_pem = order
            .finalize()
            .await
            .map_err(|e| format!("finalize for {host}: {e}"))?;
        let cert_chain_pem = order
            .poll_certificate(&RetryPolicy::default())
            .await
            .map_err(|e| format!("certificate for {host}: {e}"))?;

        tracing::info!("certificate issued for {host}");
        publish(
            &self.store,
            &self.cert_dir,
            host,
            &cert_chain_pem,
            &private_key_pem,
        );
        Ok(())
    }

    /// Build the ACME account, resuming persisted credentials when present.
    async fn account(&self) -> Result<Account, String> {
        let builder = match &self.acme_root_pem {
            Some(pem) => {
                let path = std::env::temp_dir().join("raddy_acme_root.pem");
                std::fs::write(&path, pem).map_err(|e| format!("write acme root pem: {e}"))?;
                Account::builder_with_root(path).map_err(|e| format!("acme builder: {e}"))?
            }
            None => Account::builder().map_err(|e| format!("acme builder: {e}"))?,
        };

        // Resume a persisted account if one exists on disk.
        let account_path = self.cert_dir.join("account.json");
        if let Ok(json) = std::fs::read_to_string(&account_path) {
            if let Ok(credentials) = serde_json::from_str(&json) {
                return builder
                    .from_credentials(credentials)
                    .await
                    .map_err(|e| format!("failed to restore ACME account: {e}"));
            }
        }

        // Create a fresh account and persist its credentials.
        let contacts: Vec<String> = self
            .email
            .iter()
            .map(|email| format!("mailto:{email}"))
            .collect();
        let contacts: Vec<&str> = contacts.iter().map(String::as_str).collect();
        let account = NewAccount {
            contact: &contacts,
            terms_of_service_agreed: true,
            only_return_existing: false,
        };
        let (account, credentials) = builder
            .create(&account, self.directory_url.clone(), None)
            .await
            .map_err(|e| format!("failed to create ACME account: {e}"))?;
        if let Err(e) = std::fs::create_dir_all(&self.cert_dir) {
            tracing::warn!("failed to create cert dir {}: {e}", self.cert_dir.display());
        }
        match serde_json::to_string(&credentials)
            .map_err(|e| format!("serialize account credentials: {e}"))
            .and_then(|json| {
                std::fs::write(&account_path, json).map_err(|e| format!("write account.json: {e}"))
            }) {
            Ok(()) => tracing::info!("ACME account created"),
            Err(e) => tracing::warn!("failed to persist ACME account credentials: {e}"),
        }
        Ok(account)
    }
}

/// Load `<host>.pem`/`<host>.key` pairs from `cert_dir` into the store.
fn load_persisted_certs(store: &Arc<CertStore>, cert_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(cert_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pem") {
            continue;
        }
        let Some(host) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let key_path = path.with_extension("key");
        let (Ok(cert_pem), Ok(key_pem)) = (
            std::fs::read_to_string(&path),
            std::fs::read_to_string(&key_path),
        ) else {
            continue;
        };
        match crate::tls::cert_key_from_pem(&cert_pem, &key_pem) {
            Ok(cert) => {
                store.store(&host, cert);
                tracing::info!("loaded cached certificate for {host}");
            }
            Err(e) => tracing::warn!("failed to load cached certificate for {host}: {e}"),
        }
    }
}

/// Publish an issued certificate: parse into a [`CertKey`], insert into the
/// store, and write to disk so a future restart resumes it.
fn publish(
    store: &Arc<CertStore>,
    cert_dir: &PathBuf,
    host: &str,
    cert_chain_pem: &str,
    key_pem: &str,
) {
    match crate::tls::cert_key_from_pem(cert_chain_pem, key_pem) {
        Ok(cert) => {
            store.store(host, cert);
            if let Err(e) = std::fs::create_dir_all(cert_dir) {
                tracing::warn!("failed to create cert dir {}: {e}", cert_dir.display());
            }
            let _ = std::fs::write(cert_dir.join(format!("{host}.pem")), cert_chain_pem);
            let _ = std::fs::write(cert_dir.join(format!("{host}.key")), key_pem);
        }
        Err(e) => {
            tracing::error!("failed to parse issued certificate for {host}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_store_register_lookup() {
        let store = ChallengeStore::new();
        assert_eq!(store.lookup("abc"), None);
        store.register("abc", "abc.key_auth");
        assert_eq!(store.lookup("abc").as_deref(), Some("abc.key_auth"));
        // Overwrite is allowed (a retry may re-register).
        store.register("abc", "abc.key_auth_v2");
        assert_eq!(store.lookup("abc").as_deref(), Some("abc.key_auth_v2"));
        assert_eq!(store.lookup("missing"), None);
    }

    #[test]
    fn load_persisted_certs_reads_pem_pairs() {
        let store = Arc::new(CertStore::new());
        let dir = std::env::temp_dir().join(format!("raddy_certstore_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // Write a `<host>.pem`/`<host>.key` pair plus a stray file that must be
        // ignored (no matching key).
        let (cert_pem, key_pem) = rcgen_test_cert("example.test");
        std::fs::write(dir.join("example.test.pem"), &cert_pem).unwrap();
        std::fs::write(dir.join("example.test.key"), &key_pem).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a cert").unwrap();

        load_persisted_certs(&store, &dir);
        assert!(
            store.has("example.test"),
            "expected loaded cert for example.test"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Generate a self-signed certificate via `rcgen` (dev/test helper).
    fn rcgen_test_cert(host: &str) -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec![host.to_string()])
            .expect("failed to generate test certificate");
        (cert.cert.pem(), cert.signing_key.serialize_pem())
    }
}
