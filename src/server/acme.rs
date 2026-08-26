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
//!
//! The single issuance worker bounds every per-host attempt with
//! [`ISSUANCE_TIMEOUT`], so a hung ACME server (or an unresponsive DNS-01
//! provider) fails the host into cooldown instead of monopolizing the worker;
//! the provider's own requests are separately bounded (finite timeouts + the
//! shared resolver pool), so even synchronous cleanup terminates.

use crate::config::ast::DnsChallenge;
use crate::server::dns::{self, DnsProvider, DnsRecord};
use crate::server::issuance_queue::{AcmeQueue, EnqueueOutcome, RequestKind};
use crate::tls::{CertStore, TlsAlpnChallengeStore};
use instant_acme::{
    Account, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
    RetryPolicy,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

/// Re-issue a certificate once it is within this window of its expiry.
const RENEW_WINDOW: Duration = Duration::from_secs(30 * 24 * 3600);

/// Wall-clock bound on a single issuance attempt at the worker boundary.
///
/// Real first-time issuance (account restore/create, new order, authorizations,
/// validation polls, finalize, certificate) can take well over a minute; 5
/// minutes bounds a hung attempt (a stalled ACME server, an unresponsive
/// authorizer) without being aggressive. When it fires the attempt future is
/// dropped; the synchronous DNS-01 cleanup then runs under the provider's
/// agent's own timeouts, so the single issuance worker always proceeds to the
/// next queued host.
const ISSUANCE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Bounded number of issuance requests that may wait for the worker at once.
/// The queue itself (in `server::issuance_queue`) also caps the per-host state
/// table, so neither grows with SNI-miss traffic. Startup sizes that table from
/// the configured hosts plus this queue capacity.
pub(crate) const ISSUANCE_QUEUE_CAPACITY: usize = 256;

/// The ACME DNS identifier for a certificate-store key: the host part, with a
/// `:port` suffix stripped when present. Store keys are the bare host on port
/// 443 and `host:port` on any other TLS port ([`crate::tls::cert_store_key`]);
/// `host:port` is not a valid DNS identifier, so every ACME order and DNS-01
/// record must use the bare name.
fn dns_name_for(store_key: &str) -> &str {
    match store_key.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => host,
        _ => store_key,
    }
}

/// In-memory HTTP-01 challenge registry: token -> key authorization.
///
/// Populated when an order is created, read by the proxy handler to answer
/// `/.well-known/acme-challenge/<token>`.
#[derive(Debug, Default)]
pub struct ChallengeStore {
    challenges: RwLock<HashMap<String, String>>,
}

/// RAII guard that removes all temporary TLS-ALPN-01 certificates after an
/// issuance attempt, including when the ACME future fails or times out.
struct TlsAlpnGuard {
    store: Arc<TlsAlpnChallengeStore>,
    hosts: Vec<String>,
}

impl TlsAlpnGuard {
    /// Create an empty guard for one issuance attempt.
    fn new(store: Arc<TlsAlpnChallengeStore>) -> Self {
        Self {
            store,
            hosts: Vec::new(),
        }
    }

    /// Register a challenge certificate and remember it for cleanup.
    fn register(&mut self, host: &str, digest: &[u8]) -> Result<(), String> {
        self.store.register(host, digest)?;
        self.hosts.push(host.to_ascii_lowercase());
        Ok(())
    }
}

impl Drop for TlsAlpnGuard {
    fn drop(&mut self) {
        for host in self.hosts.drain(..) {
            self.store.remove(&host);
        }
    }
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
    tls_alpn_challenges: Arc<TlsAlpnChallengeStore>,
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
    /// DNS-01 challenge credentials (spec §5.3); when set, issuance proves
    /// domain control via DNS-01 (the provider's API) instead of HTTP-01.
    dns_challenge: Option<DnsChallenge>,
    /// Whether ACME should use TLS-ALPN-01 instead of HTTP-01 or DNS-01.
    tls_alpn_challenge: bool,
    /// Serializes issuance so account creation/order processing is never
    /// concurrent (a v0.1 simplification; `instant-acme` supports concurrent
    /// orders per account, but account creation races are messy on first run).
    issuance_lock: tokio::sync::Mutex<()>,
}

