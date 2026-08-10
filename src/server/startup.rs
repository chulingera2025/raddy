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

//! Startup sequence: parse → validate → snapshot → derive listeners → serve.
//!
//! The Raddyfile is fully parsed and validated before any listener is bound
//! (Q6): an invalid config returns an error and the process exits non-zero
//! without serving. The listener set is derived from the snapshot and fixed for
//! the process lifetime (ADR-010). Port 443 is served over TLS with SNI
//! dynamic certificates (M4); every other port is plain HTTP.
//!
//! Zero-downtime binary upgrade (M7, ADR-008) is expressed through the two
//! pingora flags threaded in here: `-u/--upgrade` (this process is the *new*
//! side: acquire the running instance's listening fds over the upgrade socket)
//! and `-t/--test` (validate config + construction, then exit 0/1 without
//! touching any listener). The upgrade socket path must agree between old and
//! new process.

use crate::config::ast::{
    AccessLogDirective, AccessLogFormat, CompiledConfig, LogLevel, SiteKey, TlsSource,
};
use crate::config::snapshot::{self, ConfigStore};
use crate::proxy::handler::ProxyHandler;
use crate::proxy::lb::{spawn_health_check_runner, LoadBalancerPool};
use crate::server::acme::{AcmeManager, ChallengeStore, ISSUANCE_QUEUE_CAPACITY};
use crate::server::issuance_queue::{EnqueueOutcome, RequestKind};
use crate::server::reload;
use crate::tls::{CertStore, SniCallback};
use pingora::listeners::{tls::TlsSettings, TlsAcceptCallbacks};
use pingora::prelude::*;
use pingora::server::configuration::{Opt, ServerConf};
use pingora::services::listening::Service;
use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Runtime options that come from the CLI and shape the server.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Directory for persisted certificates and the ACME account.
    pub cert_dir: PathBuf,
    /// ACME directory URL (Let's Encrypt production by default).
    pub acme_directory: String,
    /// Path to a PEM root CA that trusts the ACME server (required for Pebble).
    pub acme_root_pem: Option<PathBuf>,
    /// Append structured JSON access logs to this file (none if unset).
    pub access_log: Option<PathBuf>,
    /// Address for the Prometheus `/metrics` listener (none if unset).
    pub metrics_addr: Option<String>,
    /// Start as the *new* side of a zero-downtime upgrade: acquire the running
    /// instance's listening fds over the upgrade socket (ADR-008).
    pub upgrade: bool,
    /// Validate the config and construction, then exit 0/1 without binding any
    /// listener (used as the `raddy upgrade` pre-flight).
    pub test: bool,
    /// Write this process's PID here so `raddy upgrade` can find it (none =
    /// don't write; `raddy upgrade` then requires an explicit `--pidfile`).
    pub pidfile: Option<PathBuf>,
    /// Unix socket both sides use to hand over listening fds (must match).
    pub upgrade_sock: String,
}

