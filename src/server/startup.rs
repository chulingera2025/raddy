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
//! The Raddexfile is fully parsed and validated before any listener is bound
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
    AccessLogDirective, AccessLogFormat, CompiledConfig, Layer4Listener, LogLevel, SiteKey,
    TlsSource,
};
use crate::config::snapshot::{self, ConfigStore};
use crate::layer4::tcp::{TcpAccessRecord, TcpListenerService, TransparentTcpProxy};
use crate::layer4::tls_accept::TlsAcceptor;
use crate::layer4::udp::{UdpFlowRecord, UdpProxy};
use crate::proxy::handler::ProxyHandler;
use crate::proxy::lb::{spawn_health_check_runner, LoadBalancerPool};
use crate::server::acme::{AcmeManager, ChallengeStore, ISSUANCE_QUEUE_CAPACITY};
use crate::server::issuance_queue::{EnqueueOutcome, RequestKind};
use crate::server::reload;
use crate::server::upgrade;
use crate::tls::{
    cert_store_key, configure_http_alpn, CertStore, SniCallback, TlsAlpnChallengeStore,
};
use pingora::listeners::{tls::TlsSettings, TcpSocketOptions, TlsAcceptCallbacks};
use pingora::prelude::*;
use pingora::server::configuration::{Opt, ServerConf};
use pingora::services::background::background_service;
use pingora::services::listening::Service;
use std::collections::BTreeSet;
use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Runtime options that come from the CLI and shape the server.
#[derive(Debug, Clone)]
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
    /// listener (used as the `raddex upgrade` pre-flight).
    pub test: bool,
    /// Write this process's PID here so `raddex upgrade` can find it (none =
    /// don't write; `raddex upgrade` then requires an explicit `--pidfile`).
    pub pidfile: Option<PathBuf>,
    /// Unix socket both sides use to hand over listening fds (must match).
    pub upgrade_sock: String,
    /// Worker threads per listener runtime: the HTTP service and each
    /// layer-4 TCP/UDP listener get this many.
    pub threads: usize,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            cert_dir: PathBuf::default(),
            acme_directory: String::default(),
            acme_root_pem: None,
            access_log: None,
            metrics_addr: None,
            upgrade: false,
            test: false,
            pidfile: None,
            upgrade_sock: String::default(),
            threads: 1,
        }
    }
}

