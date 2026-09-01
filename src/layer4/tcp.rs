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
//! The L4 data path is native Tokio: [`TcpListenerService`] owns the accept
//! loop. Each accepted [`tokio::net::TcpStream`] is admitted against the
//! per-listener semaphore ([`TcpProxyApp::admit`]) *before* any per-connection
//! work — so `max_connections` bounds pending TLS handshakes as well as
//! established relays — and then handed to
//! [`TcpProxyApp::handle_connection`], which selects an upstream and spawns a
//! relay task that owns the connection, leaving the acceptor free to take the
//! next one. The relay
//! connects under a hard wall-clock bound, then copies bytes bidirectionally
//! with a *true* inactivity timeout (reset by traffic in either direction) and
//! half-close propagation, and records typed metrics and an access record on
//! close.
//!
//! Nothing here wraps the socket in a buffered transport: `write_all` reaches
//! the kernel directly, which is where the measured advantage over the previous
//! Pingora-`Stream` data path comes from. Pingora remains the *process* host —
//! the listener runs as a `BackgroundService` and observes `ShutdownWatch` —
//! but no byte of relayed traffic passes through it.

use crate::config::ast::{
    wildcard_match_specificity, L4Upstream, Layer4Listener, ListenAddress, TcpHealthCheckSpec,
    TcpProxyConfig,
};
use crate::config::snapshot::ConfigStore;
use crate::layer4::tls;
use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::Semaphore;

/// Initial relay buffer, per direction.
///
/// Relay buffers dominate this proxy's resident memory: two per connection,
/// held for its whole life. A listener holding 10 000 mostly-idle connections
/// pays for every byte of them, which is why the buffer starts small instead of
/// being sized for peak throughput.
const RELAY_CHUNK_MIN: usize = 8 * 1024;

/// Largest relay buffer, per direction.
///
/// Matches Pingora's 64 KiB read buffer, which is the size that made its
/// throughput competitive: a bulk transfer costs one `read` syscall per buffer,
/// so a small buffer multiplies syscalls on exactly the workload that can least
/// afford them. Buffers grow to this only when a connection proves it is
/// carrying enough data to fill them (see [`pump`]), so the memory is spent on
/// active transfers rather than reserved for idle connections.
const RELAY_CHUNK_MAX: usize = 64 * 1024;

/// Pending-connection backlog for an L4 listener.
///
/// Matches Pingora's `LISTENER_BACKLOG`, which is what this path used before it
/// was native. The kernel silently caps this at `net.core.somaxconn`, so the
/// large value is a request, not an allocation. It has to be large: a burst of
/// connections that overflows the accept queue is answered by dropping SYNs,
/// and the client then waits out a retransmission timeout — which collapses the
/// connection rate far more than any per-connection work in this file.
const LISTEN_BACKLOG: i32 = 65_535;

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
    /// Last `ConfigStore` generation reconciled into `runtime`, so an unchanged
    /// config costs one atomic load per connection instead of a spec rebuild.
    applied_generation: AtomicU64,
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
            let balancer = Arc::new(L4Balancer::new(&resolved, tcp.lb_policy, tcp.health_check));
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
pub(crate) struct RuntimeHandle {
    selector: Arc<L4Selector>,
    admission: Arc<Semaphore>,
    connect_timeout: Duration,
    idle_timeout: Duration,
    /// The load-balancing policy (for building the `ip_hash` selection key in
    /// `to` mode; unused in SNI mode).
    policy: crate::config::ast::LbPolicy,
    transparent: bool,
}

