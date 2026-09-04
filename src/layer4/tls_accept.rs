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

//! Native TLS termination for `tcp` listeners (spec §5.7).
//!
//! The L4 data path owns its own OpenSSL acceptor rather than borrowing
//! Pingora's TLS listener, so an accepted socket stays a plain
//! [`tokio::net::TcpStream`] until this module wraps it. The acceptor is built
//! **once at startup**: a `tcp` listener serves one static (or internal)
//! certificate, so there is nothing to decide per connection and no
//! per-handshake callback to run — unlike the HTTP listeners, which pick a
//! certificate by SNI and keep using [`crate::tls`].
//!
//! The handshake is remote-driven, so it is always bounded by
//! [`HANDSHAKE_TIMEOUT`]: a client that opens a connection and never completes
//! the handshake must not hold a relay slot open.

use crate::config::ast::{ClientAuthMode, TlsConfig, TlsVersion};
use openssl::ssl::{SslAcceptor, SslMethod, SslVerifyMode, SslVersion};
use openssl::x509::store::X509StoreBuilder;
use pingora::utils::tls::CertKey;
use std::pin::Pin;
use std::time::Duration;
use tokio::net::TcpStream;

/// Wall-clock bound on one TLS handshake. Matches the downstream handshake
/// deadline the Pingora listener applied before this path existed, so operator-
/// visible behaviour is unchanged.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// A TLS-terminated client connection, handed to the relay like any other
/// stream.
pub type TlsStream = tokio_openssl::SslStream<TcpStream>;

/// A prebuilt OpenSSL server context for one `tcp` listener.
pub struct TlsAcceptor {
    acceptor: SslAcceptor,
}

impl std::fmt::Debug for TlsAcceptor {
    /// `SslAcceptor` is not `Debug` and its internals are not meaningful to a
    /// caller; the type name is enough for assertion messages.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsAcceptor").finish_non_exhaustive()
    }
}

impl TlsAcceptor {
    /// Build the acceptor for one listener's certificate and TLS options.
    ///
    /// `cert` is the static or internally generated certificate chain and key;
    /// `options` carries `min_version` / `max_version` / `ciphers` /
    /// `client_auth` from the site block. Returns an error when OpenSSL rejects
    /// the certificate, the key, the cipher list, or the client-auth trust
    /// store — all of which are configuration mistakes worth failing startup
    /// for rather than failing every handshake later.
    pub fn new(cert: &CertKey, options: Option<&TlsConfig>) -> Result<Self, String> {
        let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())
            .map_err(|e| format!("build TLS acceptor: {e}"))?;
        builder
            .set_certificate(cert.leaf())
            .map_err(|e| format!("set TCP TLS certificate: {e}"))?;
        for intermediate in cert.intermediates() {
            builder
                .add_extra_chain_cert(intermediate.to_owned())
                .map_err(|e| format!("add TCP TLS chain certificate: {e}"))?;
        }
        builder
            .set_private_key(cert.key())
            .map_err(|e| format!("set TCP TLS private key: {e}"))?;
        builder
            .check_private_key()
            .map_err(|e| format!("TCP TLS certificate and key do not match: {e}"))?;

