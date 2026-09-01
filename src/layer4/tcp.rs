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

//! Raw TCP proxy runtime (L4_PROXY_PLAN, P0).
//!
//! One [`TcpProxyApp`] is bound to one `tcp <address> { ... }` listener and
//! registered as a Pingora `Service<ServerApp>`. `process_new` performs
//! admission (per-listener semaphore) and upstream selection, then spawns a
//! relay task that owns the connection — so the worker is free to accept the
//! next connection. The relay connects under a hard wall-clock bound, then
//! copies bytes bidirectionally with a *true* inactivity timeout (reset by
//! traffic in either direction) and half-close propagation, and records typed
//! metrics and an access record on close.

use crate::config::ast::{
    wildcard_match_specificity, L4Upstream, Layer4Listener, ListenAddress, TcpHealthCheckSpec,
    TcpProxyConfig,
};
use crate::config::snapshot::ConfigStore;
use crate::layer4::tls;
use async_trait::async_trait;
use pingora::apps::ServerApp;
use pingora::lb::health_check;
use pingora::lb::selection::{Consistent, Random, RoundRobin};
use pingora::lb::LoadBalancer;
use pingora::protocols::l4::stream::Stream as L4Stream;
use pingora::protocols::{SocketDigest, Stream};
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::Semaphore;

/// Read chunk used by the relay pump. Bounded, so per-connection memory is two
/// buffers plus codec state, independent of transfer size.
const RELAY_CHUNK: usize = 16 * 1024;

/// How often hostname upstreams are re-resolved (L4 plan: refresh, keep
/// last-known-good on a transient failure). Overridable via
/// `RADDEX_DNS_REFRESH_SECS` (a test hook). TTL-aware scheduling is a follow-up;
/// this fixed period is a safe, simple refresh.
const DNS_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Prometheus metrics for one raw-TCP listener, distinguished by the `listener`
/// label. The counter vectors are process-global (registered once via
/// [`LazyLock`]); each [`TcpMetrics`] holds its listener's labelled handles.
#[derive(Debug)]
pub struct TcpMetrics {
    /// Connections accepted by the listener.
    pub accepted: prometheus::IntCounter,
    /// Connections rejected because the admission limit was reached.
    pub rejected: prometheus::IntCounter,
    /// Connections closed because no upstream was available.
    pub no_upstream: prometheus::IntCounter,
    /// Upstream connect failures (including timeouts).
    pub connect_failures: prometheus::IntCounter,
    /// Bytes relayed client -> upstream.
    pub client_to_upstream_bytes: prometheus::Counter,
    /// Bytes relayed upstream -> client.
    pub upstream_to_client_bytes: prometheus::Counter,
    /// Connections ended by the idle timeout.
    pub idle_timeouts: prometheus::IntCounter,
    /// Connections cancelled because the server shut down.
    pub shutdown_cancellations: prometheus::IntCounter,
    /// Background DNS refreshes that failed (last-known-good kept serving).
    pub dns_refresh_failures: prometheus::IntCounter,
}

use std::sync::LazyLock;

static ACCEPTED: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_tcp_accepted_total",
        "TCP connections accepted",
        &["listener"]
    )
    .expect("register counter vec")
});
static REJECTED: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_tcp_rejected_total",
        "TCP connections rejected by the admission limit",
        &["listener"]
    )
    .expect("register counter vec")
});
static NO_UPSTREAM: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_tcp_no_upstream_total",
        "TCP connections closed because no upstream was available",
        &["listener"]
    )
    .expect("register counter vec")
});
static CONNECT_FAILURES: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_tcp_connect_failures_total",
        "Upstream TCP connect failures",
        &["listener"]
    )
    .expect("register counter vec")
});
static C2U_BYTES: LazyLock<prometheus::CounterVec> = LazyLock::new(|| {
    prometheus::register_counter_vec!(
        "raddex_l4_tcp_client_to_upstream_bytes_total",
        "Bytes relayed client to upstream",
        &["listener"]
    )
    .expect("register counter vec")
});
static U2C_BYTES: LazyLock<prometheus::CounterVec> = LazyLock::new(|| {
    prometheus::register_counter_vec!(
        "raddex_l4_tcp_upstream_to_client_bytes_total",
        "Bytes relayed upstream to client",
        &["listener"]
    )
    .expect("register counter vec")
});
static IDLE_TIMEOUTS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_tcp_idle_timeouts_total",
        "Connections closed by the inactivity timeout",
        &["listener"]
    )
    .expect("register counter vec")
});
static SHUTDOWN_CANCELLATIONS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_tcp_shutdown_cancellations_total",
        "Connections cancelled during shutdown",
        &["listener"]
    )
    .expect("register counter vec")
});
static DNS_REFRESH_FAILURES: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_tcp_dns_refresh_failures_total",
        "Background DNS refreshes that failed (last-known-good kept serving)",
        &["listener"]
    )
    .expect("register counter vec")
});

impl TcpMetrics {
    /// Build a metric set bound to `listener` (the `tcp/<address>` form).
    fn register(listener: &str) -> Arc<Self> {
        let label = [listener];
        Arc::new(Self {
            accepted: ACCEPTED.with_label_values(&label),
            rejected: REJECTED.with_label_values(&label),
            no_upstream: NO_UPSTREAM.with_label_values(&label),
            connect_failures: CONNECT_FAILURES.with_label_values(&label),
            client_to_upstream_bytes: C2U_BYTES.with_label_values(&label),
            upstream_to_client_bytes: U2C_BYTES.with_label_values(&label),
            idle_timeouts: IDLE_TIMEOUTS.with_label_values(&label),
            shutdown_cancellations: SHUTDOWN_CANCELLATIONS.with_label_values(&label),
            dns_refresh_failures: DNS_REFRESH_FAILURES.with_label_values(&label),
        })
    }
}