/// Boot the proxy server and run until a shutdown signal.
///
/// Returns an error if the Raddyfile is invalid or the server cannot be
/// constructed; the caller reports it and exits non-zero.
pub fn run(config_path: &Path, opts: &RunOptions) -> Result<(), Box<dyn Error>> {
    let snapshot = snapshot::build(config_path)?;
    let ports = snapshot.listeners();
    let email = snapshot.global.acme_email.clone();
    // The access-log directive (spec §5.13); read before `snapshot` is moved.
    let global_access_log = snapshot.global.access_log.clone();
    let startup_hosts = hosts_needing_certs(&snapshot);
    // Computed before `snapshot` is moved into the store below (used later to
    // decide which listeners are TLS).
    let tls_ports = tls_listener_ports(&snapshot);

    init_tracing(default_log_filter(snapshot.global.log_level));

    // Certificate store + ACME manager (certificates are process-lifetime and
    // survive config reloads; reload swaps only the routing snapshot).
    let cert_store = Arc::new(CertStore::new());
    let challenges = Arc::new(ChallengeStore::new());
    let acme_root_pem = match &opts.acme_root_pem {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?,
        ),
        None => None,
    };
    let acme = Arc::new(AcmeManager::new(
        cert_store.clone(),
        challenges.clone(),
        opts.acme_directory.clone(),
        acme_root_pem,
        opts.cert_dir.clone(),
        email,
        snapshot.global.dns_challenge.clone(),
    ));
    acme.load_persisted_certs();
    // Sites with a static or internal `tls` source (spec §5.7) serve their own
    // certificate; load them into the store now, overriding any stale persisted
    // ACME cert for the same host.
    load_site_certificates(&cert_store, &snapshot)?;
    // The issuance state table (B3a) must be bounded by the authorized
    // configured hosts plus the queue capacity: on-miss SNI and renewal can
    // only ever name hosts configured as named sites, so a host-count-derived
    // bound cannot grow with unconfigured traffic.
    let configured_hosts = snapshot
        .sites
        .iter()
        .filter(|site| matches!(&site.key, SiteKey::Named { .. }))
        .count();
    let issuance_queue = acme.spawn_issuance_worker(configured_hosts + ISSUANCE_QUEUE_CAPACITY + 1);
    // Renewal: periodically re-issue certificates inside the renewal window.
    // The interval is overridable via RADDY_RENEW_INTERVAL_SECS (a test hook so
    // Pebble's short-lived certificates can be renewed quickly).
    acme.spawn_renewal_scheduler(issuance_queue.clone(), renew_interval());

    // Load-balancing pool (ADR-011: process-lifetime, health survives reloads)
    // plus the health-check runner thread. Warm the pool from the snapshot so
    // health checks begin immediately at startup.
    let lb_pool = Arc::new(LoadBalancerPool::new());
    lb_pool.warm(&snapshot);
    spawn_health_check_runner(lb_pool.clone());

    let config_store = Arc::new(ConfigStore::new(snapshot));
    // Access log destination (spec §5.13): the `--access-log` CLI flag wins;
    // otherwise the global `access_log` directive. `access_log off` (or neither)
    // disables it.
    let access_log = match &opts.access_log {
        Some(path) => Some(open_access_log(
            path,
            global_access_log_format(&global_access_log),
        )?),
        None => match &global_access_log {
            Some(AccessLogDirective::File { path, format }) => {
                Some(open_access_log(std::path::Path::new(path), *format)?)
            }
            _ => None,
        },
    };
    // Single-node rate limiter (M10): process-lifetime, so bucket state
    // survives reloads like the LB pool (ADR-011).
    let rate_limiter = Arc::new(crate::proxy::ratelimit::RateLimiter::new());
    let handler = ProxyHandler::new(
        config_store.clone(),
        challenges.clone(),
        access_log,
        lb_pool.clone(),
        rate_limiter,
    );

    // Proactive issuance for configured named-443 hosts that lack a cached cert.
    for host in startup_hosts {
        match issuance_queue.enqueue(&host, RequestKind::New) {
            EnqueueOutcome::Queued => tracing::info!("queued issuance for {host}"),
            EnqueueOutcome::Duplicate | EnqueueOutcome::UpgradeForced => {}
            EnqueueOutcome::InCooldown | EnqueueOutcome::QueueFull => {
                tracing::warn!("ACME issuance for {host} deferred (queue busy or cooling down)")
            }
        }
    }

    // SNI on-demand: only issue for hosts configured as named sites (ADR-003).
    let on_miss_store = config_store.clone();
    let on_miss_queue = issuance_queue.clone();
    let on_miss: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |host: &str| {
        let config = on_miss_store.load();
        if is_configured_host(&config, host) {
            match on_miss_queue.enqueue(host, RequestKind::New) {
                EnqueueOutcome::Queued => {
                    tracing::info!(
                        "on-demand TLS requested for authorized host {host}; queuing issuance"
                    )
                }
                EnqueueOutcome::Duplicate | EnqueueOutcome::UpgradeForced => {}
                EnqueueOutcome::InCooldown => tracing::warn!(
                    "on-demand TLS for {host} deferred (host is in its failure cooldown)"
                ),
                EnqueueOutcome::QueueFull => {
                    tracing::warn!("on-demand TLS for {host} deferred (ACME queue full)")
                }
            }
        } else {
            tracing::warn!("on-demand TLS refused for unauthorized host {host}");
        }
    });

    let mut server = Server::new_with_opt_and_conf(
        Some(Opt {
            upgrade: opts.upgrade,
            test: opts.test,
            daemon: false,
            nocapture: false,
            conf: None,
        }),
        ServerConf {
            upgrade_sock: opts.upgrade_sock.clone(),
            // After an upgrade the old process has already handed its listeners
            // to the replacement; the grace period only drains already-in-flight
            // requests. Pingora's default (300s) would make an upgraded process
            // linger for minutes for nothing.
            grace_period_seconds: Some(10),
            ..ServerConf::default()
        },
    );
    server.bootstrap();

    // Record our PID for `raddy upgrade` once the server is actually going to
    // serve (bootstrap exits the process in test mode, so a throwaway check
    // never clobbers the running instance's pidfile).
    if !opts.test {
        if let Some(pidfile) = &opts.pidfile {
            std::fs::write(pidfile, std::process::id().to_string())
                .map_err(|e| format!("failed to write pidfile {}: {e}", pidfile.display()))?;
        }
    }

    let mut proxy = http_proxy_service(&server.configuration, handler);
    // TLS is served on port 443 by default, and on any named site whose `tls`
    // directive opts it in (spec §5.7); every other port is plain HTTP.
    for port in ports {
        if tls_ports.contains(&port) {
            // TLS listener with SNI dynamic certificates from the store. The
            // callback is bound to this listener's port so per-(host, port)
            // certs and `tls` options stay independent (P2).
            let callbacks: TlsAcceptCallbacks = Box::new(SniCallback::new(
                cert_store.clone(),
                config_store.clone(),
                port,
                on_miss.clone(),
            ));
            // Advertise HTTP/2 over ALPN (`h2` preferred, HTTP/1.1 fallback),
            // spec §5.6.
            let mut settings = TlsSettings::with_callbacks(callbacks)?;
            settings.enable_h2();
            proxy.add_tls_with_settings(&format!("0.0.0.0:{port}"), None, settings);
            tracing::info!("listening (TLS) on 0.0.0.0:{port}");
        } else {
            proxy.add_tcp(&format!("0.0.0.0:{port}"));
            tracing::info!("listening (plain HTTP) on 0.0.0.0:{port}");
        }
    }
    server.add_service(proxy);

    // Prometheus metrics listener (M5), if enabled.
    if let Some(metrics_addr) = &opts.metrics_addr {
        let mut metrics = Service::prometheus_http_service();
        metrics.add_tcp(metrics_addr);
        server.add_service(metrics);
        tracing::info!("metrics on {metrics_addr}");
    }

    // Config hot reload runs on its own thread; `run_forever` never returns.
    reload::spawn(config_path.to_path_buf(), config_store, lb_pool);

    server.run_forever();
}