/// Boot the proxy server and run until a shutdown signal.
///
/// Returns an error if the Raddexfile is invalid or the server cannot be
/// constructed; the caller reports it and exits non-zero.
pub fn run(config_path: &Path, opts: &RunOptions) -> Result<(), Box<dyn Error>> {
    // The ACME/DNS-01 HTTP clients use rustls, which 0.23 refuses to
    // auto-select a CryptoProvider for when more than one backend feature is
    // compiled in (here both aws-lc-rs and ring). Install one explicitly before
    // the issuance worker can make its first TLS connection.
    install_rustls_crypto_provider();
    let snapshot = snapshot::build(config_path)?;
    if opts.upgrade
        && snapshot.layer4.iter().any(|listener| {
            matches!(
                listener,
                Layer4Listener::Tcp(tcp) if tcp.transparent
            )
        })
    {
        return Err(
            "transparent TCP listeners cannot be inherited by Pingora upgrade; use a restart"
                .into(),
        );
    }
    let ports = listeners_to_serve(&snapshot);
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
    let tls_alpn_challenges = Arc::new(TlsAlpnChallengeStore::new());
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
        tls_alpn_challenges.clone(),
        opts.acme_directory.clone(),
        acme_root_pem,
        opts.cert_dir.clone(),
        email,
        snapshot.global.dns_challenge.clone(),
        snapshot.global.tls_alpn_challenge,
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
    // The interval is overridable via RADDEX_RENEW_INTERVAL_SECS (a test hook so
    // Pebble's short-lived certificates can be renewed quickly).
    acme.spawn_renewal_scheduler(issuance_queue.clone(), renew_interval());

    // Load-balancing pool (ADR-011: process-lifetime, health survives reloads)
    // plus the health-check runner thread. Warm the pool from the snapshot so
    // health checks begin immediately at startup.
    let lb_pool = Arc::new(LoadBalancerPool::new());
    lb_pool.warm(&snapshot);
    spawn_health_check_runner(lb_pool.clone());

    // The layer-4 listener set is captured before `snapshot` is moved into the
    // store: the L4 services are registered after the HTTP proxy service.
    let layer4 = snapshot.layer4.clone();
    let config_store = Arc::new(ConfigStore::new(snapshot));
    // Access log destination (spec §5.13): the `--access-log` CLI flag wins;
    // otherwise the global `access_log` directive. `access_log off` (or neither)
    // disables it. Shared (via `Arc`) by the HTTP handler and the layer-4
    // services.
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
    let access_log = access_log.map(Arc::new);
    // Single-node rate limiter (M10): process-lifetime, so bucket state
    // survives reloads like the LB pool (ADR-011).
    let rate_limiter = Arc::new(crate::proxy::ratelimit::RateLimiter::new());
    let handler = ProxyHandler::new(
        config_store.clone(),
        challenges.clone(),
        access_log.clone(),
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
            threads: opts.threads,
            // After an upgrade the old process has already handed its listeners
            // to the replacement; the grace period only drains already-in-flight
            // requests. Pingora's default (300s) would make an upgraded process
            // linger for minutes for nothing.
            grace_period_seconds: Some(10),
            ..ServerConf::default()
        },
    );
    server.bootstrap();

    // Record our PID for `raddex upgrade` once the server is actually going to
    // serve (bootstrap exits the process in test mode, so a throwaway check
    // never clobbers the running instance's pidfile).
    if !opts.test {
        if let Some(pidfile) = &opts.pidfile {
            std::fs::write(pidfile, std::process::id().to_string())
                .map_err(|e| format!("failed to write pidfile {}: {e}", pidfile.display()))?;
            upgrade::write_topology_state(pidfile, &config_store.load())?;
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
            let callbacks: TlsAcceptCallbacks = Box::new(SniCallback::new_with_alpn(
                cert_store.clone(),
                config_store.clone(),
                port,
                on_miss.clone(),
                tls_alpn_challenges.clone(),
            ));
            // Advertise HTTP/2 over ALPN (`h2` preferred, HTTP/1.1 fallback),
            // spec §5.6.
            let mut settings = TlsSettings::with_callbacks(callbacks)?;
            configure_http_alpn(&mut settings, tls_alpn_challenges.clone());
            proxy.add_tls_with_settings(
                &format!("[::]:{port}"),
                Some(ipv6_dual_stack_options()),
                settings,
            );
            tracing::info!("listening (TLS) on [::]:{port} (IPv4 + IPv6)");
        } else {
            proxy.add_tcp_with_settings(&format!("[::]:{port}"), ipv6_dual_stack_options());
            tracing::info!("listening (plain HTTP) on [::]:{port} (IPv4 + IPv6)");
        }
    }
    server.add_service(proxy);

    // Layer-4 raw-TCP services (L4_PROXY_PLAN P0): one `Service<TcpProxyApp>`
    // per `tcp` listener. Access records are written as JSON lines to the same
    // access-log file the HTTP handler uses (the plan keeps HTTP and layer-4
    // records distinct typed events, not the HTTP common-log format).
    let l4_access_log = access_log.clone();
    for listener in &layer4 {
        match listener {
            Layer4Listener::Tcp(tcp) => {
                let sink = l4_access_log.as_ref().map(|log| {
                    let log = log.clone();
                    Arc::new(move |record: &TcpAccessRecord| {
                        let mut guard = log.file.lock().expect("access log lock poisoned");
                        let _ = serde_json::to_writer(&mut *guard, record);
                        let _ = writeln!(&mut *guard);
                        let _ = guard.flush();
                    }) as Arc<dyn Fn(&TcpAccessRecord) + Send + Sync>
                });
                if tcp.transparent {
                    let transparent = TransparentTcpProxy::new(tcp, config_store.clone(), sink)?;
                    add_layer4_service(
                        &mut server,
                        &format!("transparent-tcp/{}", tcp.listen.display()),
                        transparent,
                        opts.threads,
                    );
                    tracing::info!("listening (transparent TCP) on {}", tcp.listen.display());
                    continue;
                }
                let tls_acceptor = match &tcp.tls {
                    None => None,
                    Some(tls) => {
                        let cert = match &tls.source {
                            TlsSource::Internal => generate_internal_cert("localhost")?,
                            TlsSource::Static {
                                cert_file,
                                key_file,
                            } => {
                                let cert_pem = std::fs::read_to_string(cert_file).map_err(|e| {
                                    format!("failed to read certificate {cert_file}: {e}")
                                })?;
                                let key_pem = std::fs::read_to_string(key_file)
                                    .map_err(|e| format!("failed to read key {key_file}: {e}"))?;
                                crate::tls::cert_key_from_pem(&cert_pem, &key_pem)?
                            }
                            TlsSource::Acme => {
                                return Err(
                                    "TCP TLS termination cannot use ACME without a site identity"
                                        .into(),
                                )
                            }
                        };
                        Some(Arc::new(TlsAcceptor::new(&cert, Some(tls))?))
                    }
                };
                let terminates_tls = tls_acceptor.is_some();
                let service = TcpListenerService::new(
                    tcp,
                    config_store.clone(),
                    sink,
                    tls_acceptor,
                    opts.upgrade,
                    opts.threads,
                    opts.upgrade_sock.clone(),
                    Some(server.watch_execution_phase()),
                )?;
                add_layer4_service(
                    &mut server,
                    &format!("tcp/{}", tcp.listen.display()),
                    service,
                    opts.threads,
                );
                if terminates_tls {
                    tracing::info!("listening (TLS-terminated TCP) on {}", tcp.listen.display());
                } else {
                    tracing::info!("listening (raw TCP) on {}", tcp.listen.display());
                }
            }
            Layer4Listener::Udp(udp) => {
                let sink = l4_access_log.as_ref().map(|log| {
                    let log = log.clone();
                    Arc::new(move |record: &UdpFlowRecord| {
                        let mut guard = log.file.lock().expect("access log lock poisoned");
                        let _ = serde_json::to_writer(&mut *guard, record);
                        let _ = writeln!(&mut *guard);
                        let _ = guard.flush();
                    }) as Arc<dyn Fn(&UdpFlowRecord) + Send + Sync>
                });
                let proxy = UdpProxy::new(
                    udp,
                    config_store.clone(),
                    sink,
                    opts.upgrade || opts.test,
                    opts.threads,
                    opts.upgrade_sock.clone(),
                    udp.listen.display(),
                    Some(server.watch_execution_phase()),
                )?;
                add_layer4_service(
                    &mut server,
                    &format!("udp/{}", udp.listen.display()),
                    proxy,
                    opts.threads,
                );
                tracing::info!("listening (UDP) on {}", udp.listen.display());
            }
        }
    }

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