/// A structured access record for one closed TCP connection.
#[derive(Debug, serde::Serialize)]
pub struct TcpAccessRecord {
    /// Epoch milliseconds when the connection was accepted.
    pub ts_ms: u64,
    /// The listener identity (`tcp/<address>`).
    pub listener: String,
    /// The client socket address.
    pub client: SocketAddr,
    /// The selected upstream socket address.
    pub upstream: SocketAddr,
    /// Connection duration (accept to close).
    pub duration_ms: u64,
    /// Bytes relayed client -> upstream.
    pub client_to_upstream_bytes: u64,
    /// Bytes relayed upstream -> client.
    pub upstream_to_client_bytes: u64,
    /// How the connection ended.
    pub outcome: &'static str,
}

/// A sink for typed TCP access records (the access-log file, when configured).
type AccessLogSink = dyn Fn(&TcpAccessRecord) + Send + Sync;

/// The raw-TCP proxy application for one listener.
///
/// Reload-aware (L4 plan §reload semantics): the app reads its own config from
/// the live [`ConfigStore`] snapshot on every new connection. When the
/// selection-relevant spec (upstreams, policy, timeouts, limits, health check)
/// changed, the balancer and admission are rebuilt for *new* connections;
/// existing connections keep their selected upstream and relay policy.
pub struct TcpProxyApp {
    /// The listener identity (for metrics and records).
    listener: String,
    /// The bound address — how this app finds its config in the snapshot.
    listen: ListenAddress,
    config_store: Arc<ConfigStore>,
    /// The current runtime (balancer + admission + timeouts), rebuilt on reload
    /// when the spec changes. Shared with the health-check thread.
    runtime: Arc<Mutex<AppRuntime>>,
    metrics: Arc<TcpMetrics>,
    /// Optional sink for typed access records.
    access_log: Option<Arc<AccessLogSink>>,
}

/// The selection-relevant portion of a `tcp` listener — the reload
/// change-detection key. Changing any of these rebuilds the runtime for new
/// connections (a `header_up`-style non-selection change does not).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TcpSelectionSpec {
    transparent: bool,
    upstreams: Vec<L4Upstream>,
    lb_policy: crate::config::ast::LbPolicy,
    connect_timeout: Duration,
    idle_timeout: Duration,
    max_connections: usize,
    health_check: Option<TcpHealthCheckSpec>,
    /// SNI-routing mode (L4 P1): exact-SNI -> upstream, plus a fallback.
    sni_routes: Vec<crate::config::ast::SniRoute>,
    sni_fallback: Option<L4Upstream>,
}

impl TcpSelectionSpec {
    fn from(tcp: &TcpProxyConfig) -> Self {
        Self {
            transparent: tcp.transparent,
            upstreams: tcp.upstreams.clone(),
            lb_policy: tcp.lb_policy,
            connect_timeout: tcp.connect_timeout,
            idle_timeout: tcp.idle_timeout,
            max_connections: tcp.max_connections,
            health_check: tcp.health_check,
            sni_routes: tcp.sni_routes.clone(),
            sni_fallback: tcp.sni_fallback.clone(),
        }
    }
}

/// One build of the runtime: the route selector, admission gate, and timeouts.
/// A reload replaces this wholesale when the spec changes; the DNS refresher
/// replaces it when the resolved addresses change.
struct AppRuntime {
    spec: TcpSelectionSpec,
    /// The resolved upstream addresses (for the DNS refresher's change check).
    resolved: Vec<SocketAddr>,
    selector: Arc<L4Selector>,
    connect_timeout: Duration,
    idle_timeout: Duration,
    admission: Arc<Semaphore>,
}

/// How a raw-TCP connection's upstream is chosen.
enum L4Selector {
    /// `to` mode: a health-checked balancer over the shared upstream set.
    Balancer(Arc<L4Balancer>),
    /// SNI-routing mode (L4 P1): exact or one-label wildcard SNI -> its own
    /// upstream, plus a fallback for unknown/absent/malformed SNI.
    Sni {
        routes: Arc<HashMap<String, SocketAddr>>,
        fallback: Option<SocketAddr>,
    },
}

impl AppRuntime {
    /// Resolve the spec's upstreams and build a fresh runtime (either mode).
    fn build(tcp: &TcpProxyConfig, listener: &ListenAddress) -> Result<Self, String> {
        let (selector, resolved) = if !tcp.sni_routes.is_empty() {
            let (routes, fallback) = resolve_sni_routes(tcp, listener)?;
            let resolved = routes.values().chain(fallback.iter()).copied().collect();
            (
                L4Selector::Sni {
                    routes: Arc::new(routes),
                    fallback,
                },
                resolved,
            )
        } else {
            let resolved = resolve_upstreams(tcp, listener)?;
            let balancer = Arc::new(L4Balancer::build(tcp, &resolved)?);
            (L4Selector::Balancer(balancer), resolved)
        };
        Ok(Self {
            spec: TcpSelectionSpec::from(tcp),
            resolved,
            selector: Arc::new(selector),
            connect_timeout: tcp.connect_timeout,
            idle_timeout: tcp.idle_timeout,
            admission: Arc::new(Semaphore::new(tcp.max_connections)),
        })
    }
}

/// Resolve a `tcp` listener's configured upstreams. A literal IP parses
/// directly; a hostname resolves to all its A/AAAA addresses.
fn resolve_upstreams(
    tcp: &TcpProxyConfig,
    listener: &ListenAddress,
) -> Result<Vec<SocketAddr>, String> {
    let mut upstreams = Vec::with_capacity(tcp.upstreams.len());
    for upstream in &tcp.upstreams {
        let resolved = resolve_upstream(&upstream.host, upstream.port).map_err(|e| {
            format!(
                "tcp listener {}: upstream {}: {e}",
                listener.display(),
                upstream.display()
            )
        })?;
        if resolved.is_empty() {
            return Err(format!(
                "tcp listener {}: upstream {} resolved to no addresses",
                listener.display(),
                upstream.display()
            ));
        }
        upstreams.extend(resolved);
    }
    Ok(upstreams)
}

