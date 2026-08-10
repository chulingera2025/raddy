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

//! TLS certificate store and the SNI dynamic-certificate callback (M4).
//!
//! [`CertStore`] holds issued certificates in memory, keyed by hostname, and is
//! process-lifetime (across config reloads — certificates are not part of the
//! swapped snapshot). [`SniCallback`] implements pingora's `TlsAccept` so the
//! TLS handshake for a hostname is answered from the store; a miss triggers the
//! on-demand issuance path (authorized by the `ask` callback per ADR-003).
//!
//! Each stored certificate also records its `notAfter` (M8) so the renewal
//! scheduler can re-issue before expiry.

use crate::config::ast::{ClientAuthMode, SiteKey, TlsConfig, TlsVersion};
use crate::config::snapshot::ConfigStore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use async_trait::async_trait;
use openssl::ssl::{SslVerifyMode, SslVersion};
use openssl::x509::store::{X509Store, X509StoreBuilder};
use pingora::listeners::TlsAccept;
use pingora::protocols::tls::TlsRef;
use pingora::tls::{ext, pkey::PKey, ssl::NameType, x509::X509};
use pingora::utils::tls::CertKey;

/// A certificate plus the moment its leaf expires (parsed `notAfter`).
#[derive(Debug)]
pub struct CachedCert {
    cert: Arc<CertKey>,
    expires_at: SystemTime,
}

/// Process-lifetime store of certificates keyed by hostname.
#[derive(Debug, Default)]
pub struct CertStore {
    certs: RwLock<HashMap<String, CachedCert>>,
}

impl CertStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up the certificate for a hostname.
    pub fn get(&self, host: &str) -> Option<Arc<CertKey>> {
        self.certs
            .read()
            .expect("cert store lock poisoned")
            .get(host)
            .map(|entry| entry.cert.clone())
    }

    /// Whether a hostname has a certificate.
    pub fn has(&self, host: &str) -> bool {
        self.certs
            .read()
            .expect("cert store lock poisoned")
            .contains_key(host)
    }

    /// The expiry of a hostname's certificate (if any).
    pub fn expiry(&self, host: &str) -> Option<SystemTime> {
        self.certs
            .read()
            .expect("cert store lock poisoned")
            .get(host)
            .map(|entry| entry.expires_at)
    }

    /// Hostnames whose certificate expires on or before `before` — candidates
    /// for renewal. Unknown expiry (unparsable `notAfter`) is treated as never
    /// due rather than hammering ACME on every scan.
    pub fn hosts_due_renewal(&self, before: SystemTime) -> Vec<String> {
        self.certs
            .read()
            .expect("cert store lock poisoned")
            .iter()
            .filter(|(_, entry)| entry.expires_at <= before)
            .map(|(host, _)| host.clone())
            .collect()
    }

    /// Insert or replace the certificate for a hostname, recording its expiry.
    pub fn store(&self, host: &str, cert: CertKey) {
        let expires_at = leaf_not_after(&cert).unwrap_or_else(|| {
            // Unreachable for a cert that parsed as valid PEM; fall back to
            // never-due so a parsing regression cannot cause a renewal storm.
            tracing::warn!("failed to read notAfter for {host}; renewal disabled for it");
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100 * 365 * 24 * 3600)
        });
        self.certs
            .write()
            .expect("cert store lock poisoned")
            .insert(
                host.to_string(),
                CachedCert {
                    cert: Arc::new(cert),
                    expires_at,
                },
            );
    }
}