/// Hostnames that need an ACME certificate up front: named sites served over
/// TLS whose `tls` source is ACME (a static or internal source supplies its
/// own cert — spec §5.7).
fn hosts_needing_certs(config: &CompiledConfig) -> Vec<String> {
    let tls_ports = tls_listener_ports(config);
    config
        .sites
        .iter()
        .filter_map(|site| match &site.key {
            SiteKey::Named { host, port } if tls_ports.contains(port) && uses_acme(site) => {
                Some(host.clone())
            }
            _ => None,
        })
        .collect()
}

/// Whether `host` is a named site configured on this instance whose
/// certificate comes from ACME (a static/internal site never triggers
/// on-demand issuance — spec §5.7).
fn is_configured_host(config: &CompiledConfig, host: &str) -> bool {
    config.sites.iter().any(|site| {
        matches!(&site.key, SiteKey::Named { host: named, .. } if named == host) && uses_acme(site)
    })
}

/// The listener ports served over TLS: 443 plus any named-site port with a
/// `tls` directive (spec §5.7).
fn tls_listener_ports(config: &CompiledConfig) -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();
    ports.insert(443);
    for site in &config.sites {
        if site.tls.is_some() {
            if let SiteKey::Named { port, .. } = &site.key {
                ports.insert(*port);
            }
        }
    }
    ports
}

/// Whether a site's certificate comes from ACME (no `tls` source, or an
/// explicit ACME default).
fn uses_acme(site: &crate::config::ast::CompiledSite) -> bool {
    site.tls
        .as_ref()
        .is_none_or(|tls| tls.source == TlsSource::Acme)
}