/// The health-checked backend selector for one L4 listener, provided by the
/// native [`crate::layer4::balance`] module.
type L4Balancer = crate::layer4::balance::Balancer;

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
            applied_generation: AtomicU64::new(config_store.generation()),
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
        // Fast path: nothing has been reloaded since the last check, so the
        // runtime is already current. This matters because it runs on the
        // accept path for every connection, and the slow path below clones and
        // compares the listener's whole spec — including its upstream and SNI
        // vectors, each holding owned strings. Doing that per connection is
        // pure allocation churn when reloads are rare, which they are.
        let generation = self.config_store.generation();
        if self.applied_generation.load(Ordering::Acquire) == generation {
            return self.handle_from_runtime();
        }
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
        // Record the generation only after reconciling it, so a reload racing
        // with this check is picked up by the next connection rather than lost.
        self.applied_generation.store(generation, Ordering::Release);
        RuntimeHandle {
            selector: guard.selector.clone(),
            admission: guard.admission.clone(),
            connect_timeout: guard.connect_timeout,
            idle_timeout: guard.idle_timeout,
            policy: guard.spec.lb_policy,
            transparent: guard.spec.transparent,
        }
    }

    /// Snapshot the current runtime without consulting the config store.
    fn handle_from_runtime(&self) -> RuntimeHandle {
        let guard = self.runtime.lock().expect("L4 runtime lock poisoned");
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

/// The native accept loop for one non-transparent `tcp` listener.
///
/// Owns the bound socket and hands every accepted connection to the shared
/// [`TcpProxyApp`]. Registered as a Pingora `BackgroundService` purely for
/// process lifecycle — no relayed byte passes through Pingora.
pub struct TcpListenerService {
    /// The listening sockets, one per accept loop.
    ///
    /// A single accept loop is a scalability ceiling: every connection is
    /// accepted, admitted, and dispatched by one task, so connection-rate
    /// workloads queue behind it while Nginx spreads the same work over its
    /// workers. `SO_REUSEPORT` gives each loop its own socket and lets the
    /// kernel distribute incoming connections between them.
    ///
    /// Bound at construction — which happens on the startup thread, before the
    /// Pingora server builds its runtime — and converted to Tokio listeners
    /// inside [`BackgroundService::start`], which does run on the runtime.
    /// `tokio::net::TcpListener::from_std` panics without a reactor, so the
    /// sockets cannot be registered any earlier. Empty until received when this
    /// process is an upgrade replacement.
    listeners: Mutex<Vec<std::net::TcpListener>>,
    app: Arc<TcpProxyApp>,
    /// Set when the listener terminates TLS before relaying (spec §5.7).
    tls: Option<Arc<crate::layer4::tls_accept::TlsAcceptor>>,
    /// The `tcp/<address>` label, for logs and handoff artifacts.
    label: String,
    /// How many accept loops (and therefore `SO_REUSEPORT` sockets) this
    /// listener runs. Matches the configured worker-thread count.
    accept_loops: usize,
    /// True when this process must receive the listening socket from the
    /// outgoing process instead of binding it.
    upgrade: bool,
    #[cfg(unix)]
    upgrade_sock: String,
    #[cfg(unix)]
    handoff_id: String,
    #[cfg(unix)]
    phase_watch: Mutex<Option<tokio::sync::broadcast::Receiver<pingora::server::ExecutionPhase>>>,
}

impl TcpListenerService {
    /// Bind `tcp.listen` (or prepare to inherit it) and build the proxy app.
    ///
    /// `tls` terminates TLS on this listener when set. `upgrade` marks this
    /// process as the replacement side of a zero-downtime upgrade, in which
    /// case no socket is bound here — the listening descriptor arrives from the
    /// outgoing process in [`BackgroundService::start`]. Returns an error when
    /// the socket cannot be bound or the proxy app cannot be built; both are
    /// startup failures, reported before any listener serves traffic.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tcp: &TcpProxyConfig,
        config_store: Arc<ConfigStore>,
        access_log: Option<Arc<AccessLogSink>>,
        tls: Option<Arc<crate::layer4::tls_accept::TlsAcceptor>>,
        upgrade: bool,
        accept_loops: usize,
        #[cfg(unix)] upgrade_sock: String,
        #[cfg(unix)] phase_watch: Option<
            tokio::sync::broadcast::Receiver<pingora::server::ExecutionPhase>,
        >,
    ) -> Result<Self, String> {
        let ListenAddress::Socket(address) = &tcp.listen;
        let label = format!("tcp/{}", tcp.listen.display());
        let accept_loops = accept_loops.max(1);
        let listeners = if upgrade {
            Vec::new()
        } else {
            (0..accept_loops)
                .map(|_| bind_listener(*address, accept_loops > 1))
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(Self {
            accept_loops,
            listeners: Mutex::new(listeners),
            app: Arc::new(TcpProxyApp::new(tcp, config_store, access_log)?),
            tls,
            #[cfg(unix)]
            handoff_id: crate::layer4::handoff_key(&tcp.listen.display()),
            label,
            upgrade,
            #[cfg(unix)]
            upgrade_sock,
            #[cfg(unix)]
            phase_watch: Mutex::new(phase_watch),
        })
    }

    /// The status path one TCP listener publishes its handoff outcome to, so
    /// the upgrade driver can refuse to continue after a failed transfer.
    #[cfg(unix)]
    pub(crate) fn status_path_for(upgrade_sock: &str, listener: &str) -> String {
        format!(
            "{upgrade_sock}.tcp.{}.status",
            crate::layer4::handoff_key(listener)
        )
    }

    /// This listener's handoff status path.
    #[cfg(unix)]
    fn handoff_status_path(&self) -> String {
        format!("{}.tcp.{}.status", self.upgrade_sock, self.handoff_id)
    }

    /// The socket path the listening descriptor is passed over.
    #[cfg(unix)]
    fn handoff_socket_path(&self) -> String {
        format!("{}.tcp.{}.fd", self.upgrade_sock, self.handoff_id)
    }

    /// Send the listening descriptor to the replacement process, then publish
    /// the outcome so the upgrade driver can verify it.
    ///
    /// Unlike the UDP handoff there is no per-connection state to carry: an
    /// established TCP relay is owned by its own task in the outgoing process
    /// and drains there, so only the *listening* socket moves.
    #[cfg(unix)]
    async fn send_handoff(&self) {
        let result = self.send_handoff_inner().await;
        if let Err(error) = &result {
            tracing::error!("{}: upgrade handoff failed: {error}", self.label);
        }
        let status = match &result {
            Ok(()) => "ok".to_string(),
            Err(error) => format!("error: {error}"),
        };
        if let Err(error) = crate::layer4::write_handoff_file(
            &self.handoff_status_path(),
            status.as_bytes(),
            "TCP handoff status",
        ) {
            tracing::error!("{}: failed to publish handoff status: {error}", self.label);
        }
    }

    #[cfg(unix)]
    async fn send_handoff_inner(&self) -> Result<(), String> {
        let fds: Vec<i32> = {
            let guard = self
                .listeners
                .lock()
                .expect("L4 TCP listener lock poisoned");
            if guard.is_empty() {
                return Err("TCP listening socket is unavailable".to_string());
            }
            guard.iter().map(|listener| listener.as_raw_fd()).collect()
        };
        let path = self.handoff_socket_path();
        // `Fds::send_to_sock` blocks on the unix socket, so it runs off the
        // reactor thread. Every `SO_REUSEPORT` socket is transferred, so the
        // replacement inherits the same accept parallelism.
        tokio::task::spawn_blocking(move || {
            let mut transfer = pingora::server::Fds::new();
            for (index, fd) in fds.iter().enumerate() {
                transfer.add(format!("listener-{index}"), *fd);
            }
            transfer
                .send_to_sock(path.as_str())
                .map_err(|e| format!("send TCP listener descriptors: {e}"))?;
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("TCP handoff worker failed: {e}"))?
    }

    /// Receive the inherited listening descriptor from the outgoing process.
    #[cfg(unix)]
    async fn receive_handoff(&self) -> Result<Vec<std::net::TcpListener>, String> {
        let path = self.handoff_socket_path();
        let expected = self.accept_loops;
        tokio::task::spawn_blocking(move || {
            let mut transfer = pingora::server::Fds::new();
            transfer
                .get_from_sock(path.as_str())
                .map_err(|e| format!("receive TCP listener descriptors: {e}"))?;
            let mut listeners = Vec::with_capacity(expected);
            for index in 0..expected {
                let fd = *transfer
                    .get(&format!("listener-{index}"))
                    .ok_or_else(|| format!("TCP handoff is missing listener descriptor {index}"))?;
                // SAFETY: the descriptor was transferred through SCM_RIGHTS and
                // is now owned by this process.
                let listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
                listener
                    .set_nonblocking(true)
                    .map_err(|e| format!("set inherited TCP listener nonblocking: {e}"))?;
                listeners.push(listener);
            }
            Ok(listeners)
        })
        .await
        .map_err(|e| format!("TCP handoff receiver failed: {e}"))?
    }
}

#[async_trait]
impl BackgroundService for TcpListenerService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        // An upgrade replacement inherits the listening socket instead of
        // binding it, so the port is never released and no connection is
        // refused during the swap.
        #[cfg(unix)]
        if self.upgrade {
            match self.receive_handoff().await {
                Ok(listener) => {
                    *self
                        .listeners
                        .lock()
                        .expect("L4 TCP listener lock poisoned") = listener;
                }
                Err(error) => {
                    tracing::error!("{}: failed to inherit listener: {error}", self.label);
                    let status = format!("error: {error}");
                    let _ = crate::layer4::write_handoff_file(
                        &self.handoff_status_path(),
                        status.as_bytes(),
                        "TCP handoff status",
                    );
                    // Fail the whole replacement: continuing would leave this
                    // listener silently unserved while the old process exits.
                    std::process::exit(1);
                }
            }
        }

        // Register the pre-bound sockets with the reactor now that a runtime
        // exists. The `std` listeners are retained (cloned, not consumed)
        // because their descriptors are what a later upgrade hands over.
        let std_listeners: Vec<std::net::TcpListener> = self
            .listeners
            .lock()
            .expect("L4 TCP listener lock poisoned")
            .iter()
            .filter_map(|listener| listener.try_clone().ok())
            .collect();
        if std_listeners.is_empty() {
            tracing::error!("{}: listening socket is unavailable", self.label);
            if self.upgrade {
                std::process::exit(1);
            }
            return;
        }
        let mut listeners = Vec::with_capacity(std_listeners.len());
        for std_listener in std_listeners {
            match TcpListener::from_std(std_listener) {
                Ok(listener) => listeners.push(Arc::new(listener)),
                Err(error) => {
                    tracing::error!("{}: listener registration failed: {error}", self.label);
                    if self.upgrade {
                        std::process::exit(1);
                    }
                    return;
                }
            }
        }

        // Pingora signals the fd-transfer phase of a graceful upgrade on this
        // broadcast channel; that is when the descriptors must be sent.
        let (handoff_tx, mut handoff_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        #[cfg(unix)]
        if let Some(mut phase) = self
            .phase_watch
            .lock()
            .expect("L4 TCP phase watch lock poisoned")
            .take()
        {
            tokio::spawn(async move {
                while let Ok(value) = phase.recv().await {
                    if matches!(
                        value,
                        pingora::server::ExecutionPhase::GracefulUpgradeTransferringFds
                    ) {
                        let _ = handoff_tx.send(());
                        break;
                    }
                }
            });
        }
        #[cfg(not(unix))]
        drop(handoff_tx);

        // One accept task per `SO_REUSEPORT` socket. `stop` ends them all at
        // once, so the handoff and the shutdown watch stay single-signal.
        let stop = Arc::new(tokio::sync::Notify::new());
        let mut workers = Vec::with_capacity(listeners.len());
        for listener in listeners {
            let app = self.app.clone();
            let tls = self.tls.clone();
            let shutdown = shutdown.clone();
            let stop = stop.clone();
            workers.push(tokio::spawn(async move {
                accept_loop(listener, app, tls, shutdown, stop).await;
            }));
        }

        // Wait for whichever ends the listener: shutdown, or the upgrade
        // handoff. The accept tasks keep running until `stop` is notified.
        tokio::select! {
            signal = handoff_rx.recv() => {
                if signal.is_some() {
                    #[cfg(unix)]
                    self.send_handoff().await;
                }
            }
            _ = async {
                loop {
                    if *shutdown.borrow() {
                        break;
                    }
                    if shutdown.changed().await.is_err() {
                        break;
                    }
                }
            } => {}
        }
        // Stop accepting. In-flight relays keep running until they drain or the
        // shutdown watch cancels them.
        stop.notify_waiters();
        for worker in workers {
            let _ = worker.await;
        }
    }
}

/// Accept connections on one socket until `stop` is notified.
///
/// Each `SO_REUSEPORT` socket gets one of these, so accept work scales with the
/// configured worker count instead of funnelling through a single task.
async fn accept_loop(
    listener: Arc<TcpListener>,
    app: Arc<TcpProxyApp>,
    tls: Option<Arc<crate::layer4::tls_accept::TlsAcceptor>>,
    shutdown: ShutdownWatch,
    stop: Arc<tokio::sync::Notify>,
) {
    loop {
        let accepted = tokio::select! {
            result = listener.accept() => result,
            _ = stop.notified() => break,
        };
        let (stream, peer) = match accepted {
            Ok(pair) => pair,
            Err(error) => {
                // A per-connection accept error (EMFILE, a client that went
                // away between the SYN and accept) must not kill the listener;
                // log and take the next one.
                tracing::warn!("tcp accept failed: {error}");
                continue;
            }
        };
        // Relayed traffic is latency-sensitive and already chunked by the pump,
        // so Nagle only adds delay.
        let _ = stream.set_nodelay(true);
        let peer = Some(normalize_socket_addr(peer));
        let child_shutdown = shutdown.clone();
        // Admit before the handshake so `max_connections` bounds pending
        // handshakes too, not only established relays.
        let Some((handle, permit)) = app.admit() else {
            continue;
        };
        match &tls {
            None => {
                app.handle_connection(stream, peer, None, &child_shutdown, handle, permit)
                    .await;
            }
            Some(acceptor) => {
                // The handshake is remote-driven, so it runs inside the spawned
                // task under its own timeout: a stalled client must never block
                // the accept loop. The permit moves with it and is released if
                // the handshake fails.
                let acceptor = acceptor.clone();
                let app = app.clone();
                tokio::spawn(async move {
                    match acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            app.handle_connection(
                                tls_stream,
                                peer,
                                None,
                                &child_shutdown,
                                handle,
                                permit,
                            )
                            .await;
                        }
                        Err(error) => {
                            drop(permit);
                            tracing::debug!("tcp TLS handshake failed: {error}");
                        }
                    }
                });
            }
        }
    }
}