impl AcmeManager {
    /// Create a manager bound to one directory/account/cert dir.
    ///
    /// The certificate and challenge stores are process-local; the directory,
    /// root PEM, certificate directory, email, DNS challenge, and
    /// TLS-ALPN-01 flag select the ACME behavior. Returns a manager ready for
    /// worker startup; validation errors are reported when issuance begins.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<CertStore>,
        challenges: Arc<ChallengeStore>,
        tls_alpn_challenges: Arc<TlsAlpnChallengeStore>,
        directory_url: String,
        acme_root_pem: Option<String>,
        cert_dir: PathBuf,
        email: Option<String>,
        dns_challenge: Option<DnsChallenge>,
        tls_alpn_challenge: bool,
    ) -> Self {
        Self {
            store,
            challenges,
            tls_alpn_challenges,
            directory_url,
            acme_root_pem,
            cert_dir,
            email,
            dns_challenge,
            tls_alpn_challenge,
            issuance_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Load cached certificates from `cert_dir` into the store.
    pub fn load_persisted_certs(&self) {
        load_persisted_certs(&self.store, &self.cert_dir);
    }

    /// Spawn a background worker thread that processes issuance requests
    /// serially, and return the bounded queue handle. The startup batch, the
    /// on-demand SNI-miss path, and the renewal scheduler all push requests
    /// into this queue through [`AcmeQueue::enqueue`]; renewals
    /// ([`RequestKind::Renew`]) bypass the has-certificate check.
    ///
    /// `max_hosts` bounds the per-host state table and must be sized from the
    /// authorized configured hosts so an SNI-miss flood cannot grow memory.
    pub fn spawn_issuance_worker(self: &Arc<Self>, max_hosts: usize) -> Arc<AcmeQueue> {
        let queue = AcmeQueue::new(ISSUANCE_QUEUE_CAPACITY, max_hosts);
        let manager = self.clone();
        let worker_queue = queue.clone();
        std::thread::spawn(move || {
            // `instant-acme` is async; give this worker its own runtime so it
            // can block on each issuance without touching the server's runtime.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build ACME worker runtime");
            loop {
                let request = worker_queue.next_request();
                let host = request.host.clone();
                // The whole per-host attempt (lock + account + order + cert) is
                // bounded by ISSUANCE_TIMEOUT; a hung attempt fails the host
                // into cooldown instead of monopolizing the single worker.
                let attempt = manager.issue_for(&host, request.kind.force());
                rt.block_on(process_attempt(
                    &worker_queue,
                    &host,
                    attempt,
                    ISSUANCE_TIMEOUT,
                ));
            }
        });
        queue
    }

    /// Spawn a background thread that periodically scans the store and queues a
    /// renewal for any certificate inside [`RENEW_WINDOW`] of expiry. Failures
    /// are logged by the issuance worker and retried on the next scan (or after
    /// the queue's failure cooldown); the old certificate keeps serving until a
    /// renewal succeeds (ARCHITECTURE §5).
    pub fn spawn_renewal_scheduler(
        self: &Arc<Self>,
        queue: Arc<AcmeQueue>,
        check_interval: Duration,
    ) {
        let manager = self.clone();
        std::thread::spawn(move || loop {
            let deadline = SystemTime::now() + RENEW_WINDOW;
            for host in manager.store.hosts_due_renewal(deadline) {
                tracing::info!("certificate for {host} is due for renewal; queuing reissue");
                match queue.enqueue(&host, RequestKind::Renew) {
                    EnqueueOutcome::Queued | EnqueueOutcome::UpgradeForced => {}
                    EnqueueOutcome::Duplicate => {
                        tracing::debug!("{host} renewal already queued or in flight")
                    }
                    EnqueueOutcome::InCooldown => tracing::debug!(
                        "{host} is in its failure cooldown; retrying on the next scan"
                    ),
                    EnqueueOutcome::QueueFull => {
                        tracing::warn!("ACME queue full; renewal for {host} deferred")
                    }
                }
            }
            std::thread::sleep(check_interval);
        });
    }

    /// Issue a certificate for a single hostname and publish it.
    ///
    /// `store_key` is the certificate-store key the caller enqueued — the bare
    /// host on port 443, `host:port` on a non-443 TLS listener (see
    /// [`crate::tls::cert_store_key`]). The ACME order and DNS-01 records use
    /// the *bare* DNS name (a `host:port` is not a valid DNS identifier), while
    /// the store and `cert_dir` persist under `store_key` so the SNI callback
    /// finds the certificate (P2, A1).
    ///
    /// Domain control is proven via HTTP-01 by default, or via DNS-01 when
    /// dns_challenge is configured, or via TLS-ALPN-01 when the corresponding
    /// global flag is enabled. With force (renewal) the existing certificate,
    /// if any, is replaced. Errors include ACME, challenge setup, polling,
    /// finalization, persistence, and cleanup failures.
    pub async fn issue_for(&self, store_key: &str, force: bool) -> Result<(), String> {
        if !force && self.store.has(store_key) {
            return Ok(());
        }
        let _guard = self.issuance_lock.lock().await;
        if !force && self.store.has(store_key) {
            return Ok(());
        }
        // The ACME DNS identifier: the host part of the store key, stripping a
        // `:port` suffix when present.
        let dns_name = dns_name_for(store_key);

        let account = self.account().await?;
        let identifiers = vec![Identifier::Dns(dns_name.to_string())];
        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(|e| format!("new_order for {store_key}: {e}"))?;

        // For each authorization, present the challenge then notify the server
        // it is ready. TLS-ALPN-01 serves a temporary certificate on 443;
        // HTTP-01 answers from the ChallengeStore; DNS-01 publishes a TXT
        // record through the provider (removed on drop via the guard).
        let challenge_type = if self.tls_alpn_challenge {
            ChallengeType::TlsAlpn01
        } else {
            match &self.dns_challenge {
                Some(_) => ChallengeType::Dns01,
                None => ChallengeType::Http01,
            }
        };
        let challenge_label = match &challenge_type {
            ChallengeType::Dns01 => "DNS-01",
            ChallengeType::Http01 => "HTTP-01",
            ChallengeType::TlsAlpn01 => "TLS-ALPN-01",
            _ => "challenge",
        };
        let mut dns_guard = match &self.dns_challenge {
            Some(dns) => Some(Dns01Guard::new(
                dns::build(dns.provider, &dns.api_token)
                    .map_err(|e| format!("dns-01 provider init: {e}"))?,
            )),
            None => None,
        };
        let mut tls_alpn_guard = self
            .tls_alpn_challenge
            .then(|| TlsAlpnGuard::new(self.tls_alpn_challenges.clone()));
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.map_err(|e| format!("authorization for {store_key}: {e}"))?;
            if authz.status == AuthorizationStatus::Valid {
                continue;
            }
            let mut challenge = authz
                .challenge(challenge_type.clone())
                .ok_or_else(|| format!("no {challenge_label} challenge offered for {store_key}"))?;
            let key_authorization = challenge.key_authorization();
            if matches!(&challenge_type, ChallengeType::TlsAlpn01) {
                let digest = key_authorization.digest();
                tls_alpn_guard
                    .as_mut()
                    .expect("TLS-ALPN guard")
                    .register(dns_name, digest.as_ref())
                    .map_err(|e| format!("TLS-ALPN-01 present for {store_key}: {e}"))?;
            } else {
                match &mut dns_guard {
                    Some(guard) => {
                        let dns_value = key_authorization.dns_value();
                        let handle = guard
                            .provider
                            .present(dns_name, &dns_value)
                            .map_err(|e| format!("dns-01 present for {store_key}: {e}"))?;
                        guard.handles.push(handle);
                    }
                    None => {
                        let token = challenge.token.clone();
                        self.challenges.register(&token, key_authorization.as_str());
                    }
                }
            }
            challenge
                .set_ready()
                .await
                .map_err(|e| format!("set_ready for {store_key}: {e}"))?;
        }

        let status = order
            .poll_ready(&RetryPolicy::default())
            .await
            .map_err(|e| format!("poll_ready for {store_key}: {e}"))?;
        if status != OrderStatus::Ready {
            return Err(format!("order for {store_key} not ready: {status:?}"));
        }
        let private_key_pem = order
            .finalize()
            .await
            .map_err(|e| format!("finalize for {store_key}: {e}"))?;
        let cert_chain_pem = order
            .poll_certificate(&RetryPolicy::default())
            .await
            .map_err(|e| format!("certificate for {store_key}: {e}"))?;

        tracing::info!("certificate issued for {store_key}");
        publish(
            &self.store,
            &self.cert_dir,
            store_key,
            &cert_chain_pem,
            &private_key_pem,
        )
        .map_err(|e| format!("failed to persist certificate for {store_key}: {e}"))?;
        Ok(())
    }

    /// Build the ACME account, resuming persisted credentials when present.
    async fn account(&self) -> Result<Account, String> {
        // Secure the cert dir first: the ACME root CA PEM below is written into
        // it, and account credentials are persisted here. A fixed /tmp path for
        // the root PEM would be world-writable and a symlink-attack vector, and
        // would race between concurrent instances.
        ensure_cert_dir(&self.cert_dir)
            .map_err(|e| format!("failed to secure cert dir {}: {e}", self.cert_dir.display()))?;
        let builder = match &self.acme_root_pem {
            Some(pem) => {
                let path = self.cert_dir.join("acme_root.pem");
                atomic_write(&path, pem, CERT_FILE_MODE)
                    .map_err(|e| format!("write acme root pem: {e}"))?;
                Account::builder_with_root(path).map_err(|e| format!("acme builder: {e}"))?
            }
            None => Account::builder().map_err(|e| format!("acme builder: {e}"))?,
        };

        // Resume a persisted account if one exists on disk.
        let account_path = self.cert_dir.join("account.json");
        if let Ok(json) = std::fs::read_to_string(&account_path) {
            if let Ok(credentials) = serde_json::from_str(&json) {
                // The persisted account holds the account private key; tighten
                // it (and the dir) now that we successfully read it.
                tighten_secret(&account_path, &self.cert_dir);
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
        // Persist atomically with private permissions. A failure is returned
        // rather than merely logged: losing the account means every restart
        // registers a fresh ACME account. (The cert dir was secured at the top
        // of `account`.)
        let json = serde_json::to_string(&credentials)
            .map_err(|e| format!("serialize account credentials: {e}"))?;
        atomic_write(&account_path, &json, PRIVATE_FILE_MODE).map_err(|e| {
            format!(
                "failed to persist ACME account credentials to {}: {e}",
                account_path.display()
            )
        })?;
        tracing::info!("ACME account created");
        Ok(account)
    }
}

/// Bound an issuance attempt with a wall-clock timeout.
///
/// `tokio::time::timeout` can only fire while the future is parked on an
/// `.await`; the synchronous DNS-01 provider calls inside the attempt are
/// separately bounded by the provider's Agent timeouts, so the whole attempt
/// always terminates even on a single-threaded runtime.
async fn run_with_timeout<Fut>(timeout: Duration, attempt: Fut) -> Result<(), String>
where
    Fut: std::future::Future<Output = Result<(), String>>,
{
    match tokio::time::timeout(timeout, attempt).await {
        Ok(result) => result,
        Err(_elapsed) => Err(format!("issuance timed out after {timeout:?}")),
    }
}

/// Run one host's issuance attempt under a wall-clock timeout and report the
/// outcome to the queue. Returns the success flag for tests.
///
/// A timeout is logged as a failure including the host and the bound, and the
/// host is always [`AcmeQueue::complete`]d — as a failure on timeout — so it
/// enters cooldown and the worker advances to the next queued host. Success
/// completes the host exactly once, preserving the queue's Renew upgrade
/// semantics.
async fn process_attempt<Fut>(
    queue: &AcmeQueue,
    host: &str,
    attempt: Fut,
    timeout: Duration,
) -> bool
where
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let result = run_with_timeout(timeout, attempt).await;
    let success = result.is_ok();
    if let Err(message) = result {
        tracing::error!("ACME issuance failed for {host}: {message}");
    }
    queue.complete(host, success);
    success
}

/// Publishes DNS-01 TXT records for an in-flight order and removes them when
/// the issuance attempt finishes, whether it succeeds or fails. A leaked record
/// would keep the challenge valid (and the record on the zone) indefinitely.
struct Dns01Guard {
    provider: Box<dyn DnsProvider>,
    handles: Vec<Box<dyn DnsRecord>>,
}

impl Dns01Guard {
    fn new(provider: Box<dyn DnsProvider>) -> Self {
        Self {
            provider,
            handles: Vec::new(),
        }
    }
}

impl Drop for Dns01Guard {
    fn drop(&mut self) {
        for handle in std::mem::take(&mut self.handles) {
            if let Err(e) = handle.cleanup() {
                tracing::warn!("failed to remove DNS-01 TXT record: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Secure persistence
//
// `cert_dir` holds private key material (each `<host>.key` and the ACME
// `account.json`), so the directory is created 0700 and those files are
// written 0600. All writes are atomic (temp file + rename in the same
// directory), so a crash never leaves a loadable half-written final file. Modes
// are set explicitly at open/creation time rather than relying on the process
// umask.
//
// A published certificate is a *pair* of files (`<host>.pem` + `<host>.key`),
// and POSIX rename cannot switch two files atomically. Writes are therefore
// staged in two phases: both temp files are written and fsynced first, then
// renamed into place. A crash between the two renames can still leave a
// mismatched pair on disk, so `load_persisted_certs` refuses any pair whose
// key does not match its certificate — the next `New` re-issues (repair).
// ---------------------------------------------------------------------------

/// Mode for `cert_dir`: private to the owning user.
const PRIVATE_DIR_MODE: u32 = 0o700;
/// Mode for files holding private key material (`account.json`, `<host>.key`).
const PRIVATE_FILE_MODE: u32 = 0o600;
/// Mode for issued certificates (public by nature; readable, not world-writable).
const CERT_FILE_MODE: u32 = 0o644;

/// Process-unique counter folded into temp file names. A bare PID is not
/// enough: two concurrent writers in one process would pick the same temp name
/// and delete each other's files. The counter makes every `stage_write` (and
/// every retry after an `AlreadyExists`) choose a fresh name.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Ensure `dir` exists and is private (0700 on Unix). Existing directories are
/// tightened, since an older version may have created `cert_dir` world-readable.
fn ensure_cert_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    set_mode(dir, PRIVATE_DIR_MODE)
}

/// Set `path`'s permissions to `mode` on Unix; a no-op elsewhere.
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// A temp file staged beside its target, ready to be renamed into place.
///
/// [`stage_write`] creates it (write + fsync); [`StagedWrite::commit`] renames
/// it over the final path and fsyncs the parent directory so the rename itself
/// survives a crash. Dropping without committing removes the temp file
/// best-effort, so a failure at any point never leaves debris behind.
struct StagedWrite {
    tmp: PathBuf,
    final_path: PathBuf,
    committed: bool,
}

impl StagedWrite {
    /// Publish the staged contents by renaming over `final_path`, then fsync
    /// the parent directory (Unix). On error the temp file is removed by
    /// `Drop`; the caller sees the error and can retry the whole write.
    fn commit(mut self) -> std::io::Result<()> {
        std::fs::rename(&self.tmp, &self.final_path)?;
        self.committed = true;
        sync_parent_dir(&self.final_path)
    }
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Open `path` for exclusive creation, applying `mode` on Unix. `create_new`
/// guarantees we never clobber an existing file — another live writer's temp,
/// or a stale temp left by a crashed process whose PID was reused.
#[cfg(unix)]
fn open_exclusive(path: &Path, mode: u32) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
}

#[cfg(not(unix))]
fn open_exclusive(path: &Path, _mode: u32) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// The next process-unique temp path for `file_name` in `dir`.
fn next_temp_path(dir: &Path, file_name: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{file_name}.{}.{n}.tmp", std::process::id()))
}

/// Write `contents` to a fresh temp file beside `path` with `mode` permissions
/// and fsync it. The final rename is left to [`StagedWrite::commit`] so a
/// caller can stage several files (the cert + key pair) before publishing any
/// of them, shrinking the crash window between renames.
///
/// On write/sync failure the temp file is removed before the error returns. An
/// `AlreadyExists` (stale debris from a recycled PID) picks a fresh name rather
/// than deleting what it found.
fn stage_write(path: &Path, contents: &str, mode: u32) -> std::io::Result<StagedWrite> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent directory")
    })?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let tmp = loop {
        let candidate = next_temp_path(dir, file_name);
        match open_exclusive(&candidate, mode) {
            Ok(mut f) => {
                use std::io::Write;
                if let Err(e) = f.write_all(contents.as_bytes()).and_then(|()| f.sync_all()) {
                    drop(f);
                    let _ = std::fs::remove_file(&candidate);
                    return Err(e);
                }
                break candidate;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    Ok(StagedWrite {
        tmp,
        final_path: path.to_path_buf(),
        committed: false,
    })
}

/// fsync the directory containing `path` (Unix) so a completed rename survives
/// a crash. Best-effort durability; a no-op on platforms without directory
/// fsync.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent directory")
    })?;
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Atomically write `contents` to `path` with `mode` permissions: stage the
/// contents as a temp file, then rename it over the final path. A reader never
/// observes a partial file, and a crash leaves only a stale temp file — never
/// a loadable half-written final file.
fn atomic_write(path: &Path, contents: &str, mode: u32) -> std::io::Result<()> {
    stage_write(path, contents, mode)?.commit()
}

/// True when `key_pem` is the private key whose public half matches the leaf of
/// the certificate chain in `cert_chain_pem`. Used at load time so a mismatched
/// pair — possible if a crash lands between the two rename steps of
/// [`publish`] — is refused instead of served as a valid certificate.
fn key_matches_cert_chain(cert_chain_pem: &str, key_pem: &str) -> bool {
    let Ok(certs) = openssl::x509::X509::stack_from_pem(cert_chain_pem.as_bytes()) else {
        return false;
    };
    let Some(leaf) = certs.first() else {
        return false;
    };
    let (Ok(cert_public), Ok(key)) = (
        leaf.public_key(),
        openssl::pkey::PKey::private_key_from_pem(key_pem.as_bytes()),
    ) else {
        return false;
    };
    key.public_eq(&cert_public)
}

/// Best-effort tightening of a secret file and its directory after a successful
/// read. Older versions may have written them world-readable; failures are
/// logged rather than fatal because the read already succeeded and the next
/// overwrite path re-secures them.
fn tighten_secret(file: &Path, dir: &Path) {
    if let Err(e) = set_mode(dir, PRIVATE_DIR_MODE) {
        tracing::warn!("failed to tighten cert dir {}: {e}", dir.display());
    }
    if let Err(e) = set_mode(file, PRIVATE_FILE_MODE) {
        tracing::warn!("failed to tighten permissions on {}: {e}", file.display());
    }
}

/// Load `<host>.pem`/`<host>.key` pairs from `cert_dir` into the store.
fn load_persisted_certs(store: &Arc<CertStore>, cert_dir: &Path) {
    // A cert_dir written by an older version may be world-readable; tighten it
    // before reading the cached secrets.
    if cert_dir.is_dir() {
        if let Err(e) = set_mode(cert_dir, PRIVATE_DIR_MODE) {
            tracing::warn!(
                "failed to tighten cert dir {} permissions: {e}",
                cert_dir.display()
            );
        }
    }
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
            Ok(cert) if key_matches_cert_chain(&cert_pem, &key_pem) => {
                store.store(&host, cert);
                // The cached key is private; tighten it now that we loaded it.
                if let Err(e) = set_mode(&key_path, PRIVATE_FILE_MODE) {
                    tracing::warn!(
                        "failed to tighten permissions on {}: {e}",
                        key_path.display()
                    );
                }
                tracing::info!("loaded cached certificate for {host}");
            }
            Ok(_) => tracing::warn!(
                "cached certificate for {host} does not match its private key; skipping (next issuance repairs it)"
            ),
            Err(e) => tracing::warn!("failed to load cached certificate for {host}: {e}"),
        }
    }
}

/// Publish an issued certificate: write `<host>.pem` and `<host>.key` to disk
/// atomically, then insert into the store so a future restart resumes it. The
/// key and `cert_dir` are secured to private permissions.
///
/// Both files are staged (temp write + fsync) before either final name is
/// renamed into place, narrowing the crash window between the two renames; a
/// crash there can still leave a mismatched pair, which [`load_persisted_certs`]
/// refuses to load and the next `New` repairs by re-issuing. The store is
/// updated only after both files are durably published, so a failed publish
/// never leaves an in-memory certificate that `has()` would use to short-circuit
/// a later retry. Returns an error describing the first step that failed.
fn publish(
    store: &Arc<CertStore>,
    cert_dir: &Path,
    host: &str,
    cert_chain_pem: &str,
    key_pem: &str,
) -> Result<(), String> {
    let cert = crate::tls::cert_key_from_pem(cert_chain_pem, key_pem)
        .map_err(|e| format!("failed to parse issued certificate for {host}: {e}"))?;
    ensure_cert_dir(cert_dir)
        .map_err(|e| format!("failed to secure cert dir {}: {e}", cert_dir.display()))?;
    let cert_path = cert_dir.join(format!("{host}.pem"));
    let key_path = cert_dir.join(format!("{host}.key"));
    // Stage both temp files (write + fsync) before publishing either final
    // name, so a crash between the renames leaves at most a mismatched pair,
    // never a half-written final file.
    let cert_staged = stage_write(&cert_path, cert_chain_pem, CERT_FILE_MODE)
        .map_err(|e| format!("failed to stage {}: {e}", cert_path.display()))?;
    let key_staged = stage_write(&key_path, key_pem, PRIVATE_FILE_MODE)
        .map_err(|e| format!("failed to stage {}: {e}", key_path.display()))?;
    cert_staged
        .commit()
        .map_err(|e| format!("failed to publish {}: {e}", cert_path.display()))?;
    key_staged
        .commit()
        .map_err(|e| format!("failed to publish {}: {e}", key_path.display()))?;
    // Only once both final files are durably in place may the certificate enter
    // the store: a store entry without a persisted copy would make `New`
    // short-circuit via `has()` while a restart would lose it entirely.
    store.store(host, cert);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_name_for_strips_a_port_suffix() {
        // Store keys are bare hosts on 443 and `host:port` elsewhere; the ACME
        // identifier must always be the bare DNS name (A1/A3).
        assert_eq!(dns_name_for("example.test"), "example.test");
        assert_eq!(dns_name_for("example.test:8443"), "example.test");
        assert_eq!(dns_name_for("example.test:443"), "example.test");
    }

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

    #[test]
    fn atomic_write_sets_mode_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("raddy_atomic_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.txt");

        atomic_write(&path, "v1", PRIVATE_FILE_MODE).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
        // Overwrite (the renewal path) must succeed — the final write is a
        // rename, never a create_new on an existing file.
        atomic_write(&path, "v2", PRIVATE_FILE_MODE).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, PRIVATE_FILE_MODE, "secret must be 0600");
        }
        // No stale temp files may remain after a successful write.
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stale temp files: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_failure_leaves_no_final_file() {
        // The parent directory does not exist, so the write fails; there must
        // be no half-written final file left behind that a later load could
        // mistake for a complete one.
        let missing_dir =
            std::env::temp_dir().join(format!("raddy_missing_test_{}", std::process::id()));
        let path = missing_dir.join("secret.txt");
        assert!(atomic_write(&path, "x", PRIVATE_FILE_MODE).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn ensure_cert_dir_tightens_existing_directory() {
        let dir = std::env::temp_dir().join(format!("raddy_certdir_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        ensure_cert_dir(&dir).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "cert dir must be 0700"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publish_writes_private_key_public_cert_and_overwrites() {
        let store = Arc::new(CertStore::new());
        let dir = std::env::temp_dir().join(format!("raddy_publish_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let (cert_pem, key_pem) = rcgen_test_cert("example.test");
        publish(&store, &dir, "example.test", &cert_pem, &key_pem).expect("first publish");
        assert!(store.has("example.test"));
        assert!(dir.join("example.test.pem").exists());
        assert!(dir.join("example.test.key").exists());

        // Overwrite (renewal) with a freshly generated key must succeed and
        // replace both files on disk.
        let (cert_pem2, key_pem2) = rcgen_test_cert("example.test");
        publish(&store, &dir, "example.test", &cert_pem2, &key_pem2).expect("renewal overwrite");
        assert_ne!(key_pem, key_pem2, "each rcgen call must yield a fresh key");
        assert_eq!(
            std::fs::read_to_string(dir.join("example.test.key")).unwrap(),
            key_pem2
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("example.test.pem")).unwrap(),
            cert_pem2
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key_mode = std::fs::metadata(dir.join("example.test.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(key_mode & 0o777, 0o600, "private key must be 0600");
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o777, 0o700, "cert dir must be 0700");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_persisted_certs_tightens_loose_key() {
        let store = Arc::new(CertStore::new());
        let dir = std::env::temp_dir().join(format!("raddy_tighten_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (cert_pem, key_pem) = rcgen_test_cert("example.test");
        std::fs::write(dir.join("example.test.pem"), &cert_pem).unwrap();
        std::fs::write(dir.join("example.test.key"), &key_pem).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Simulate files written by an older version with loose perms.
            std::fs::set_permissions(
                dir.join("example.test.key"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        load_persisted_certs(&store, &dir);
        assert!(store.has("example.test"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join("example.test.key"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "loaded key must be tightened to 0600"
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "cert dir must be tightened to 0700"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publish_failure_keeps_store_empty_and_retry_recovers() {
        let store = Arc::new(CertStore::new());
        let dir =
            std::env::temp_dir().join(format!("raddy_publish_fail_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (cert_pem, key_pem) = rcgen_test_cert("example.test");

        // Block the key's final path with a directory so the second rename
        // fails (rename(2) refuses to replace a directory with a file). This
        // simulates a disk/rename failure after the cert file was already
        // renamed into place.
        std::fs::create_dir(dir.join("example.test.key")).unwrap();

        let err = publish(&store, &dir, "example.test", &cert_pem, &key_pem).unwrap_err();
        assert!(
            err.contains("failed to publish"),
            "expected a rename failure, got: {err}"
        );
        // The store must not see the certificate: `issue_for(New)` would
        // otherwise short-circuit on `has()` and never retry.
        assert!(
            !store.has("example.test"),
            "store must not hold a certificate that failed to persist"
        );
        // Both staged temps must be gone: the cert temp was consumed by its
        // successful rename, the key temp by best-effort cleanup on failure.
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stale temp files: {leftovers:?}");

        // Repair: remove the blocker and retry. The retry must succeed and the
        // store must now hold the cert — this is the path that fixes a
        // mismatched/incomplete pair left by a crash mid-publish.
        std::fs::remove_dir(dir.join("example.test.key")).unwrap();
        publish(&store, &dir, "example.test", &cert_pem, &key_pem).expect("retry succeeds");
        assert!(store.has("example.test"));
        assert_eq!(
            std::fs::read_to_string(dir.join("example.test.key")).unwrap(),
            key_pem
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("example.test.pem")).unwrap(),
            cert_pem
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_persisted_certs_skips_mismatched_pair() {
        let store = Arc::new(CertStore::new());
        let dir = std::env::temp_dir().join(format!("raddy_mismatch_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (cert_pem, _key_pem) = rcgen_test_cert("example.test");
        let (_other_cert_pem, other_key_pem) = rcgen_test_cert("other.test");
        // A pair whose key belongs to a different certificate — exactly what a
        // crash between publish's two renames can leave on disk. Both parse as
        // valid PEM, so only the key-match check can refuse them.
        std::fs::write(dir.join("example.test.pem"), &cert_pem).unwrap();
        std::fs::write(dir.join("example.test.key"), &other_key_pem).unwrap();

        load_persisted_certs(&store, &dir);
        assert!(
            !store.has("example.test"),
            "a mismatched cert/key pair must not be loaded as valid"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_matches_cert_chain_compares_public_keys() {
        let (cert_pem, key_pem) = rcgen_test_cert("example.test");
        assert!(
            key_matches_cert_chain(&cert_pem, &key_pem),
            "own key must match"
        );
        let (_other_cert, other_key) = rcgen_test_cert("other.test");
        assert!(
            !key_matches_cert_chain(&cert_pem, &other_key),
            "another cert's key must not match"
        );
        assert!(!key_matches_cert_chain("not a certificate", &key_pem));
        assert!(!key_matches_cert_chain(&cert_pem, "not a key"));
    }

    #[tokio::test]
    async fn issuance_timeout_bounds_a_never_completing_attempt() {
        // A hung attempt (e.g. a stalled ACME server) must be cut off by the
        // injected timeout and reported as a clear timeout failure, promptly.
        let start = std::time::Instant::now();
        let result = run_with_timeout(Duration::from_millis(30), std::future::pending()).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "the timeout must fire promptly, took {elapsed:?}"
        );
        let err = result.expect_err("a never-completing attempt must time out");
        assert!(err.contains("timed out after"), "got: {err}");
        assert!(
            err.contains("30ms"),
            "the bound must appear in the error, got: {err}"
        );
    }

    #[tokio::test]
    async fn timed_out_attempt_completes_failure_and_worker_advances() {
        // Worker-step abstraction: after host A's attempt times out and is
        // completed as a failure (entering cooldown), the worker proceeds to
        // host B, which succeeds. This is the guarantee that a hung issuance
        // can never permanently monopolize the single worker.
        let queue = AcmeQueue::with_cooldown(4, 4, Duration::from_secs(600));
        queue.enqueue("a.test", RequestKind::New);
        queue.enqueue("b.test", RequestKind::New);

        // Host A: never-completing attempt under a short injected timeout.
        let req_a = queue.next_request();
        assert_eq!(req_a.host, "a.test");
        let success = process_attempt(
            &queue,
            &req_a.host,
            std::future::pending::<Result<(), String>>(),
            Duration::from_millis(30),
        )
        .await;
        assert!(!success, "a timed-out attempt is a failure");
        assert_eq!(
            queue.enqueue("a.test", RequestKind::New),
            EnqueueOutcome::InCooldown,
            "the failed host must be in cooldown"
        );

        // Host B: a ready success is processed after A timed out.
        let req_b = queue.next_request();
        assert_eq!(req_b.host, "b.test");
        let success = process_attempt(
            &queue,
            &req_b.host,
            async { Ok(()) },
            Duration::from_millis(30),
        )
        .await;
        assert!(success, "a ready attempt succeeds");
        assert_eq!(
            queue.enqueue("b.test", RequestKind::New),
            EnqueueOutcome::Queued,
            "a successful host is cleared and can be queued fresh"
        );
    }

    #[tokio::test]
    async fn successful_attempt_completes_host() {
        // A successful attempt completes the host exactly once and clears its
        // state, preserving the success semantics the queue already covers.
        let queue = AcmeQueue::with_cooldown(4, 4, Duration::from_secs(600));
        queue.enqueue("ok.test", RequestKind::New);
        let request = queue.next_request();
        assert_eq!(request.host, "ok.test");
        let success = process_attempt(
            &queue,
            &request.host,
            async { Ok(()) },
            Duration::from_millis(30),
        )
        .await;
        assert!(success);
        assert_eq!(
            queue.enqueue("ok.test", RequestKind::New),
            EnqueueOutcome::Queued,
            "the completed host is no longer tracked"
        );
    }

    /// Generate a self-signed certificate via `rcgen` (dev/test helper).
    fn rcgen_test_cert(host: &str) -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec![host.to_string()])
            .expect("failed to generate test certificate");
        (cert.cert.pem(), cert.signing_key.serialize_pem())
    }
}