/// Load the static and internal certificates for sites that opted out of ACME
/// into the certificate store, keyed by `(host, port)` (spec §5.7, P2): the
/// bare host on 443 (where ACME certs live), `host:port` elsewhere. Runs at
/// startup (after persisted ACME certs, so it wins on a stale host).
fn load_site_certificates(
    cert_store: &CertStore,
    config: &CompiledConfig,
) -> Result<(), Box<dyn Error>> {
    for site in &config.sites {
        let Some(tls) = &site.tls else { continue };
        let SiteKey::Named { host, port } = &site.key else {
            continue;
        };
        let key = if *port == 443 {
            host.clone()
        } else {
            format!("{host}:{port}")
        };
        match &tls.source {
            TlsSource::Acme => {}
            TlsSource::Internal => {
                let cert = generate_internal_cert(host)?;
                cert_store.store(&key, cert);
                tracing::info!("serving a self-signed internal certificate for {host}");
            }
            TlsSource::Static {
                cert_file,
                key_file,
            } => {
                let cert_pem = std::fs::read_to_string(cert_file)
                    .map_err(|e| format!("failed to read certificate {cert_file}: {e}"))?;
                let key_pem = std::fs::read_to_string(key_file)
                    .map_err(|e| format!("failed to read key {key_file}: {e}"))?;
                let cert = crate::tls::cert_key_from_pem(&cert_pem, &key_pem)?;
                cert_store.store(&key, cert);
                tracing::info!("serving static certificate for {host}");
            }
        }
    }
    Ok(())
}

/// Generate a self-signed certificate for `host` (the `tls internal` source).
fn generate_internal_cert(host: &str) -> Result<pingora::utils::tls::CertKey, String> {
    let cert = rcgen::generate_simple_self_signed(vec![host.to_string()])
        .map_err(|e| format!("failed to generate internal certificate for {host}: {e}"))?;
    crate::tls::cert_key_from_pem(&cert.cert.pem(), &cert.signing_key.serialize_pem())
}

/// Open the access-log file (spec §5.13), creating it if needed.
fn open_access_log(
    path: &Path,
    format: AccessLogFormat,
) -> Result<crate::proxy::handler::AccessLog, Box<dyn Error>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("failed to open access log {}: {e}", path.display()))?;
    Ok(crate::proxy::handler::AccessLog {
        file: Mutex::new(file),
        format,
    })
}

/// The access-log format to use for the `--access-log` flag: the global
/// directive's format when set, otherwise JSON (spec §5.13).
fn global_access_log_format(global: &Option<AccessLogDirective>) -> AccessLogFormat {
    match global {
        Some(AccessLogDirective::File { format, .. }) => *format,
        _ => AccessLogFormat::Json,
    }
}

/// The renewal scan interval: hourly by default, overridable via
/// `RADDY_RENEW_INTERVAL_SECS` (a test hook for Pebble's short-lived certs).
fn renew_interval() -> std::time::Duration {
    std::env::var("RADDY_RENEW_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(3600))
}

/// The tracing filter level to use when `RUST_LOG` is unset: the configured
/// global `log_level`, or `info` (the default) when the Raddyfile does not set
/// one.
fn default_log_filter(log_level: Option<LogLevel>) -> &'static str {
    match log_level {
        Some(LogLevel::Debug) => "debug",
        Some(LogLevel::Info) => "info",
        Some(LogLevel::Warn) => "warn",
        Some(LogLevel::Error) => "error",
        None => "info",
    }
}

/// Install the global tracing subscriber. `RUST_LOG` takes precedence; when it
/// is unset the given `default_level` is used. Idempotent (`try_init` returns
/// without panicking if a subscriber is already installed), so `run` could be
/// reused from a test harness without a double-initialize panic.
fn init_tracing(default_level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_log_filter_uses_configured_level() {
        assert_eq!(default_log_filter(Some(LogLevel::Debug)), "debug");
        assert_eq!(default_log_filter(Some(LogLevel::Info)), "info");
        assert_eq!(default_log_filter(Some(LogLevel::Warn)), "warn");
        assert_eq!(default_log_filter(Some(LogLevel::Error)), "error");
    }

    #[test]
    fn default_log_filter_falls_back_to_info() {
        assert_eq!(default_log_filter(None), "info");
    }
}