/// Configure an IPv6 wildcard listener to accept IPv4-mapped connections too.
/// A single dual-stack socket keeps the IPv4/IPv6 listener pair in lockstep,
/// which also makes Pingora's listener-FD upgrade path transfer both families
/// as one endpoint.
fn ipv6_dual_stack_options() -> TcpSocketOptions {
    let mut options = TcpSocketOptions::default();
    options.ipv6_only = Some(false);
    options
}

/// Register a layer-4 listener as a Pingora background service with `threads`
/// runtime workers.
///
/// `background_service()` pins every background service to a single worker
/// thread and never consults `ServerConf::threads`, so without this the L4
/// listeners run every accept loop and relay on one core no matter what
/// `--threads` says. Measured on the L4 benchmark: the TCP connection-rate
/// scenario ran at half of Nginx's rate with the whole listener on one thread,
/// while the other worker threads sat idle.
fn add_layer4_service<S>(server: &mut Server, name: &str, service: S, threads: usize)
where
    S: pingora::services::background::BackgroundService + Send + Sync + 'static,
{
    let mut service = background_service(name, service);
    service.threads = Some(threads.max(1));
    server.add_service(service);
}

/// The listeners to serve: the configured sites' ports, plus an implicit
/// plain-HTTP :80 listener when automatic HTTPS needs HTTP-01.
///
/// raddex proves domain control with HTTP-01 on a plain-HTTP listener, so a
/// config with named sites but no explicit :80 listener would otherwise be
/// unreachable by the ACME server and issuance would hang forever (P0). DNS-01
/// deployments (`dns_challenge`) skip the implicit listener — they chose DNS
/// precisely because port 80 is unavailable. An explicit :80 catch-all already
/// covers the challenge (it is served before site selection), so it is never
/// duplicated.
pub(crate) fn listeners_to_serve(snapshot: &CompiledConfig) -> BTreeSet<u16> {
    let mut ports = snapshot.listeners();
    if snapshot.global.dns_challenge.is_none()
        && !snapshot.global.tls_alpn_challenge
        && !ports.contains(&80)
        && !hosts_needing_certs(snapshot).is_empty()
    {
        ports.insert(80);
    }
    ports
}