/// Resolve an SNI-routing listener's upstreams: each route's single upstream
/// (its first resolved address) plus the fallback.
fn resolve_sni_routes(
    tcp: &TcpProxyConfig,
    listener: &ListenAddress,
) -> Result<(HashMap<String, SocketAddr>, Option<SocketAddr>), String> {
    let mut routes = HashMap::with_capacity(tcp.sni_routes.len());
    for route in &tcp.sni_routes {
        let addrs = resolve_upstream(&route.upstream.host, route.upstream.port).map_err(|e| {
            format!(
                "tcp listener {}: sni route '{}' upstream {}: {e}",
                listener.display(),
                route.name,
                route.upstream.display()
            )
        })?;
        let Some(addr) = addrs.first() else {
            return Err(format!(
                "tcp listener {}: sni route '{}' upstream {} resolved to no addresses",
                listener.display(),
                route.name,
                route.upstream.display()
            ));
        };
        routes.insert(route.name.clone(), *addr);
    }
    let fallback = match &tcp.sni_fallback {
        Some(fb) => {
            let addrs = resolve_upstream(&fb.host, fb.port).map_err(|e| {
                format!(
                    "tcp listener {}: sni fallback upstream {}: {e}",
                    listener.display(),
                    fb.display()
                )
            })?;
            Some(*addrs.first().ok_or_else(|| {
                format!(
                    "tcp listener {}: sni fallback upstream {} resolved to no addresses",
                    listener.display(),
                    fb.display()
                )
            })?)
        }
        None => None,
    };
    Ok((routes, fallback))
}

/// The flat set of resolved upstream addresses for a listener, in either mode —
/// the DNS refresher's change-detection key.
fn resolved_set(tcp: &TcpProxyConfig, listener: &ListenAddress) -> Result<Vec<SocketAddr>, String> {
    if !tcp.sni_routes.is_empty() {
        let (routes, fallback) = resolve_sni_routes(tcp, listener)?;
        Ok(routes.values().chain(fallback.iter()).copied().collect())
    } else {
        resolve_upstreams(tcp, listener)
    }
}

/// The live pieces a new connection needs, cloned out of the runtime.
struct RuntimeHandle {
    selector: Arc<L4Selector>,
    admission: Arc<Semaphore>,
    connect_timeout: Duration,
    idle_timeout: Duration,
    /// The load-balancing policy (for building the `ip_hash` selection key in
    /// `to` mode; unused in SNI mode).
    policy: crate::config::ast::LbPolicy,
    transparent: bool,
}

/// A type-erased, health-checked backend selector for one L4 listener.
enum L4Balancer {
    RoundRobin(LoadBalancer<RoundRobin>),
    Random(LoadBalancer<Random>),
    Consistent(LoadBalancer<Consistent>),
}

impl L4Balancer {
    /// Build the selector for the resolved `upstreams`, attaching the active
    /// TCP-connect health check when configured.
    fn build(tcp: &TcpProxyConfig, upstreams: &[SocketAddr]) -> Result<Self, String> {
        let addrs: Vec<String> = upstreams.iter().map(|a| a.to_string()).collect();
        let mut lb = match tcp.lb_policy {
            crate::config::ast::LbPolicy::RoundRobin => L4Balancer::RoundRobin(
                LoadBalancer::<RoundRobin>::try_from_iter(addrs)
                    .map_err(|_| "failed to build round-robin balancer".to_string())?,
            ),
            crate::config::ast::LbPolicy::Random => L4Balancer::Random(
                LoadBalancer::<Random>::try_from_iter(addrs)
                    .map_err(|_| "failed to build random balancer".to_string())?,
            ),
            crate::config::ast::LbPolicy::IpHash => L4Balancer::Consistent(
                LoadBalancer::<Consistent>::try_from_iter(addrs)
                    .map_err(|_| "failed to build ip_hash balancer".to_string())?,
            ),
        };
        if let Some(hc) = &tcp.health_check {
            let mut check = health_check::TcpHealthCheck::new();
            check.consecutive_failure = hc.consecutive_failures;
            check.consecutive_success = hc.consecutive_successes;
            check.peer_template.options.connection_timeout = Some(hc.timeout);
            match &mut lb {
                L4Balancer::RoundRobin(lb) => lb.set_health_check(check),
                L4Balancer::Random(lb) => lb.set_health_check(check),
                L4Balancer::Consistent(lb) => lb.set_health_check(check),
            }
        }
        Ok(lb)
    }

    /// Select a healthy upstream for `key` (the client IP bytes for `ip_hash`;
    /// ignored by the other policies). `None` when every upstream is unhealthy.
    fn select(&self, key: &[u8]) -> Option<SocketAddr> {
        let backend = match self {
            L4Balancer::RoundRobin(lb) => lb.select(key, 256),
            L4Balancer::Random(lb) => lb.select(key, 256),
            L4Balancer::Consistent(lb) => lb.select(key, 256),
        };
        backend?.addr.as_inet().cloned()
    }

    /// Run one round of active health checks.
    async fn probe(&self) {
        match self {
            L4Balancer::RoundRobin(lb) => lb.backends().run_health_check(true).await,
            L4Balancer::Random(lb) => lb.backends().run_health_check(true).await,
            L4Balancer::Consistent(lb) => lb.backends().run_health_check(true).await,
        }
    }
}