/// The `notAfter` of the leaf certificate as a `SystemTime`.
fn leaf_not_after(cert: &CertKey) -> Option<SystemTime> {
    let not_after = cert.leaf().not_after();
    let epoch = openssl::asn1::Asn1Time::from_unix(0).ok()?;
    // `ASN1_TIME_diff(from, to)` yields `to - from`, so diffing the epoch
    // against notAfter gives the (positive) seconds until expiry. The result
    // is days + seconds; clamp defensively in case a cert predates the epoch.
    let diff = epoch.diff(not_after).ok()?;
    let total = diff.days as i64 * 86_400 + diff.secs as i64;
    let secs = u64::try_from(total).unwrap_or(0);
    Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

/// The SNI callback that answers a TLS handshake from the certificate store.
///
/// When the requested hostname has no certificate, `on_miss` is invoked (the
/// ACME on-demand path) and the handshake proceeds without a certificate, so it
/// fails; the client is expected to retry once issuance completes.
///
/// For a hostname that is a named site with a `tls` directive (spec §5.7), the
/// callback also applies that site's per-SNI options on the handshake:
/// `min_version` / `max_version`, `ciphers`, and `client_auth` (mTLS). The
/// options are read from the current config snapshot, so a reload updates them
/// without rebuilding the listener (ADR-010).
pub struct SniCallback {
    store: Arc<CertStore>,
    on_miss: Arc<dyn Fn(&str) + Send + Sync>,
    config: Arc<ConfigStore>,
    /// Cache of parsed mTLS CA certificates, keyed by CA file path. mTLS sites
    /// are few, so this is bounded by the number of distinct CA files; caching
    /// the parsed certs avoids re-reading and re-parsing the CA PEM on every
    /// handshake. (The `X509Store` itself is built per handshake — it is not
    /// `Clone`.)
    ca_cache: Mutex<HashMap<String, Arc<Vec<X509>>>>,
}

impl SniCallback {
    /// Create a callback backed by `store`; `on_miss` fires for unknown SNI.
    /// `config` supplies the per-site `tls` options applied on the handshake.
    pub fn new(
        store: Arc<CertStore>,
        config: Arc<ConfigStore>,
        on_miss: Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Self {
        Self {
            store,
            on_miss,
            config,
            ca_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Apply a site's per-SNI TLS options (spec §5.7) to an in-progress
    /// handshake. Runs from within `certificate_callback`, before the version,
    /// cipher, and client-certificate decisions are finalized. Best-effort:
    /// an invalid cipher list or a missing CA file is logged, never fatal.
    fn apply_site_tls_options(&self, ssl: &mut TlsRef, tls: &TlsConfig) {
        if let Some(v) = tls.min_version {
            if let Err(e) = ssl.set_min_proto_version(Some(tls_ssl_version(v))) {
                tracing::warn!("failed to set min TLS version: {e}");
            }
        }
        if let Some(v) = tls.max_version {
            if let Err(e) = ssl.set_max_proto_version(Some(tls_ssl_version(v))) {
                tracing::warn!("failed to set max TLS version: {e}");
            }
        }
        if let Some(ciphers) = &tls.ciphers {
            if let Err(e) = ssl.set_cipher_list(ciphers) {
                tracing::warn!("invalid tls ciphers '{ciphers}': {e}");
            }
        }
        if let Some(client_auth) = &tls.client_auth {
            match self.ca_certs(&client_auth.ca_file) {
                Some(certs) => {
                    // `require` also rejects clients that present no certificate.
                    let mode = match client_auth.mode {
                        ClientAuthMode::Require => {
                            SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT
                        }
                        ClientAuthMode::Optional => SslVerifyMode::PEER,
                    };
                    ssl.set_verify(mode);
                    match self.build_ca_store(&certs) {
                        Some(store) => {
                            if let Err(e) = ssl.set_verify_cert_store(store) {
                                tracing::warn!("failed to set mTLS CA store: {e}");
                            }
                        }
                        None => tracing::warn!(
                            "tls client_auth CA file {} could not be loaded; clients are not verified",
                            client_auth.ca_file
                        ),
                    }
                }
                None => tracing::warn!(
                    "tls client_auth CA file {} unreadable; clients are not verified",
                    client_auth.ca_file
                ),
            }
        }
    }

    /// The named site's `tls` config for `host`, if any (spec §5.7).
    fn site_tls(&self, host: &str) -> Option<TlsConfig> {
        let config = self.config.load();
        config
            .sites
            .iter()
            .find(|site| matches!(&site.key, SiteKey::Named { host: h, .. } if h == host))
            .and_then(|site| site.tls.clone())
    }

    /// Parse (and cache) an mTLS CA PEM file into its certificates.
    fn ca_certs(&self, path: &str) -> Option<Arc<Vec<X509>>> {
        if let Ok(cache) = self.ca_cache.lock() {
            if let Some(certs) = cache.get(path) {
                return Some(certs.clone());
            }
        }
        let pem = std::fs::read_to_string(path).ok()?;
        let certs: Arc<Vec<X509>> = Arc::new(X509::stack_from_pem(pem.as_bytes()).ok()?);
        if let Ok(mut cache) = self.ca_cache.lock() {
            cache.insert(path.to_string(), certs.clone());
        }
        Some(certs)
    }

    /// Build a trust store from cached CA certificates.
    fn build_ca_store(&self, certs: &[X509]) -> Option<X509Store> {
        let mut builder = X509StoreBuilder::new().ok()?;
        for cert in certs {
            let _ = builder.add_cert(cert.clone());
        }
        Some(builder.build())
    }
}

/// Map a config TLS version to the openssl `SslVersion`.
fn tls_ssl_version(version: TlsVersion) -> SslVersion {
    match version {
        TlsVersion::Tls12 => SslVersion::TLS1_2,
        TlsVersion::Tls13 => SslVersion::TLS1_3,
    }
}

#[async_trait]
impl TlsAccept for SniCallback {
    async fn certificate_callback(&self, ssl: &mut TlsRef) {
        // Copy the SNI out (lowercased, as config hosts are normalized) so the
        // immutable borrow of `ssl` ends before we mutate it via `ext::ssl_use_*`.
        let Some(sni) = ssl
            .servername(NameType::HOST_NAME)
            .map(str::to_owned)
            .map(|s| s.to_ascii_lowercase())
        else {
            // No SNI; there is nothing to route on, so leave the handshake
            // without a certificate.
            return;
        };
        match self.store.get(&sni) {
            Some(cert) => {
                if let Err(e) = ext::ssl_use_certificate(ssl, cert.leaf()) {
                    tracing::warn!("failed to set certificate for {sni}: {e}");
                    return;
                }
                for intermediate in cert.intermediates() {
                    let _ = ext::ssl_add_chain_cert(ssl, intermediate);
                }
                if let Err(e) = ext::ssl_use_private_key(ssl, cert.key()) {
                    tracing::warn!("failed to set private key for {sni}: {e}");
                }
            }
            None => {
                tracing::warn!("no certificate for SNI '{sni}'; triggering on-demand issuance");
                (self.on_miss)(&sni);
            }
        }
        // Per-site TLS options (spec §5.7): min/max version, ciphers, mTLS.
        if let Some(tls) = self.site_tls(&sni) {
            self.apply_site_tls_options(ssl, &tls);
        }
    }
}

/// Parse a PEM certificate chain and a PEM private key into a [`CertKey`].
pub fn cert_key_from_pem(cert_chain_pem: &str, key_pem: &str) -> Result<CertKey, String> {
    let certs = X509::stack_from_pem(cert_chain_pem.as_bytes())
        .map_err(|e| format!("failed to parse certificate chain: {e}"))?;
    if certs.is_empty() {
        return Err("certificate chain is empty".to_string());
    }
    let key = PKey::private_key_from_pem(key_pem.as_bytes())
        .map_err(|e| format!("failed to parse private key: {e}"))?;
    Ok(CertKey::new(certs, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_store_get_store_has() {
        // Build a trivial CertKey from a generated self-signed cert.
        let (cert_pem, key_pem) = rcgen_test_cert("example.com");
        let cert = cert_key_from_pem(&cert_pem, &key_pem).unwrap();
        let store = CertStore::new();
        assert!(!store.has("example.com"));
        store.store("example.com", cert);
        assert!(store.has("example.com"));
        assert!(store.get("example.com").is_some());
        assert!(store.get("other.com").is_none());
    }

    #[test]
    fn parses_pem_roundtrip() {
        let (cert_pem, key_pem) = rcgen_test_cert("api.test");
        let cert = cert_key_from_pem(&cert_pem, &key_pem).unwrap();
        // A self-signed cert parses as a leaf with no intermediates, and the
        // leaf re-serializes to valid PEM.
        assert!(cert.intermediates().is_empty());
        let leaf_pem = String::from_utf8(cert.leaf().to_pem().unwrap()).unwrap();
        assert!(leaf_pem.starts_with("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn store_records_expiry_and_filters_due_renewal() {
        let store = CertStore::new();
        // A certificate that expired long ago (fixed date), and one valid until
        // 2035. The deadline below is derived from the stored expiries, so the
        // test never depends on the current wall clock.
        let (pem, key) = rcgen_test_cert_validity("due.test", (2025, 1, 1), (2026, 1, 1));
        store.store("due.test", cert_key_from_pem(&pem, &key).unwrap());
        let (pem, key) = rcgen_test_cert_validity("later.test", (2025, 1, 1), (2035, 1, 1));
        store.store("later.test", cert_key_from_pem(&pem, &key).unwrap());

        let due_expiry = store.expiry("due.test").expect("due.test expiry");
        let later_expiry = store.expiry("later.test").expect("later.test expiry");
        assert!(
            due_expiry < later_expiry,
            "due.test should expire before later.test"
        );

        // Due at its own expiry (and only it), not a day before.
        assert_eq!(
            store.hosts_due_renewal(due_expiry),
            vec!["due.test".to_string()]
        );
        let one_day = std::time::Duration::from_secs(24 * 3600);
        assert!(
            store.hosts_due_renewal(due_expiry - one_day).is_empty(),
            "not yet due a day before expiry"
        );
        // Far in the future both are due.
        assert_eq!(store.hosts_due_renewal(later_expiry).len(), 2);
    }

    /// Generate a self-signed certificate valid between two fixed dates
    /// (dev/test helper; production certificates come from ACME).
    fn rcgen_test_cert_validity(
        host: &str,
        not_before: (i32, u8, u8),
        not_after: (i32, u8, u8),
    ) -> (String, String) {
        let mut params = rcgen::CertificateParams::new(vec![host.to_string()])
            .expect("failed to build certificate params");
        params.not_before = rcgen::date_time_ymd(not_before.0, not_before.1, not_before.2);
        params.not_after = rcgen::date_time_ymd(not_after.0, not_after.1, not_after.2);
        let keypair = rcgen::KeyPair::generate().expect("failed to generate key");
        let cert = params.self_signed(&keypair).expect("failed to sign cert");
        (cert.pem(), keypair.serialize_pem())
    }

    /// Generate a self-signed certificate via `rcgen` (a dev/test-only helper;
    /// production certificates come from ACME).
    fn rcgen_test_cert(host: &str) -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec![host.to_string()])
            .expect("failed to generate test certificate");
        (cert.cert.pem(), cert.signing_key.serialize_pem())
    }

    #[tokio::test]
    async fn sni_serves_stored_certificate() {
        use pingora::listeners::TlsAcceptCallbacks;
        use pingora::protocols::tls::server::handshake_with_callback;
        use pingora::protocols::tls::SslStream;
        use pingora::tls::ssl::{self, SslAcceptor, SslMethod};
        use std::pin::Pin;

        // Store a self-signed cert for a host, then verify a TLS handshake with
        // that SNI serves exactly it.
        let store = Arc::new(CertStore::new());
        let (cert_pem, key_pem) = rcgen_test_cert("example.test");
        store.store(
            "example.test",
            cert_key_from_pem(&cert_pem, &key_pem).unwrap(),
        );
        let config = Arc::new(ConfigStore::new(crate::config::ast::CompiledConfig {
            global: crate::config::ast::GlobalConfig::default(),
            sites: vec![],
        }));

        let callbacks: TlsAcceptCallbacks =
            Box::new(SniCallback::new(store, config, Arc::new(|_| {})));
        let acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())
            .unwrap()
            .build();
        let (client, server) = tokio::io::duplex(8192);

        // Server handshake runs concurrently; it must stay alive (not see a
        // closed pipe) until the client stream is dropped at the end of the test.
        let server_task =
            tokio::spawn(
                async move { handshake_with_callback(&acceptor, server, &callbacks).await },
            );

        // Client connects with SNI and reads back the served leaf certificate.
        let ssl_context = ssl::SslContext::builder(SslMethod::tls()).unwrap().build();
        let mut ssl = ssl::Ssl::new(&ssl_context).unwrap();
        ssl.set_hostname("example.test").unwrap();
        ssl.set_verify(ssl::SslVerifyMode::NONE);
        let mut client_stream = SslStream::new(ssl, client).unwrap();
        Pin::new(&mut client_stream).connect().await.unwrap();
        let served_pem = String::from_utf8(
            client_stream
                .ssl()
                .peer_certificate()
                .unwrap()
                .to_pem()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(served_pem.trim(), cert_pem.trim());

        server_task
            .await
            .unwrap()
            .expect("server handshake should succeed");
    }
}