/// The HTTP listener topology and immutable challenge mode used by reload
/// validation. TLS source/config changes are included because static/internal
/// certificates are loaded only when the listener is constructed.
pub(crate) fn http_listener_topology_keys(snapshot: &CompiledConfig) -> BTreeSet<String> {
    let tls_ports = tls_listener_ports(snapshot);
    let challenge_mode = format!(
        "dns={} alpn={}",
        snapshot.global.dns_challenge.is_some(),
        snapshot.global.tls_alpn_challenge
    );
    let mut keys = listeners_to_serve(snapshot)
        .into_iter()
        .map(|port| {
            format!(
                "Http:{port}:tls={}:{}",
                tls_ports.contains(&port),
                challenge_mode
            )
        })
        .collect::<BTreeSet<_>>();
    keys.extend(snapshot.sites.iter().filter_map(|site| {
        site.tls
            .as_ref()
            .map(|tls| format!("SiteTls:{}:{tls:?}", site.key.describe()))
    }));
    keys
}

/// The layer-4 listener topology, including whether each TCP endpoint is
/// Pingora-owned, TLS-terminated, or custom transparent TCP.
pub(crate) fn l4_listener_topology_keys(snapshot: &CompiledConfig) -> BTreeSet<String> {
    snapshot
        .layer4
        .iter()
        .map(|listener| match listener {
            Layer4Listener::Tcp(tcp) => format!(
                "Tcp:{}:tls={:?}:transparent={}",
                tcp.listen.display(),
                tcp.tls,
                tcp.transparent
            ),
            Layer4Listener::Udp(udp) => format!("Udp:{}", udp.listen.display()),
        })
        .collect()
}

/// Certificate-store keys that need an ACME certificate up front: named sites
/// served over TLS whose `tls` source is ACME (a static or internal source
/// supplies its own cert — spec §5.7). Keys are the bare host on 443 and
/// `host:port` on any other TLS port, matching the SNI callback's lookup.
fn hosts_needing_certs(config: &CompiledConfig) -> Vec<String> {
    let tls_ports = tls_listener_ports(config);
    config
        .sites
        .iter()
        .filter_map(|site| match &site.key {
            SiteKey::Named { host, port } if tls_ports.contains(port) && uses_acme(site) => {
                Some(cert_store_key(host, *port))
            }
            _ => None,
        })
        .collect()
}