impl TcpProxyApp {
    /// Create an app for one compiled `tcp` listener against the live config
    /// store (so a reload updates the upstream set for new connections).
    /// Upstream hostnames are resolved at build time; an unresolvable host is a
    /// startup error. A configured `health_check` spawns a per-listener probe
    /// thread (its own runtime) that always probes the *current* balancer.
    pub fn new(
        tcp: &TcpProxyConfig,
        config_store: Arc<ConfigStore>,
        access_log: Option<Arc<AccessLogSink>>,
    ) -> Result<Self, String> {
        let listen = tcp.listen.clone();
        let listener = format!("tcp/{}", tcp.listen.display());
        let runtime = Arc::new(Mutex::new(AppRuntime::build(tcp, &listen)?));
        let metrics = TcpMetrics::register(&listener);
        // Active health checks only apply to `to` mode; SNI routes are exact
        // single-upstream mappings (per-route health is a P1 follow-up).
        if let Some(hc) = &tcp.health_check {
            if tcp.sni_routes.is_empty() {
                let interval = hc.interval;
                let runtime = runtime.clone();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to build L4 health-check runtime");
                    rt.block_on(async move {
                        loop {
                            tokio::time::sleep(interval).await;
                            // Probe the current balancer: a reload replaces it.
                            let selector = runtime
                                .lock()
                                .expect("L4 runtime lock poisoned")
                                .selector
                                .clone();
                            if let L4Selector::Balancer(balancer) = &*selector {
                                balancer.probe().await;
                            }
                        }
                    });
                });
            }
        }
        // DNS refresh: only hostname upstreams can change; IP literals are
        // static. A transient refresh failure keeps last-known-good.
        if has_hostname_upstreams(tcp) {
            let config_store = config_store.clone();
            let listen = listen.clone();
            let runtime = runtime.clone();
            let metrics = metrics.clone();
            let listener_c = listener.clone();
            let interval = dns_refresh_interval();
            std::thread::spawn(move || loop {
                std::thread::sleep(interval);
                refresh_dns(&config_store, &listen, &runtime, &metrics, &listener_c);
            });
        }
        Ok(Self {
            listener,
            listen,
            config_store,
            runtime,
            metrics,
            access_log,
        })
    }

    /// The runtime to use for a new connection, rebuilt on reload when this
    /// listener's selection-relevant spec changed. If the listener is missing
    /// from the snapshot (a reload that should have been rejected), the
    /// last-known runtime keeps serving.
    fn current_handle(&self) -> RuntimeHandle {
        let config = self.config_store.load();
        let new_spec = config.layer4.iter().find_map(|listener| match listener {
            Layer4Listener::Tcp(tcp) if tcp.listen == self.listen => {
                Some(TcpSelectionSpec::from(tcp))
            }
            _ => None,
        });
        let mut guard = self.runtime.lock().expect("L4 runtime lock poisoned");
        if let Some(new_spec) = new_spec {
            if guard.spec != new_spec {
                if let Some(tcp) = config.layer4.iter().find_map(|listener| match listener {
                    Layer4Listener::Tcp(tcp) if tcp.listen == self.listen => Some(tcp),
                    _ => None,
                }) {
                    match AppRuntime::build(tcp, &self.listen) {
                        Ok(runtime) => {
                            tracing::info!("tcp {}: upstream set changed on reload", self.listener);
                            *guard = runtime;
                        }
                        Err(e) => tracing::error!(
                            "tcp {}: reload upstream rebuild failed, keeping previous: {e}",
                            self.listener
                        ),
                    }
                }
            }
        }
        RuntimeHandle {
            selector: guard.selector.clone(),
            admission: guard.admission.clone(),
            connect_timeout: guard.connect_timeout,
            idle_timeout: guard.idle_timeout,
            policy: guard.spec.lb_policy,
            transparent: guard.spec.transparent,
        }
    }
}

/// A Linux transparent-proxy acceptor. Pingora's ServerApp and relay logic are
/// reused after this small listener shim attaches the socket digest that carries
/// the original destination.
pub struct TransparentTcpProxy {
    listener: Arc<TcpListener>,
    app: Arc<TcpProxyApp>,
}

impl TransparentTcpProxy {
    /// Bind a transparent TCP listener and build its Pingora-backed proxy app.
    ///
    /// The tcp parameter supplies the listener and routing configuration;
    /// config_store supplies reloadable upstream state; access_log is the
    /// optional typed access-log sink. Returns the service on success, or an
    /// error when the Linux transparent socket or proxy app cannot be built.
    pub fn new(
        tcp: &TcpProxyConfig,
        config_store: Arc<ConfigStore>,
        access_log: Option<Arc<AccessLogSink>>,
    ) -> Result<Self, String> {
        let ListenAddress::Socket(address) = &tcp.listen;
        let listener = bind_transparent_listener(*address)?;
        Ok(Self {
            listener: Arc::new(listener),
            app: Arc::new(TcpProxyApp::new(tcp, config_store, access_log)?),
        })
    }
}

#[async_trait]
impl BackgroundService for TransparentTcpProxy {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        loop {
            let accepted = tokio::select! {
                result = self.listener.accept() => result,
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
            };
            let (stream, peer) = match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    tracing::warn!("transparent TCP accept failed: {error}");
                    continue;
                }
            };
            let raw_fd = stream.as_raw_fd();
            let mut session: Stream = Box::new(L4Stream::from(stream));
            session.set_socket_digest(SocketDigest::from_raw_fd(raw_fd));
            tracing::debug!("transparent TCP connection from {peer}");
            let app = self.app.clone();
            let child_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let _ = app.process_new(session, &child_shutdown).await;
            });
        }
    }
}