/// Bind a plain TCP listener for an L4 listener address.
///
/// An unspecified IPv6 address is left dual-stack (`IPV6_V6ONLY` off) so one
/// listener serves both families, matching the HTTP listeners. Returns a
/// blocking `std` listener: binding happens on the startup thread, before a
/// Tokio runtime exists, so reactor registration is deferred to `start`.
///
/// `reuse_port` sets `SO_REUSEPORT`, which is what lets several sockets share
/// the address so each accept loop has its own and the kernel distributes
/// connections between them. It is only set when more than one loop is
/// requested, so a single-loop listener keeps the stricter "one bind wins"
/// behaviour and a genuine port conflict is still an error.
fn bind_listener(address: SocketAddr, reuse_port: bool) -> Result<std::net::TcpListener, String> {
    let domain = if address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))
        .map_err(|e| format!("create TCP socket: {e}"))?;
    socket
        .set_reuse_address(true)
        .map_err(|e| format!("set TCP reuseaddr: {e}"))?;
    if reuse_port {
        socket
            .set_reuse_port(true)
            .map_err(|e| format!("set TCP reuseport: {e}"))?;
    }
    if let SocketAddr::V6(v6) = address {
        if v6.ip().is_unspecified() {
            socket
                .set_only_v6(false)
                .map_err(|e| format!("set TCP dual-stack: {e}"))?;
        }
    }
    socket
        .bind(&SockAddr::from(address))
        .map_err(|e| format!("bind TCP listener {address}: {e}"))?;
    socket
        .listen(LISTEN_BACKLOG)
        .map_err(|e| format!("listen on TCP {address}: {e}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("set TCP nonblocking: {e}"))?;
    Ok(socket.into())
}

/// A Linux transparent-proxy acceptor: same relay path, but the socket carries
/// `IP_TRANSPARENT` and the upstream is the original destination.
pub struct TransparentTcpProxy {
    /// Bound at construction, registered with the reactor in `start` — see
    /// [`TcpListenerService::listener`] for why.
    listener: Mutex<Option<std::net::TcpListener>>,
    app: Arc<TcpProxyApp>,
}

impl TransparentTcpProxy {
    /// Bind a transparent TCP listener and build its proxy app.
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
            listener: Mutex::new(Some(listener)),
            app: Arc::new(TcpProxyApp::new(tcp, config_store, access_log)?),
        })
    }
}