/// Whether `store_key` is a named site configured on this instance whose
/// certificate comes from ACME (a static/internal site never triggers on-demand
/// issuance — spec §5.7). The key includes the port, so a host served on two
/// TLS ports is authorized independently per port (P2).
fn is_configured_host(config: &CompiledConfig, store_key: &str) -> bool {
    config.sites.iter().any(|site| {
        matches!(
            &site.key,
            SiteKey::Named { host, port } if cert_store_key(host, *port) == store_key
        ) && uses_acme(site)
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
        let key = cert_store_key(host, *port);
        match &tls.source {
            TlsSource::Acme => {}
            TlsSource::Internal => {
                let cert = generate_internal_cert(host)?;
                // `store_supplied`: this certificate is the operator's, so the
                // renewal scheduler never re-issues it via ACME.
                cert_store.store_supplied(&key, cert);
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
                cert_store.store_supplied(&key, cert);
                tracing::info!("serving static certificate for {host}");
            }
        }
    }
    Ok(())
}

/// Generate a self-signed certificate for `host` (the `tls internal` source).
pub(crate) fn generate_internal_cert(host: &str) -> Result<pingora::utils::tls::CertKey, String> {
    let cert = rcgen::generate_simple_self_signed(vec![host.to_string()])
        .map_err(|e| format!("failed to generate internal certificate for {host}: {e}"))?;
    crate::tls::cert_key_from_pem(&cert.cert.pem(), &cert.signing_key.serialize_pem())
}

/// Open the access-log file (spec §5.13), creating it if needed. The handle is
/// `Arc`-shared by the HTTP handler and the layer-4 services.
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
        file: Arc::new(Mutex::new(file)),
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

/// Install the rustls `CryptoProvider` that the ACME/DNS-01 HTTP clients need.
///
/// rustls 0.23 panics on first TLS use when it cannot pick a single backend
/// from the crate features. Feature unification across raddex's dependencies
/// enables both `aws-lc-rs` (instant-acme's hyper-rustls) and `ring`
/// (rustls-platform-verifier), so the provider is chosen and installed
/// explicitly here — the aws-lc-rs backend, matching instant-acme. Idempotent;
/// safe to call on every `run` (including the upgrade pre-flight).
fn install_rustls_crypto_provider() {
    use rustls::crypto::CryptoProvider;
    if CryptoProvider::get_default().is_none() {
        if let Err(already) = rustls::crypto::aws_lc_rs::default_provider().install_default() {
            tracing::debug!("rustls crypto provider already installed: {already:?}");
        }
    }
}

/// The renewal scan interval: hourly by default, overridable via
/// `RADDEX_RENEW_INTERVAL_SECS` (a test hook for Pebble's short-lived certs).
fn renew_interval() -> std::time::Duration {
    std::env::var("RADDEX_RENEW_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(3600))
}

/// The tracing filter level to use when `RUST_LOG` is unset: the configured
/// global `log_level`, or `info` (the default) when the Raddexfile does not set
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

    /// Build a snapshot from an in-memory Raddexfile (temp file). `tag`
    /// distinguishes parallel tests so they never share a temp filename.
    fn build_snapshot(tag: &str, config: &str) -> crate::config::ast::CompiledConfig {
        let path = std::env::temp_dir().join(format!(
            "raddex_startup_{tag}_{}.Raddexfile",
            std::process::id()
        ));
        std::fs::write(&path, config).unwrap();
        let cfg = snapshot::build(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        cfg
    }

    #[test]
    fn named_sites_get_implicit_http01_listener() {
        // A config with only a named site must still serve an implicit
        // plain-HTTP :80 listener so the ACME server can reach the HTTP-01
        // challenge (P0.2).
        let cfg = build_snapshot(
            "named",
            "api.example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n",
        );
        let ports = listeners_to_serve(&cfg);
        assert!(
            ports.contains(&80),
            "named sites must bind :80 for HTTP-01: {ports:?}"
        );
        assert!(ports.contains(&443));
    }

    #[test]
    fn dns_challenge_skips_implicit_http01_listener() {
        // DNS-01 deployments chose DNS because port 80 is unavailable; they
        // must not be forced to bind it.
        let cfg = build_snapshot(
            "dns01",
            "{ dns_challenge cloudflare abc123 }\napi.example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n",
        );
        let ports = listeners_to_serve(&cfg);
        assert!(
            !ports.contains(&80),
            "DNS-01 must not force a :80 listener: {ports:?}"
        );
    }

    #[test]
    fn tls_alpn_challenge_skips_implicit_http01_listener() {
        let cfg = build_snapshot(
            "tls-alpn01",
            "{ tls_alpn_challenge }\napi.example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n",
        );
        let ports = listeners_to_serve(&cfg);
        assert!(
            !ports.contains(&80),
            "TLS-ALPN-01 must not force a plain HTTP listener: {ports:?}"
        );
        assert!(ports.contains(&443));
    }

    #[test]
    fn explicit_http_listener_is_not_duplicated() {
        // An explicit :80 catch-all already answers the challenge (it is served
        // before site selection), so the implicit listener must not be added.
        let cfg = build_snapshot(
            "explicit80",
            ":80 {\n    redir https://{host}{uri} permanent\n}\napi.example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n",
        );
        let ports = listeners_to_serve(&cfg);
        assert_eq!(ports.iter().filter(|p| **p == 80).count(), 1);
    }

    #[test]
    fn catch_all_only_config_gets_no_implicit_listener() {
        let cfg = build_snapshot("catchall", ":8080 {\n    reverse_proxy 127.0.0.1:9000\n}\n");
        let ports = listeners_to_serve(&cfg);
        assert!(
            !ports.contains(&80),
            "no named sites, no implicit :80: {ports:?}"
        );
        assert_eq!(ports, BTreeSet::from([8080]));
    }

    #[test]
    fn http_topology_allows_site_routing_changes_on_an_existing_listener() {
        let old = build_snapshot("topology-old", ":8080 {\n    respond 200 old\n}\n");
        let new = build_snapshot(
            "topology-new",
            ":8080 {\n    respond 200 old\n}\napi.example.com:8080 {\n    respond 200 new\n}\n",
        );
        assert_eq!(
            http_listener_topology_keys(&old),
            http_listener_topology_keys(&new),
            "adding routing on an existing plain listener must be reloadable"
        );
        let changed = build_snapshot("topology-port", ":8081 {\n    respond 200 changed\n}\n");
        assert_ne!(
            http_listener_topology_keys(&old),
            http_listener_topology_keys(&changed)
        );
    }
}