/// Bind a TCP socket with the Linux transparent-proxy option before bind.
fn bind_transparent_listener(address: SocketAddr) -> Result<TcpListener, String> {
    let domain = if address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))
        .map_err(|e| format!("create transparent TCP socket: {e}"))?;
    socket
        .set_reuse_address(true)
        .map_err(|e| format!("set transparent TCP reuseaddr: {e}"))?;
    set_transparent_fd(socket.as_raw_fd(), address.is_ipv4())?;
    socket
        .bind(&SockAddr::from(address))
        .map_err(|e| format!("bind transparent TCP listener {address}: {e}"))?;
    socket
        .listen(65_535)
        .map_err(|e| format!("listen on transparent TCP {address}: {e}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("set transparent TCP nonblocking: {e}"))?;
    let listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(listener)
        .map_err(|e| format!("adopt transparent TCP listener {address}: {e}"))
}

/// Set IP_TRANSPARENT/IPV6_TRANSPARENT on a socket.
fn set_transparent_fd(fd: std::os::fd::RawFd, ipv4: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let value: libc::c_int = 1;
        let (level, option) = if ipv4 {
            (libc::IPPROTO_IP, libc::IP_TRANSPARENT)
        } else {
            (libc::IPPROTO_IPV6, libc::IPV6_TRANSPARENT)
        };
        // SAFETY: fd is an open socket owned by the caller and value points to
        // a valid c_int for the duration of setsockopt.
        let result = unsafe {
            libc::setsockopt(
                fd,
                level,
                option,
                (&value as *const libc::c_int).cast(),
                std::mem::size_of_val(&value) as libc::socklen_t,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(format!(
                "set transparent socket option: {}",
                io::Error::last_os_error()
            ))
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (fd, ipv4);
        Err("transparent proxying is supported only on Linux".to_string())
    }
}

/// Read the destination selected by a Linux REDIRECT/TPROXY rule.
fn original_dst(stream: &Stream) -> Option<SocketAddr> {
    stream
        .get_socket_digest()?
        .original_dst()?
        .as_inet()
        .copied()
}

#[async_trait]
impl ServerApp for TcpProxyApp {
    async fn process_new(
        self: &Arc<Self>,
        mut session: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        tracing::debug!("tcp {}: new connection", self.listener);
        // Load the current runtime (rebuilt on reload when the spec changed);
        // new connections always use the latest upstream set and limits.
        let handle = self.current_handle();
        let client_addr = peer_addr(&mut session);
        let transparent = handle.transparent;
        let original_destination = transparent.then(|| original_dst(&session)).flatten();
        // Admission first: reject before spending a worker on connect/relay
        // when the listener is at capacity.
        let permit = match handle.admission.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.metrics.rejected.inc();
                tracing::debug!("tcp {}: admission limit reached; rejecting", self.listener);
                return None;
            }
        };
        let key = match handle.policy {
            crate::config::ast::LbPolicy::IpHash => client_addr
                .map(|a| a.ip().to_string().into_bytes())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        // `to` mode selects up front so an empty (all-unhealthy) backend set
        // refuses before spawning a worker; SNI mode selects inside the relay
        // after inspecting the ClientHello.
        let pre_selected = match original_destination {
            Some(addr) => Some(addr),
            None => match &*handle.selector {
                L4Selector::Balancer(balancer) => match balancer.select(&key) {
                    Some(addr) => Some(addr),
                    None => {
                        self.metrics.no_upstream.inc();
                        return None;
                    }
                },
                L4Selector::Sni { .. } => None,
            },
        };
        self.metrics.accepted.inc();

        // The relay owns the connection: spawn it so the worker accepts the
        // next connection immediately. `None` tells pingora the session is not
        // reusable (every TCP connection is terminal).
        let metrics = self.metrics.clone();
        let connect_timeout = handle.connect_timeout;
        let idle_timeout = handle.idle_timeout;
        let listener = self.listener.clone();
        let access_log = self.access_log.clone();
        let shutdown = shutdown.clone();
        let selector = handle.selector.clone();
        let ts_ms = epoch_ms();
        tokio::spawn(async move {
            let started = Instant::now();
            let outcome = match &*selector {
                L4Selector::Balancer(_) => {
                    let upstream = pre_selected.expect("balancer mode pre-selects");
                    relay_tcp(
                        session,
                        upstream,
                        transparent.then_some(client_addr).flatten(),
                        connect_timeout,
                        idle_timeout,
                        &shutdown,
                        &metrics,
                    )
                    .await
                }
                L4Selector::Sni { routes, fallback } => {
                    relay_tcp_sni(
                        session,
                        routes.clone(),
                        *fallback,
                        connect_timeout,
                        idle_timeout,
                        &shutdown,
                        &metrics,
                    )
                    .await
                }
            };
            drop(permit);
            if let Some(log) = &access_log {
                let duration_ms = started.elapsed().as_millis() as u64;
                log(&TcpAccessRecord {
                    ts_ms,
                    listener,
                    client: client_addr.unwrap_or(outcome.upstream), // fall back defensively
                    upstream: outcome.upstream,
                    duration_ms,
                    client_to_upstream_bytes: outcome.c2u,
                    upstream_to_client_bytes: outcome.u2c,
                    outcome: outcome.reason,
                });
            }
        });
        None
    }
}

/// How a relayed TCP connection ended, with its byte counts and the upstream
/// that served it (for the access record).
struct RelayOutcome {
    c2u: u64,
    u2c: u64,
    upstream: SocketAddr,
    reason: &'static str,
}

/// Connect to the upstream under a hard wall-clock bound, then relay.
async fn connect_upstream(
    upstream: SocketAddr,
    source: Option<SocketAddr>,
) -> io::Result<TcpStream> {
    let Some(source) = source else {
        return TcpStream::connect(upstream).await;
    };
    if source.is_ipv4() != upstream.is_ipv4() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "transparent source and upstream address families differ",
        ));
    }
    let socket = if source.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    set_transparent_fd(socket.as_raw_fd(), source.is_ipv4()).map_err(io::Error::other)?;
    socket.bind(source)?;
    socket.connect(upstream).await
}

/// Connect to the upstream under a hard wall-clock bound, then relay.
async fn relay_tcp(
    client: Stream,
    upstream_addr: SocketAddr,
    source_addr: Option<SocketAddr>,
    connect_timeout: Duration,
    idle_timeout: Duration,
    shutdown: &ShutdownWatch,
    metrics: &TcpMetrics,
) -> RelayOutcome {
    let upstream = match tokio::time::timeout(
        connect_timeout,
        connect_upstream(upstream_addr, source_addr),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            metrics.connect_failures.inc();
            tracing::warn!("tcp connect to {upstream_addr} failed: {e}");
            return RelayOutcome {
                c2u: 0,
                u2c: 0,
                upstream: upstream_addr,
                reason: "connect_failed",
            };
        }
        Err(_) => {
            metrics.connect_failures.inc();
            tracing::warn!("tcp connect to {upstream_addr} timed out");
            return RelayOutcome {
                c2u: 0,
                u2c: 0,
                upstream: upstream_addr,
                reason: "connect_timeout",
            };
        }
    };
    tracing::debug!("tcp relay established to {upstream_addr}");
    let (c2u, u2c, end) = bidirectional(client, upstream, idle_timeout, shutdown).await;
    metrics.client_to_upstream_bytes.inc_by(c2u as f64);
    metrics.upstream_to_client_bytes.inc_by(u2c as f64);
    match end {
        EndReason::Idle => metrics.idle_timeouts.inc(),
        EndReason::Shutdown => metrics.shutdown_cancellations.inc(),
        EndReason::Closed => {}
    }
    RelayOutcome {
        c2u,
        u2c,
        upstream: upstream_addr,
        reason: end.as_str(),
    }
}