#[async_trait]
impl BackgroundService for TransparentTcpProxy {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let Some(std_listener) = self
            .listener
            .lock()
            .expect("transparent TCP listener lock poisoned")
            .take()
        else {
            tracing::error!("transparent tcp listener started twice");
            return;
        };
        let listener = match TcpListener::from_std(std_listener) {
            Ok(listener) => listener,
            Err(error) => {
                tracing::error!("transparent tcp listener registration failed: {error}");
                return;
            }
        };
        loop {
            let accepted = tokio::select! {
                result = listener.accept() => result,
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
            let _ = stream.set_nodelay(true);
            let original = original_dst(&stream);
            tracing::debug!("transparent TCP connection from {peer}");
            let app = self.app.clone();
            let child_shutdown = shutdown.clone();
            let Some((handle, permit)) = app.admit() else {
                continue;
            };
            app.handle_connection(
                stream,
                Some(normalize_socket_addr(peer)),
                original,
                &child_shutdown,
                handle,
                permit,
            )
            .await;
        }
    }
}

/// Bind a TCP socket with the Linux transparent-proxy option before bind.
///
/// Returns a blocking `std` listener for the same reason as [`bind_listener`]:
/// the bind happens before a Tokio runtime exists.
fn bind_transparent_listener(address: SocketAddr) -> Result<std::net::TcpListener, String> {
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
    Ok(socket.into())
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
///
/// With TPROXY the socket is bound to the original destination already, so
/// `local_addr` is the answer. With REDIRECT the kernel rewrote the destination
/// and keeps the pre-NAT address in the conntrack entry, reachable only through
/// `SO_ORIGINAL_DST`. Try the socket option first and fall back to `local_addr`.
#[cfg(target_os = "linux")]
fn original_dst(stream: &TcpStream) -> Option<SocketAddr> {
    let fd = stream.as_raw_fd();
    let ipv4 = matches!(stream.local_addr().ok()?, SocketAddr::V4(_));
    // SAFETY: `storage` is sized for either address family and `len` is
    // initialized to its capacity, as getsockopt requires. The fd is owned by
    // `stream`, which outlives the call.
    let decoded = unsafe {
        let mut storage: libc::sockaddr_storage = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let (level, name) = if ipv4 {
            (libc::SOL_IP, libc::SO_ORIGINAL_DST)
        } else {
            (libc::SOL_IPV6, IP6T_SO_ORIGINAL_DST)
        };
        if libc::getsockopt(
            fd,
            level,
            name,
            std::ptr::addr_of_mut!(storage).cast::<libc::c_void>(),
            &mut len,
        ) != 0
        {
            None
        } else {
            sockaddr_storage_to_socket_addr(&storage)
        }
    };
    // TPROXY binds the socket to the original destination, so `local_addr` is
    // already correct there and is the right fallback when the REDIRECT-only
    // socket option is unavailable.
    decoded
        .or_else(|| stream.local_addr().ok())
        .map(normalize_socket_addr)
}

/// `IP6T_SO_ORIGINAL_DST` from `linux/netfilter_ipv6/ip6_tables.h`; `libc` does
/// not export it.
#[cfg(target_os = "linux")]
const IP6T_SO_ORIGINAL_DST: libc::c_int = 80;

/// Decode a kernel-filled `sockaddr_storage` into a [`SocketAddr`].
///
/// Returns `None` for an address family this proxy does not handle, so a
/// surprising value falls back to the local address rather than being
/// misinterpreted.
#[cfg(target_os = "linux")]
fn sockaddr_storage_to_socket_addr(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match i32::from(storage.ss_family) {
        libc::AF_INET => {
            // SAFETY: ss_family says this storage holds a sockaddr_in, and
            // sockaddr_storage is defined to be large enough and aligned for it.
            let addr = unsafe { *std::ptr::from_ref(storage).cast::<libc::sockaddr_in>() };
            Some(SocketAddr::from((
                u32::from_be(addr.sin_addr.s_addr).to_be_bytes(),
                u16::from_be(addr.sin_port),
            )))
        }
        libc::AF_INET6 => {
            // SAFETY: as above, for a sockaddr_in6.
            let addr = unsafe { *std::ptr::from_ref(storage).cast::<libc::sockaddr_in6>() };
            Some(SocketAddr::from((
                addr.sin6_addr.s6_addr,
                u16::from_be(addr.sin6_port),
            )))
        }
        _ => None,
    }
}

/// Non-Linux builds never reach transparent mode (the config validator rejects
/// it), so the original destination is simply the local address.
#[cfg(not(target_os = "linux"))]
fn original_dst(stream: &TcpStream) -> Option<SocketAddr> {
    stream.local_addr().ok().map(normalize_socket_addr)
}

impl TcpProxyApp {
    /// Admit, select an upstream for, and relay one accepted connection.
    ///
    /// `client` is the accepted transport — a plain [`TcpStream`], or a
    /// TLS-terminated stream once the listener terminates TLS. `peer` is the
    /// client address and `original_destination` the Linux REDIRECT/TPROXY
    /// destination in transparent mode (`None` otherwise).
    ///
    /// Returns as soon as the connection has been handed to its relay task, so
    /// the caller can accept the next connection. A connection refused by
    /// admission or by an all-unhealthy backend set is dropped here.
    /// Reserve one admission slot, or report the listener as full.
    ///
    /// Taken by the acceptor *before* any per-connection work — including the
    /// TLS handshake — so `max_connections` bounds everything the listener is
    /// holding open, not just established relays. A client that connects and
    /// never completes a handshake would otherwise occupy an unbounded number
    /// of tasks and descriptors.
    pub(crate) fn admit(
        self: &Arc<Self>,
    ) -> Option<(RuntimeHandle, tokio::sync::OwnedSemaphorePermit)> {
        // Load the current runtime (rebuilt on reload when the spec changed);
        // new connections always use the latest upstream set and limits.
        let handle = self.current_handle();
        match handle.admission.clone().try_acquire_owned() {
            Ok(permit) => Some((handle, permit)),
            Err(_) => {
                self.metrics.rejected.inc();
                tracing::debug!("tcp {}: admission limit reached; rejecting", self.listener);
                None
            }
        }
    }

    /// Select an upstream for an admitted connection and relay it.
    ///
    /// `client` is the accepted transport — a plain [`TcpStream`], or a
    /// TLS-terminated stream once the listener terminates TLS. `peer` is the
    /// client address and `original_destination` the Linux REDIRECT/TPROXY
    /// destination in transparent mode (`None` otherwise). `handle` and
    /// `permit` come from [`Self::admit`].
    ///
    /// Returns as soon as the connection has been handed to its relay task, so
    /// the caller can accept the next connection. A connection refused by an
    /// all-unhealthy backend set is dropped here, releasing the permit.
    pub(crate) async fn handle_connection<S>(
        self: &Arc<Self>,
        client: S,
        peer: Option<SocketAddr>,
        original_destination: Option<SocketAddr>,
        shutdown: &ShutdownWatch,
        handle: RuntimeHandle,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        tracing::debug!("tcp {}: new connection", self.listener);
        let client_addr = peer;
        let transparent = handle.transparent;
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
                        return;
                    }
                },
                L4Selector::Sni { .. } => None,
            },
        };
        self.metrics.accepted.inc();

        // The relay owns the connection: spawn it so the acceptor takes the
        // next connection immediately.
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
                        client,
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
                        client,
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
async fn relay_tcp<S>(
    client: S,
    upstream_addr: SocketAddr,
    source_addr: Option<SocketAddr>,
    connect_timeout: Duration,
    idle_timeout: Duration,
    shutdown: &ShutdownWatch,
    metrics: &TcpMetrics,
) -> RelayOutcome
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
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
async fn relay_tcp_sni<S>(
    mut client: S,
    routes: Arc<HashMap<String, SocketAddr>>,
    fallback: Option<SocketAddr>,
    connect_timeout: Duration,
    idle_timeout: Duration,
    shutdown: &ShutdownWatch,
    metrics: &TcpMetrics,
) -> RelayOutcome
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
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
async fn bidirectional<S>(
    mut client: S,
    mut upstream: TcpStream,
    idle: Duration,
    shutdown: &ShutdownWatch,
) -> (u64, u64, EndReason)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // The activity clock is an atomic, not a mutex: both pumps touch it on
    // every chunk, and an async lock there serialises the two directions
    // against each other on the hot path. Time is stored as milliseconds since
    // `base`, which is ample resolution for an idle timeout.
    let base = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    // Watchdog: once the connection has been idle for `idle`, or the server
    // begins shutting down, set `stop`. The loop recomputes the deadline from
    // the shared activity clock each iteration, so traffic in either direction
    // extends it; only a genuinely idle connection trips the `now >= deadline`
    // check. `shutdown.changed()` (re-armed every iteration) plus the
    // borrow-check at the top make shutdown prompt.
    let watchdog = {
        let last = last.clone();
        let stop = stop.clone();
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                if *shutdown.borrow() {
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                let deadline = base + Duration::from_millis(last.load(Ordering::Relaxed)) + idle;
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
        })
    };

    // Split each side into read/write halves so both directions run
    // concurrently on owned halves (`Stream` is `Unpin`).
    let (client_r, client_w) = tokio::io::split(&mut client);
    let (upstream_r, upstream_w) = tokio::io::split(&mut upstream);

    let (c2u, u2c) = tokio::join!(
        pump(client_r, upstream_w, base, &last, &stop),
        pump(upstream_r, client_w, base, &last, &stop),
    );
    // Both directions are done, so the watchdog has nothing left to guard.
    // Without this it would sleep out the remaining idle timeout — an hour, on
    // a long-`idle_timeout` listener — holding a task and a timer per closed
    // connection.
    watchdog.abort();

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
    base: Instant,
    last: &Arc<AtomicU64>,
    stop: &Arc<AtomicBool>,
) -> u64
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Grows on demand: see `RELAY_CHUNK_MIN` / `RELAY_CHUNK_MAX`. An idle
    // connection keeps the small buffer forever; a bulk transfer reaches the
    // large one within a few reads.
    let mut buf = vec![0u8; RELAY_CHUNK_MIN];
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
                // A full buffer means the socket had at least this much queued,
                // so the next read can likely be larger. Doubling costs one
                // reallocation per step and stops at `RELAY_CHUNK_MAX`.
                if n == buf.len() && buf.len() < RELAY_CHUNK_MAX {
                    buf.resize((buf.len() * 2).min(RELAY_CHUNK_MAX), 0);
                }
                // A plain `TcpStream` has no user-space write buffer, so this
                // flush is a no-op and `write_all` above already reached the
                // kernel — that is the win over the previous buffered Pingora
                // transport. It is kept because a TLS-terminated listener hands
                // this pump an `SslStream`, whose write BIO *does* buffer.
                if dst.flush().await.is_err() {
                    break;
                }
                total += n as u64;
                last.store(base.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
            Err(_) => break,
        }
    }
    total
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

    /// A loopback address on a free port, for the bind tests.
    fn free_addr() -> SocketAddr {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        probe.local_addr().expect("probe address")
    }

    #[test]
    fn reuse_port_lets_every_accept_loop_own_a_socket() {
        // The scalability fix depends on this: N accept loops each need their
        // own listening socket on the same address.
        let addr = free_addr();
        let first = bind_listener(addr, true).expect("first reuseport bind");
        let second = bind_listener(addr, true).expect("second reuseport bind");
        assert_eq!(
            first.local_addr().expect("addr"),
            second.local_addr().expect("addr")
        );
    }

    #[test]
    fn a_single_loop_listener_still_rejects_a_port_conflict() {
        // Without SO_REUSEPORT a second bind must fail, so a genuine port
        // conflict is still reported instead of being silently shared.
        let addr = free_addr();
        let _held = bind_listener(addr, false).expect("first bind");
        let error = bind_listener(addr, false).expect_err("a conflicting bind must fail");
        assert!(error.contains("bind TCP listener"), "got: {error}");
    }

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