        if let Some(options) = options {
            if let Some(version) = options.min_version {
                builder
                    .set_min_proto_version(Some(ssl_version(version)))
                    .map_err(|e| format!("set TLS min_version: {e}"))?;
            }
            if let Some(version) = options.max_version {
                builder
                    .set_max_proto_version(Some(ssl_version(version)))
                    .map_err(|e| format!("set TLS max_version: {e}"))?;
            }
            if let Some(ciphers) = &options.ciphers {
                builder
                    .set_cipher_list(ciphers)
                    .map_err(|e| format!("invalid tls ciphers '{ciphers}': {e}"))?;
            }
            if let Some(client_auth) = &options.client_auth {
                // mTLS fails closed: an unreadable or empty CA file yields an
                // empty trust store, so every client certificate is rejected
                // rather than silently accepted.
                let pem = std::fs::read(&client_auth.ca_file).map_err(|e| {
                    format!("failed to read client_auth CA {}: {e}", client_auth.ca_file)
                })?;
                let certs = openssl::x509::X509::stack_from_pem(&pem)
                    .map_err(|e| format!("invalid client_auth CA {}: {e}", client_auth.ca_file))?;
                if certs.is_empty() {
                    return Err(format!(
                        "client_auth CA {} contains no certificates",
                        client_auth.ca_file
                    ));
                }
                let mut store =
                    X509StoreBuilder::new().map_err(|e| format!("build CA store: {e}"))?;
                for cert in certs {
                    store
                        .add_cert(cert)
                        .map_err(|e| format!("add client_auth CA certificate: {e}"))?;
                }
                builder
                    .set_verify_cert_store(store.build())
                    .map_err(|e| format!("set client_auth trust store: {e}"))?;
                builder.set_verify(match client_auth.mode {
                    ClientAuthMode::Require => {
                        SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT
                    }
                    ClientAuthMode::Optional => SslVerifyMode::PEER,
                });
            }
        }

        Ok(Self {
            acceptor: builder.build(),
        })
    }

    /// Complete the server handshake on `stream`, bounded by
    /// [`HANDSHAKE_TIMEOUT`].
    ///
    /// Returns the TLS-terminated stream, or an error when the handshake fails
    /// or times out. On error the stream is dropped, closing the connection.
    pub async fn accept(&self, stream: TcpStream) -> Result<TlsStream, String> {
        let ssl = openssl::ssl::Ssl::new(self.acceptor.context())
            .map_err(|e| format!("create TLS session: {e}"))?;
        let mut tls = tokio_openssl::SslStream::new(ssl, stream)
            .map_err(|e| format!("wrap TLS stream: {e}"))?;
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, Pin::new(&mut tls).accept()).await {
            Ok(Ok(())) => Ok(tls),
            Ok(Err(e)) => Err(format!("TLS handshake failed: {e}")),
            Err(_) => Err("TLS handshake timed out".to_string()),
        }
    }
}

/// Map a configured TLS version to its OpenSSL constant.
fn ssl_version(version: TlsVersion) -> SslVersion {
    match version {
        TlsVersion::Tls12 => SslVersion::TLS1_2,
        TlsVersion::Tls13 => SslVersion::TLS1_3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn internal_cert() -> CertKey {
        crate::server::startup::generate_internal_cert("localhost").expect("internal cert")
    }

    #[test]
    fn builds_an_acceptor_for_an_internal_certificate() {
        assert!(TlsAcceptor::new(&internal_cert(), None).is_ok());
    }

    #[test]
    fn applies_min_and_max_version() {
        let options = TlsConfig {
            min_version: Some(TlsVersion::Tls13),
            ..Default::default()
        };
        assert!(TlsAcceptor::new(&internal_cert(), Some(&options)).is_ok());
    }

    #[test]
    fn rejects_an_invalid_cipher_list_at_build_time() {
        // A typo in `ciphers` must fail startup, not every handshake.
        let options = TlsConfig {
            ciphers: Some("NOT-A-REAL-CIPHER".to_string()),
            ..Default::default()
        };
        let error = TlsAcceptor::new(&internal_cert(), Some(&options))
            .expect_err("an invalid cipher list must be rejected");
        assert!(error.contains("ciphers"), "got: {error}");
    }

    #[test]
    fn client_auth_fails_closed_on_a_missing_ca_file() {
        let options = TlsConfig {
            client_auth: Some(crate::config::ast::ClientAuth {
                mode: ClientAuthMode::Require,
                ca_file: "/definitely/missing/ca.pem".to_string(),
            }),
            ..Default::default()
        };
        let error = TlsAcceptor::new(&internal_cert(), Some(&options))
            .expect_err("a missing CA file must be rejected");
        assert!(error.contains("client_auth CA"), "got: {error}");
    }
}