/// Select an SNI route with exact-match precedence and longest-suffix wildcard
/// precedence. Wildcards are intentionally limited to one DNS label.
fn select_sni_route(routes: &HashMap<String, SocketAddr>, name: &str) -> Option<SocketAddr> {
    if let Some(addr) = routes.get(name) {
        return Some(*addr);
    }
    routes
        .iter()
        .filter_map(|(pattern, addr)| {
            wildcard_match_specificity(pattern, name).map(|specificity| (specificity, *addr))
        })
        .max_by_key(|(suffix_len, _)| *suffix_len)
        .map(|(_, addr)| addr)
}

/// SNI-routed relay (L4 P1): inspect a bounded ClientHello prefix, select the
/// upstream by exact SNI (or the fallback), connect, forward the *exact*
/// inspected bytes, then relay the rest. Unknown SNI / absent SNI / malformed
/// ClientHello all use the fallback when set, otherwise the connection is
/// closed.
async fn relay_tcp_sni(
    mut client: Stream,
    routes: Arc<HashMap<String, SocketAddr>>,
    fallback: Option<SocketAddr>,
    connect_timeout: Duration,
    idle_timeout: Duration,
    shutdown: &ShutdownWatch,
    metrics: &TcpMetrics,
) -> RelayOutcome {
    // 1. Bounded inspection: read until a complete ClientHello is available.
    let inspected = tls::read_client_hello(&mut client, tls::MAX_CLIENT_HELLO_BYTES).await;
    // 2. Select the upstream: exact SNI -> its route; else the fallback; else
    // close (recorded under the relevant outcome).
    let (upstream_addr, route_reason) = match &inspected {
        tls::InspectOutcome::Sni { name, .. } => match select_sni_route(&routes, name) {
            Some(addr) => (addr, "sni_routed"),
            None => match fallback {
                Some(f) => (f, "sni_fallback"),
                None => {
                    return RelayOutcome {
                        c2u: 0,
                        u2c: 0,
                        upstream: SocketAddr::from(([0, 0, 0, 0], 0)),
                        reason: "sni_no_route",
                    }
                }
            },
        },
        tls::InspectOutcome::NoSni { .. } => match fallback {
            Some(f) => (f, "sni_no_sni"),
            None => {
                return RelayOutcome {
                    c2u: 0,
                    u2c: 0,
                    upstream: SocketAddr::from(([0, 0, 0, 0], 0)),
                    reason: "sni_no_sni",
                }
            }
        },
        tls::InspectOutcome::Malformed { .. } => match fallback {
            Some(f) => (f, "sni_malformed"),
            None => {
                return RelayOutcome {
                    c2u: 0,
                    u2c: 0,
                    upstream: SocketAddr::from(([0, 0, 0, 0], 0)),
                    reason: "sni_malformed",
                }
            }
        },
    };
    tracing::debug!("tcp sni {route_reason} -> {upstream_addr}");
    // 3. Connect under the wall-clock bound.
    let mut upstream =
        match tokio::time::timeout(connect_timeout, TcpStream::connect(upstream_addr)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                metrics.connect_failures.inc();
                tracing::warn!("tcp connect to {upstream_addr} failed: {e}");
                return RelayOutcome {
                    c2u: 0,
                    u2c: 0,
                    upstream: upstream_addr,
                    reason: "connect_failed",
                };
            }
            Err(_) => {
                metrics.connect_failures.inc();
                tracing::warn!("tcp connect to {upstream_addr} timed out");
                return RelayOutcome {
                    c2u: 0,
                    u2c: 0,
                    upstream: upstream_addr,
                    reason: "connect_timeout",
                };
            }
        };
    // 4. Forward the exact inspected prefix (the ClientHello, or whatever was
    // read before a malformed/oversized/timeout inspection).
    let prefix = match &inspected {
        tls::InspectOutcome::Sni { prefix, .. }
        | tls::InspectOutcome::NoSni { prefix }
        | tls::InspectOutcome::Malformed { prefix } => prefix,
    };
    if upstream.write_all(prefix).await.is_err() || upstream.flush().await.is_err() {
        metrics.connect_failures.inc();
        return RelayOutcome {
            c2u: 0,
            u2c: 0,
            upstream: upstream_addr,
            reason: "prefix_write_failed",
        };
    }
    // 5. Relay the remaining bytes (the inspected prefix is already forwarded).
    let (c2u, u2c, end) = bidirectional(client, upstream, idle_timeout, shutdown).await;
    metrics.client_to_upstream_bytes.inc_by(c2u as f64);
    metrics.upstream_to_client_bytes.inc_by(u2c as f64);
    match end {
        EndReason::Idle => metrics.idle_timeouts.inc(),
        EndReason::Shutdown => metrics.shutdown_cancellations.inc(),
        EndReason::Closed => {}
    }
    RelayOutcome {
        c2u,
        u2c,
        upstream: upstream_addr,
        reason: end.as_str(),
    }
}

/// Why a bidirectional relay ended.
enum EndReason {
    /// Both directions finished normally (EOF or error).
    Closed,
    /// The inactivity timeout elapsed with no traffic.
    Idle,
    /// The server began shutting down.
    Shutdown,
}

impl EndReason {
    fn as_str(&self) -> &'static str {
        match self {
            EndReason::Closed => "closed",
            EndReason::Idle => "idle_timeout",
            EndReason::Shutdown => "shutdown",
        }
    }
}

/// Relay bytes between a client stream and an upstream socket.
///
/// A *true* inactivity timeout: both directions share one `last_activity`
/// clock, reset by traffic in either direction, and a watchdog closes the
/// connection once it has been idle for `idle` — without imposing a maximum
/// connection lifetime. Half-close is propagated (EOF on one side shuts the
/// write half of the other), so the remaining direction drains. When the
/// server begins shutting down the relay is cancelled promptly.
async fn bidirectional(
    mut client: Stream,
    mut upstream: TcpStream,
    idle: Duration,
    shutdown: &ShutdownWatch,
) -> (u64, u64, EndReason) {
    let last = Arc::new(tokio::sync::Mutex::new(Instant::now()));
    let stop = Arc::new(AtomicBool::new(false));

    // Watchdog: once the connection has been idle for `idle`, or the server
    // begins shutting down, set `stop`. The loop recomputes the deadline from
    // the shared activity clock each iteration, so traffic in either direction
    // extends it; only a genuinely idle connection trips the `now >= deadline`
    // check. `shutdown.changed()` (re-armed every iteration) plus the
    // borrow-check at the top make shutdown prompt.
    {
        let last = last.clone();
        let stop = stop.clone();
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                if *shutdown.borrow() {
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                let deadline = *last.lock().await + idle;
                let now = Instant::now();
                if now >= deadline {
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(deadline - now) => {
                        // Loop to recompute the deadline: activity during the
                        // sleep extended `last`, so the connection is not idle.
                    }
                    _ = shutdown.changed() => {}
                }
            }
        });
    }

    // Split each side into read/write halves so both directions run
    // concurrently on owned halves (`Stream` is `Unpin`).
    let (client_r, client_w) = tokio::io::split(&mut client);
    let (upstream_r, upstream_w) = tokio::io::split(&mut upstream);

    let (c2u, u2c) = tokio::join!(
        pump(client_r, upstream_w, &last, &stop),
        pump(upstream_r, client_w, &last, &stop),
    );

    let end = if *shutdown.borrow() {
        EndReason::Shutdown
    } else if stop.load(Ordering::Relaxed) {
        // `stop` was set by the idle watchdog (shutdown already handled above).
        EndReason::Idle
    } else {
        EndReason::Closed
    };
    (c2u, u2c, end)
}

/// Copy bytes from `src` to `dst`, resetting the shared activity clock on each
/// read and stopping when `stop` is set. On `src` EOF, half-closes `dst`'s
/// write side so the far end sees EOF while the other direction keeps flowing.
async fn pump<R, W>(
    mut src: R,
    mut dst: W,
    last: &Arc<tokio::sync::Mutex<Instant>>,
    stop: &Arc<AtomicBool>,
) -> u64
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; RELAY_CHUNK];
    let mut total = 0u64;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match src.read(&mut buf).await {
            Ok(0) => {
                // EOF: propagate half-close, then let the other direction end.
                let _ = dst.shutdown().await;
                break;
            }
            Ok(n) => {
                if dst.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                // Pingora's transport `Stream` wraps the socket in a buffered
                // writer; flush so relayed bytes reach the peer promptly rather
                // than sitting in the write buffer until it fills.
                if dst.flush().await.is_err() {
                    break;
                }
                total += n as u64;
                *last.lock().await = Instant::now();
            }
            Err(_) => break,
        }
    }
    total
}

/// The peer address of a stream, if the transport can report it.
fn peer_addr(session: &mut Stream) -> Option<SocketAddr> {
    let digest = session.get_socket_digest()?;
    digest
        .peer_addr
        .get()?
        .as_ref()
        .and_then(|addr| addr.as_inet())
        .copied()
        .map(normalize_socket_addr)
}

/// Collapse an IPv4-mapped IPv6 peer address from a dual-stack listener.
fn normalize_socket_addr(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(address) => address
            .ip()
            .to_ipv4()
            .map(|ip| SocketAddr::new(ip.into(), address.port()))
            .unwrap_or(SocketAddr::V6(address)),
        SocketAddr::V4(address) => SocketAddr::V4(address),
    }
}

/// Resolve a layer-4 upstream host: a literal IP parses directly, a hostname
/// resolves to all its A/AAAA addresses (both families). Blocking; used at
/// startup, on reload rebuilds, and by the DNS refresher. Shared with the UDP
/// proxy.
pub(crate) fn resolve_upstream(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("failed to resolve {host}:{port}: {e}"))?;
    Ok(addrs.collect())
}

/// Whether a `tcp` listener has any hostname upstream (a DNS refresh applies);
/// a listener with only IP-literal upstreams never changes resolution. Covers
/// `to` upstreams, SNI routes, and the SNI fallback.
fn has_hostname_upstreams(tcp: &TcpProxyConfig) -> bool {
    tcp.upstreams
        .iter()
        .chain(tcp.sni_routes.iter().map(|r| &r.upstream))
        .chain(tcp.sni_fallback.iter())
        .any(|u| u.host.parse::<std::net::IpAddr>().is_err())
}

/// The DNS refresh period, overridable via `RADDEX_DNS_REFRESH_SECS` (a test
/// hook so a short-lived CI run can observe a refresh).
fn dns_refresh_interval() -> Duration {
    std::env::var("RADDEX_DNS_REFRESH_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DNS_REFRESH_INTERVAL)
}

/// One background DNS refresh: re-resolve the listener's hostname upstreams
/// from the live snapshot and, when the resolved address set changed, rebuild
/// the runtime for new connections. A transient resolution or rebuild failure
/// keeps the last-known-good addresses and is counted (L4 plan).
fn refresh_dns(
    config_store: &ConfigStore,
    listen: &ListenAddress,
    runtime: &Mutex<AppRuntime>,
    metrics: &TcpMetrics,
    listener: &str,
) {
    let config = config_store.load();
    let Some(tcp) = config.layer4.iter().find_map(|l| match l {
        Layer4Listener::Tcp(t) if t.listen == *listen => Some(t),
        _ => None,
    }) else {
        // The listener vanished from the snapshot — a topology change that
        // reload should have rejected; nothing to refresh.
        return;
    };
    match resolved_set(tcp, listen) {
        Ok(new_resolved) => {
            let mut guard = runtime.lock().expect("L4 runtime lock poisoned");
            // Compare the *sets* of addresses (order can rotate between
            // resolutions without meaning the backend changed) so a benign
            // reorder does not rebuild the balancer and reset health state.
            let mut current: Vec<SocketAddr> = guard.resolved.clone();
            current.sort_unstable();
            let mut next: Vec<SocketAddr> = new_resolved.clone();
            next.sort_unstable();
            if current != next {
                match AppRuntime::build(tcp, listen) {
                    Ok(new_runtime) => {
                        tracing::info!(
                            "tcp {listener}: upstream addresses changed via DNS refresh"
                        );
                        *guard = new_runtime;
                    }
                    Err(e) => {
                        metrics.dns_refresh_failures.inc();
                        tracing::warn!(
                            "tcp {listener}: DNS refresh rebuild failed, keeping last-known-good: {e}"
                        );
                    }
                }
            }
        }
        Err(e) => {
            metrics.dns_refresh_failures.inc();
            tracing::warn!("tcp {listener}: DNS refresh failed, keeping last-known-good: {e}");
        }
    }
}

/// The current wall clock in epoch milliseconds.
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_upstream_accepts_ip_literals() {
        let addrs = resolve_upstream("127.0.0.1", 9000).unwrap();
        assert_eq!(addrs, vec![SocketAddr::from(([127, 0, 0, 1], 9000))]);
        let addrs = resolve_upstream("::1", 9000).unwrap();
        assert_eq!(
            addrs,
            vec![SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 9000))]
        );
    }

    #[test]
    fn resolve_upstream_resolves_localhost() {
        let addrs = resolve_upstream("localhost", 9000).expect("localhost resolves");
        assert!(!addrs.is_empty());
    }

    fn tcp_config(upstreams: &[&str]) -> TcpProxyConfig {
        TcpProxyConfig {
            listen: ListenAddress::Socket("127.0.0.1:3306".parse().unwrap()),
            tls: None,
            transparent: false,
            upstreams: upstreams
                .iter()
                .map(|u| {
                    let (host, port) = u.rsplit_once(':').expect("host:port");
                    L4Upstream {
                        host: host.to_string(),
                        port: port.parse().unwrap(),
                    }
                })
                .collect(),
            lb_policy: crate::config::ast::LbPolicy::RoundRobin,
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(60),
            max_connections: 10,
            health_check: None,
            sni_routes: vec![],
            sni_fallback: None,
        }
    }

    #[test]
    fn has_hostname_upstreams_detects_hostnames_only() {
        let ip_only = tcp_config(&["127.0.0.1:1"]);
        assert!(!has_hostname_upstreams(&ip_only), "IP literals are static");
        let mixed = tcp_config(&["127.0.0.1:1", "db.internal:1"]);
        assert!(has_hostname_upstreams(&mixed));
    }

    #[test]
    fn resolve_upstreams_resolves_a_tcp_config() {
        let tcp = tcp_config(&["127.0.0.1:9000", "::1:9001"]);
        let resolved = resolve_upstreams(&tcp, &tcp.listen).unwrap();
        assert_eq!(
            resolved,
            vec![
                SocketAddr::from(([127, 0, 0, 1], 9000)),
                SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 9001)),
            ]
        );
    }

    #[test]
    fn build_distinguishes_resolved_sets() {
        // Two runtimes over different upstream sets select differently — the
        // DNS refresher swaps the runtime when the resolved set changes.
        let a = AppRuntime::build(
            &tcp_config(&["127.0.0.1:1", "127.0.0.1:2"]),
            &ListenAddress::Socket("127.0.0.1:3306".parse().unwrap()),
        )
        .unwrap();
        let b = AppRuntime::build(
            &tcp_config(&["127.0.0.1:3", "127.0.0.1:4"]),
            &ListenAddress::Socket("127.0.0.1:3306".parse().unwrap()),
        )
        .unwrap();
        assert_ne!(a.resolved, b.resolved, "different upstreams differ");
        let L4Selector::Balancer(ba) = &*a.selector else {
            panic!("to mode")
        };
        let L4Selector::Balancer(bb) = &*b.selector else {
            panic!("to mode")
        };
        for _ in 0..4 {
            let sa = ba.select(b"").expect("a select");
            let sb = bb.select(b"").expect("b select");
            assert!(a.resolved.contains(&sa));
            assert!(b.resolved.contains(&sb));
        }
    }

    #[test]
    fn sni_routing_builds_route_map() {
        // An SNI-mode config resolves each route to its own upstream and keeps
        // the fallback; no balancer is built.
        let mut tcp = tcp_config(&[]);
        tcp.sni_routes.push(crate::config::ast::SniRoute {
            name: "api.example.com".into(),
            upstream: L4Upstream {
                host: "127.0.0.1".into(),
                port: 9001,
            },
        });
        tcp.sni_fallback = Some(L4Upstream {
            host: "127.0.0.1".into(),
            port: 9009,
        });
        let runtime = AppRuntime::build(&tcp, &tcp.listen).unwrap();
        let L4Selector::Sni { routes, fallback } = &*runtime.selector else {
            panic!("sni mode expected");
        };
        assert_eq!(
            routes.get("api.example.com"),
            Some(&SocketAddr::from(([127, 0, 0, 1], 9001)))
        );
        assert_eq!(*fallback, Some(SocketAddr::from(([127, 0, 0, 1], 9009))));
        assert!(runtime
            .resolved
            .contains(&SocketAddr::from(([127, 0, 0, 1], 9001))));
    }

    #[test]
    fn dns_refresh_failure_keeps_last_known_good() {
        // A transient DNS refresh failure must keep the working backend set and
        // count the failure (L4 plan), never discard it.
        let good = tcp_config(&["127.0.0.1:1"]);
        let runtime = Arc::new(Mutex::new(AppRuntime::build(&good, &good.listen).unwrap()));
        let metrics = TcpMetrics::register("tcp/127.0.0.1:3306");

        // The live snapshot's upstream now fails to resolve.
        let bad = tcp_config(&["nonexistent.invalid:1"]);
        let compiled = crate::config::ast::CompiledConfig {
            global: crate::config::ast::GlobalConfig::default(),
            sites: vec![],
            layer4: vec![Layer4Listener::Tcp(bad)],
        };
        let store = ConfigStore::new(compiled);

        let before = runtime.lock().unwrap().resolved.clone();
        refresh_dns(
            &store,
            &good.listen,
            &runtime,
            &metrics,
            "tcp/127.0.0.1:3306",
        );
        assert_eq!(
            runtime.lock().unwrap().resolved,
            before,
            "last-known-good addresses must be kept on a refresh failure"
        );
        assert_eq!(
            metrics.dns_refresh_failures.get(),
            1,
            "the refresh failure must be counted"
        );
    }
}
