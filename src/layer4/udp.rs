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

//! UDP proxy runtime (L4 P2).
//!
//! One [`UdpProxy`] is bound to one `udp <address> { ... }` listener and runs
//! as a Pingora [`BackgroundService`] so it starts and stops with the server.
//! The listener binds one `SO_REUSEPORT` socket per worker thread, each drained
//! by its own receive loop; the kernel hashes a datagram's 4-tuple to a socket,
//! so every datagram of one client flow lands on the same loop. This is what
//! makes the kernel receive buffer and the drain rate scale with `--threads`:
//! one socket cannot be drained faster than one task manages, and datagrams
//! arriving while that task is creating a flow are dropped by the kernel
//! (counted as `UdpRcvbufErrors`, never seen by the process).
//!
//! Each receive loop looks up or creates a flow per client. A flow has a
//! *connected* upstream UDP socket whose ephemeral local port demultiplexes
//! upstream responses, plus a reader task that sends those responses back to
//! the client.
//!
//! Resources are bounded: the flow table is capped (max_flows, with
//! deterministic oldest-first eviction), oversized datagrams are dropped and
//! counted, and a flow idle for idle_timeout is evicted (its upstream socket
//! released). Selection (source-IP hash / round-robin / random) happens once
//! per new flow. A SIGHUP reload updates the upstream set for new flows
//! (existing flows keep their socket). On Linux, zero-downtime upgrades transfer
//! every listener socket, the connected upstream sockets, and bounded flow
//! metadata through a dedicated handoff protocol.

use crate::config::ast::{L4Upstream, Layer4Listener, LbPolicy, ListenAddress, UdpProxyConfig};
use crate::config::snapshot::ConfigStore;
use crate::layer4::tcp::resolve_upstream;
use async_trait::async_trait;
use pingora::server::ShutdownWatch;
#[cfg(unix)]
use pingora::server::{ExecutionPhase, Fds};
use pingora::services::background::BackgroundService;
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

/// One client flow: the connected upstream socket and activity/counters. The
/// upstream socket is shared (`Arc`) by the datagram loop's forward path and
/// the flow's reader task.
struct UdpFlow {
    upstream: Arc<UdpSocket>,
    last_active: Instant,
    /// Datagrams relayed client -> upstream (updated by the datagram loop).
    c2u_packets: u64,
}

/// The serializable part of one UDP flow transferred across a process upgrade.
#[cfg(unix)]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct UdpHandoffFlow {
    client: SocketAddr,
    upstream: SocketAddr,
    client_packets: u64,
}

/// The manifest accompanying the transferred UDP listener and flow sockets.
#[cfg(unix)]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct UdpHandoffManifest {
    chunks: usize,
    /// How many `SO_REUSEPORT` listener descriptors chunk zero carries. Absent
    /// in manifests written before the listener fan-out existed, which always
    /// carried exactly one descriptor named `listener`; `0` therefore means
    /// "one legacy listener" rather than "no listeners".
    #[serde(default)]
    listeners: usize,
    flows: Vec<UdpHandoffFlow>,
}

#[cfg(unix)]
type UdpHandoffState = (
    Vec<std::net::UdpSocket>,
    Vec<(UdpHandoffFlow, Arc<UdpSocket>)>,
);

/// Keep the number of descriptors in every SCM_RIGHTS message below Pingora's
/// receiver limit, leaving room for the listener descriptor in chunk zero.
#[cfg(unix)]
const HANDOFF_FLOWS_PER_CHUNK: usize = 30;

#[cfg(unix)]
/// Hash a listener identity into a filesystem-safe handoff namespace.
fn handoff_key(listener: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in listener.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The selection state of one listener, reload-updatable: a SIGHUP reload swaps
/// the resolved upstream set and policy for *new* flows.
struct UdpState {
    /// The configured upstreams (for reload change detection).
    configured: Vec<L4Upstream>,
    upstreams: Vec<SocketAddr>,
    policy: LbPolicy,
    idle_timeout: Duration,
    max_flows: usize,
    max_datagram_size: usize,
}

/// Prometheus metrics for one UDP listener (labelled by listener).
#[derive(Debug)]
pub struct UdpMetrics {
    /// Datagrams received from clients.
    pub client_datagrams: prometheus::IntCounter,
    /// Datagrams sent to clients (upstream responses).
    pub upstream_datagrams: prometheus::IntCounter,
    /// Flows created.
    pub flows_created: prometheus::IntCounter,
    /// Flows evicted because the table was full.
    pub capacity_evictions: prometheus::IntCounter,
    /// Flows evicted because they were idle too long.
    pub idle_evictions: prometheus::IntCounter,
    /// Datagrams dropped because they exceeded `max_datagram_size`.
    pub oversized_drops: prometheus::IntCounter,
    /// Datagrams dropped because no upstream could be selected.
    pub no_upstream_drops: prometheus::IntCounter,
    /// Socket errors on the listener or an upstream socket.
    pub socket_errors: prometheus::IntCounter,
}

use std::sync::LazyLock;

static CLIENT_DATAGRAMS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_udp_client_datagrams_total",
        "UDP datagrams received from clients",
        &["listener"]
    )
    .expect("register counter vec")
});
static UPSTREAM_DATAGRAMS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_udp_upstream_datagrams_total",
        "UDP datagrams sent to clients (upstream responses)",
        &["listener"]
    )
    .expect("register counter vec")
});
static FLOWS_CREATED: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_udp_flows_created_total",
        "UDP flows created",
        &["listener"]
    )
    .expect("register counter vec")
});
static CAPACITY_EVICTIONS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_udp_capacity_evictions_total",
        "UDP flows evicted because the table was full",
        &["listener"]
    )
    .expect("register counter vec")
});
static IDLE_EVICTIONS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_udp_idle_evictions_total",
        "UDP flows evicted because they were idle too long",
        &["listener"]
    )
    .expect("register counter vec")
});
static OVERSIZED_DROPS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_udp_oversized_drops_total",
        "UDP datagrams dropped because they exceeded max_datagram_size",
        &["listener"]
    )
    .expect("register counter vec")
});
static NO_UPSTREAM_DROPS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_udp_no_upstream_drops_total",
        "UDP datagrams dropped because no upstream could be selected",
        &["listener"]
    )
    .expect("register counter vec")
});
static SOCKET_ERRORS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddex_l4_udp_socket_errors_total",
        "UDP socket errors",
        &["listener"]
    )
    .expect("register counter vec")
});

impl UdpMetrics {
    fn register(listener: &str) -> Arc<Self> {
        let label = [listener];
        Arc::new(Self {
            client_datagrams: CLIENT_DATAGRAMS.with_label_values(&label),
            upstream_datagrams: UPSTREAM_DATAGRAMS.with_label_values(&label),
            flows_created: FLOWS_CREATED.with_label_values(&label),
            capacity_evictions: CAPACITY_EVICTIONS.with_label_values(&label),
            idle_evictions: IDLE_EVICTIONS.with_label_values(&label),
            oversized_drops: OVERSIZED_DROPS.with_label_values(&label),
            no_upstream_drops: NO_UPSTREAM_DROPS.with_label_values(&label),
            socket_errors: SOCKET_ERRORS.with_label_values(&label),
        })
    }
}

/// A structured record for a closed UDP flow.
#[derive(Debug, serde::Serialize)]
pub struct UdpFlowRecord {
    /// Epoch milliseconds when the flow was created.
    pub ts_ms: u64,
    /// The listener identity (`udp/<address>`).
    pub listener: String,
    /// The client socket address.
    pub client: SocketAddr,
    /// The selected upstream socket address.
    pub upstream: SocketAddr,
    /// Datagrams relayed client -> upstream / upstream -> client.
    pub client_packets: u64,
    pub upstream_packets: u64,
    /// How the flow ended.
    pub outcome: &'static str,
}

/// A sink for typed UDP flow records (the access-log file, when configured).
type AccessLogSink = dyn Fn(&UdpFlowRecord) + Send + Sync;

/// The datagram-path state of one UDP listener, shared by every receive loop.
///
/// Split out of [`UdpProxy`] because [`BackgroundService::start`] only receives
/// `&self`: a receive loop that runs on its own worker thread must own an `Arc`
/// of everything it touches, so this state cannot live behind that borrow.
/// Mirrors `TcpProxyApp` on the TCP side.
struct UdpApp {
    listener: String,
    listen: ListenAddress,
    config_store: Arc<ConfigStore>,
    /// Reload-updatable selection state.
    ///
    /// `ArcSwap`, not `Mutex`: this is read on the datagram path for every
    /// packet, by every receive loop at once, so a mutex here would serialise
    /// the loops against each other. A rebuild serialises on `rebuild_lock`.
    state: arc_swap::ArcSwap<UdpState>,
    /// Held only while rebuilding `state`, so two loops observing the same
    /// reload do not both resolve upstreams.
    rebuild_lock: Mutex<()>,
    /// The config generation `state` was last reconciled against, so the
    /// common case (no reload) costs one atomic load instead of a config scan.
    applied_generation: AtomicU64,
    /// Bounded flow table keyed by client address. `Arc` so flow reader tasks
    /// hold a `Weak` to detect when their flow is evicted. One lock is shared
    /// by every receive loop; the critical sections are short (lookup +
    /// insert) and the upstream socket is built outside it.
    flows: Arc<Mutex<HashMap<SocketAddr, UdpFlow>>>,
    /// Set on shutdown so flow reader tasks stop and release their sockets.
    stop: Arc<AtomicBool>,
    /// Round-robin / random cursor (persists across reloads and datagrams).
    cursor: AtomicUsize,
    metrics: Arc<UdpMetrics>,
    access_log: Option<Arc<AccessLogSink>>,
}

/// The UDP proxy background service for one listener.
pub struct UdpProxy {
    listener: String,
    /// How many `SO_REUSEPORT` listener sockets this listener binds, one per
    /// receive loop. Each socket carries its own kernel receive buffer, so
    /// both the buffer and the drain rate scale with the worker-thread count
    /// instead of funnelling every client through a single socket.
    recv_loops: usize,
    /// The bound std sockets, one per receive loop. Converted to tokio in
    /// start (where a runtime exists); normal processes bind in new, while
    /// upgrade replacements receive them during startup.
    std_sockets: Mutex<Vec<std::net::UdpSocket>>,
    /// True when this process must receive the UDP sockets and flow table from
    /// the old process during a Pingora upgrade.
    upgrade: bool,
    #[cfg(unix)]
    upgrade_sock: String,
    #[cfg(unix)]
    handoff_id: String,
    #[cfg(unix)]
    phase_watch: Mutex<Option<tokio::sync::broadcast::Receiver<ExecutionPhase>>>,
    app: Arc<UdpApp>,
}

impl UdpProxy {
    /// Bind the listener sockets and build the proxy. Normal processes bind
    /// here; upgrade replacements receive the sockets during background
    /// startup.
    ///
    /// `recv_loops` is the worker-thread count: that many `SO_REUSEPORT`
    /// sockets are bound, each drained by its own receive loop, so the kernel
    /// receive buffer and the drain rate both scale with `--threads`. A single
    /// socket cannot be drained faster than one thread manages, and datagrams
    /// that arrive while it is busy creating a flow are dropped by the kernel
    /// (`UdpRcvbufErrors`) rather than queued.
    ///
    /// Returns the proxy on success. Errors include bind, socket-option, and
    /// upstream-resolution failures.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        udp: &UdpProxyConfig,
        config_store: Arc<ConfigStore>,
        access_log: Option<Arc<AccessLogSink>>,
        upgrade: bool,
        recv_loops: usize,
        #[cfg(unix)] upgrade_sock: String,
        #[cfg(unix)] handoff_id: String,
        #[cfg(unix)] phase_watch: Option<tokio::sync::broadcast::Receiver<ExecutionPhase>>,
    ) -> Result<Self, String> {
        let listener = format!("udp/{}", udp.listen.display());
        let ListenAddress::Socket(address) = &udp.listen;
        let recv_loops = recv_loops.max(1);
        let sockets = if upgrade {
            Vec::new()
        } else {
            (0..recv_loops)
                .map(|_| {
                    bind_udp_listener(*address, recv_loops > 1, udp.recv_buffer, udp.send_buffer)
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let upstreams = resolve_udp_upstreams(udp)?;
        let state = arc_swap::ArcSwap::from_pointee(UdpState {
            configured: udp.upstreams.clone(),
            upstreams,
            policy: udp.lb_policy,
            idle_timeout: udp.idle_timeout,
            max_flows: udp.max_flows,
            max_datagram_size: udp.max_datagram_size,
        });
        Ok(Self {
            listener: listener.clone(),
            recv_loops,
            std_sockets: Mutex::new(sockets),
            upgrade,
            #[cfg(unix)]
            upgrade_sock,
            #[cfg(unix)]
            handoff_id: handoff_key(&handoff_id),
            #[cfg(unix)]
            phase_watch: Mutex::new(phase_watch),
            app: Arc::new(UdpApp {
                cursor: AtomicUsize::new(0),
                listener,
                listen: udp.listen.clone(),
                applied_generation: AtomicU64::new(config_store.generation()),
                config_store,
                state,
                rebuild_lock: Mutex::new(()),
                flows: Arc::new(Mutex::new(HashMap::new())),
                stop: Arc::new(AtomicBool::new(false)),
                metrics: UdpMetrics::register(&format!("udp/{}", udp.listen.display())),
                access_log,
            }),
        })
    }

    /// Return the status path used by one UDP upgrade handoff.
    ///
    /// The upgrade_sock parameter is the Pingora upgrade socket path.
    /// Returns the deterministic status path beside the Pingora upgrade socket.
    pub(crate) fn status_path_for(upgrade_sock: &str, handoff_id: &str) -> String {
        format!("{upgrade_sock}.udp.{}.status", handoff_key(handoff_id))
    }

    /// Return the manifest path for one UDP listener handoff.
    ///
    /// The upgrade_sock parameter is the Pingora upgrade socket path and
    /// handoff_id identifies the listener. Returns a deterministic path.
    fn handoff_manifest_path(&self) -> String {
        format!("{}.udp.{}.manifest", self.upgrade_sock, self.handoff_id)
    }

    /// Return the status path for this listener's UDP handoff.
    fn handoff_status_path(&self) -> String {
        format!("{}.udp.{}.status", self.upgrade_sock, self.handoff_id)
    }

    #[cfg(unix)]
    /// Transfer the listener and all connected upstream flow sockets to the
    /// replacement process. The manifest is written before descriptor chunks,
    /// so the receiver knows exactly how many bounded SCM_RIGHTS messages to
    /// await.
    async fn send_handoff(&self) -> Result<(), String> {
        let result = self.send_handoff_inner().await;
        let status = match &result {
            Ok(()) => "ok".to_string(),
            Err(error) => format!("error: {error}"),
        };
        if let Err(error) = write_handoff_file(&self.handoff_status_path(), status.as_bytes()) {
            tracing::error!(
                "udp {}: failed to publish handoff status: {error}",
                self.listener
            );
            if result.is_ok() {
                return Err(error);
            }
        }
        result
    }

    #[cfg(unix)]
    async fn send_handoff_inner(&self) -> Result<(), String> {
        let (listener_fds, flow_fds, manifest) = {
            let socket_guard = self.std_sockets.lock().expect("UDP socket lock poisoned");
            if socket_guard.is_empty() {
                return Err("UDP listener sockets are unavailable".to_string());
            }
            let listener_fds: Vec<_> = socket_guard.iter().map(|s| s.as_raw_fd()).collect();
            let flows = self.app.flows.lock().expect("UDP flows lock poisoned");
            let mut metadata = Vec::with_capacity(flows.len());
            let mut fds = Vec::with_capacity(flows.len());
            for (client, flow) in flows.iter() {
                let upstream = flow
                    .upstream
                    .peer_addr()
                    .map_err(|e| format!("read UDP upstream peer: {e}"))?;
                metadata.push(UdpHandoffFlow {
                    client: *client,
                    upstream,
                    client_packets: flow.c2u_packets,
                });
                fds.push(flow.upstream.as_raw_fd());
            }
            let chunks = metadata.len().div_ceil(HANDOFF_FLOWS_PER_CHUNK);
            let listeners = listener_fds.len();
            (
                listener_fds,
                fds,
                UdpHandoffManifest {
                    chunks: chunks.max(1),
                    listeners,
                    flows: metadata,
                },
            )
        };
        let manifest_path = self.handoff_manifest_path();
        let payload = serde_json::to_vec(&manifest)
            .map_err(|e| format!("serialize UDP handoff manifest: {e}"))?;
        write_handoff_file(&manifest_path, &payload)?;

        let upgrade_sock = self.upgrade_sock.clone();
        let handoff_id = self.handoff_id.clone();
        tokio::task::spawn_blocking(move || {
            for chunk in 0..manifest.chunks {
                let start = chunk * HANDOFF_FLOWS_PER_CHUNK;
                let end = (start + HANDOFF_FLOWS_PER_CHUNK).min(manifest.flows.len());
                let mut fds = Fds::new();
                if chunk == 0 {
                    for (index, fd) in listener_fds.iter().enumerate() {
                        fds.add(format!("listener-{index}"), *fd);
                    }
                }
                for (index, fd) in flow_fds.iter().enumerate().take(end).skip(start) {
                    fds.add(format!("flow-{index}"), *fd);
                }
                let path = format!("{upgrade_sock}.udp.{handoff_id}.part.{chunk}");
                fds.send_to_sock(path.as_str())
                    .map_err(|e| format!("send UDP handoff chunk {chunk}: {e}"))?;
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("UDP handoff worker failed: {e}"))??;
        Ok(())
    }

    #[cfg(unix)]
    /// Receive the inherited UDP listeners and connected flow sockets.
    async fn receive_handoff(&self) -> Result<UdpHandoffState, String> {
        let upgrade_sock = self.upgrade_sock.clone();
        let handoff_id = self.handoff_id.clone();
        let expected = self.recv_loops;
        tokio::task::spawn_blocking(move || {
            receive_udp_handoff(&upgrade_sock, &handoff_id, expected)
        })
        .await
        .map_err(|e| format!("UDP handoff receiver failed: {e}"))?
    }
}

impl UdpApp {
    /// The current selection state, rebuilt on reload when the upstream set or
    /// policy changed (new flows use it; existing flows keep their socket).
    ///
    /// The generation check short-circuits the common case — no reload since
    /// the last datagram — to one atomic load and an `Arc` clone. It matters
    /// because this runs per datagram on every receive loop at once, and the
    /// slow path below scans the config and clones the listener's upstream
    /// vectors; doing that per datagram is pure allocation churn when reloads
    /// are rare, which they are.
    fn current_state(&self) -> Arc<UdpState> {
        let generation = self.config_store.generation();
        if self.applied_generation.load(Ordering::Acquire) == generation {
            return self.state.load_full();
        }
        let config = self.config_store.load();
        let _rebuild = self.rebuild_lock.lock().expect("UDP rebuild lock poisoned");
        if let Some(udp) = config.layer4.iter().find_map(|l| match l {
            Layer4Listener::Udp(udp) if udp.listen == self.listen => Some(udp),
            _ => None,
        }) {
            let current = self.state.load();
            let spec_changed = udp.upstreams != current.configured
                || udp.lb_policy != current.policy
                || udp.idle_timeout != current.idle_timeout
                || udp.max_flows != current.max_flows
                || udp.max_datagram_size != current.max_datagram_size;
            if spec_changed {
                match resolve_udp_upstreams(udp) {
                    Ok(upstreams) => {
                        tracing::info!("udp {}: upstream set changed on reload", self.listener);
                        self.state.store(Arc::new(UdpState {
                            configured: udp.upstreams.clone(),
                            upstreams,
                            policy: udp.lb_policy,
                            idle_timeout: udp.idle_timeout,
                            max_flows: udp.max_flows,
                            max_datagram_size: udp.max_datagram_size,
                        }));
                    }
                    Err(error) => tracing::error!(
                        "udp {}: reload upstream rebuild failed, keeping previous: {error}",
                        self.listener
                    ),
                }
            }
        }
        // Record the generation only after reconciling it, so a reload racing
        // with this check is picked up by the next datagram rather than lost.
        self.applied_generation.store(generation, Ordering::Release);
        self.state.load_full()
    }

    /// Select the upstream address for a new flow, per the policy (source-IP
    /// hash on the client IP; the flow identity still includes the port).
    fn select(&self, state: &UdpState, client: SocketAddr) -> Option<SocketAddr> {
        if state.upstreams.is_empty() {
            return None;
        }
        let n = state.upstreams.len();
        let index = match state.policy {
            LbPolicy::IpHash => stable_hash(client.ip().to_string().as_bytes()) as usize % n,
            LbPolicy::RoundRobin | LbPolicy::Random => {
                let c = self.cursor.fetch_add(1, Ordering::Relaxed);
                if state.policy == LbPolicy::Random {
                    c.wrapping_mul(1_103_515_245).wrapping_add(12_345) % n
                } else {
                    c % n
                }
            }
        };
        state.upstreams.get(index).copied()
    }

    /// Forward one client datagram: look up or create the flow, then send it
    /// to the flow's upstream (the flow's reader handles the response path).
    ///
    /// `state` is the selection state the caller already loaded for this
    /// datagram, so the config generation is consulted once per packet rather
    /// than once per phase of handling it.
    async fn handle_datagram(
        &self,
        socket: &Arc<UdpSocket>,
        data: &[u8],
        client: SocketAddr,
        state: &UdpState,
    ) {
        self.metrics.client_datagrams.inc();
        // Fast path: an existing flow forwards directly.
        let upstream_to_send = {
            let mut flows = self.flows.lock().expect("UDP flows lock poisoned");
            match flows.get_mut(&client) {
                Some(flow) => {
                    flow.last_active = Instant::now();
                    flow.c2u_packets += 1;
                    Some(flow.upstream.clone())
                }
                None => None,
            }
        };
        if let Some(upstream) = upstream_to_send {
            let _ = upstream.send(data).await;
            return;
        }

        // New flow: select an upstream and bind a connected socket (async, so
        // outside the table lock), then insert under the capacity bound.
        let Some(upstream_addr) = self.select(state, client) else {
            self.metrics.no_upstream_drops.inc();
            return;
        };
        let local_addr = if upstream_addr.is_ipv4() {
            SocketAddr::from(([0, 0, 0, 0], 0))
        } else {
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 0))
        };
        let usock = match UdpSocket::bind(local_addr).await {
            Ok(s) => s,
            Err(_) => {
                self.metrics.socket_errors.inc();
                return;
            }
        };
        if usock.connect(upstream_addr).await.is_err() {
            self.metrics.socket_errors.inc();
            return;
        }
        let usock = Arc::new(usock);

        let created = {
            let mut flows = self.flows.lock().expect("UDP flows lock poisoned");
            if flows.len() >= state.max_flows {
                // Deterministic eviction: the flow idle longest.
                self.metrics.capacity_evictions.inc();
                if let Some(victim) = flows
                    .iter()
                    .min_by_key(|(_, f)| f.last_active)
                    .map(|(k, _)| *k)
                {
                    flows.remove(&victim);
                }
            }
            if flows.len() >= state.max_flows {
                // Still full (the victim collided with this client); drop.
                self.metrics.capacity_evictions.inc();
                false
            } else {
                self.metrics.flows_created.inc();
                flows.insert(
                    client,
                    UdpFlow {
                        upstream: usock.clone(),
                        last_active: Instant::now(),
                        c2u_packets: 1,
                    },
                );
                true
            }
        };
        if !created {
            return;
        }
        let _ = usock.send(data).await;

        // Spawn the flow's reader: upstream responses -> client. It stops when
        // the flow is evicted (Weak lookup) or the server shuts down.
        self.spawn_flow_reader(socket.clone(), client, usock, state.idle_timeout);
    }

    /// Spawn the response path for one flow: read from the connected upstream
    /// socket and send the datagram back to the client via the listener socket.
    fn spawn_flow_reader(
        &self,
        listener_socket: Arc<UdpSocket>,
        client: SocketAddr,
        upstream: Arc<UdpSocket>,
        idle_timeout: Duration,
    ) {
        let flows = Arc::downgrade(&self.flows);
        let stop = self.stop.clone();
        let metrics = self.metrics.clone();
        let access_log = self.access_log.clone();
        let listener = self.listener.clone();
        let ts_ms = epoch_ms();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut u2c_packets = 0u64;
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // The recv is bounded by the idle timeout: a flow with no
                // traffic for that long is evicted (its socket released); a
                // still-active flow keeps waiting.
                let n = match tokio::time::timeout(idle_timeout, upstream.recv_from(&mut buf)).await
                {
                    Ok(Ok((n, _))) => n,
                    Ok(Err(_)) => break,
                    Err(_) => {
                        // Idle timeout fired: evict this flow if it has been
                        // idle long enough; a recently-active flow (e.g. only
                        // client traffic) keeps waiting.
                        let mut gone = false;
                        if let Some(flows) = flows.upgrade() {
                            let mut guard = flows.lock().expect("UDP flows lock poisoned");
                            match guard.get_mut(&client) {
                                Some(flow) => {
                                    if flow.last_active.elapsed() >= idle_timeout {
                                        guard.remove(&client);
                                        metrics.idle_evictions.inc();
                                        gone = true;
                                    }
                                }
                                None => gone = true,
                            }
                        } else {
                            gone = true;
                        }
                        if gone {
                            break;
                        }
                        continue;
                    }
                };
                if listener_socket.send_to(&buf[..n], client).await.is_err() {
                    break;
                }
                metrics.upstream_datagrams.inc();
                u2c_packets += 1;
                // Update the flow's activity and stop if it was evicted
                // concurrently (releasing the upstream socket).
                let mut gone = false;
                if let Some(flows) = flows.upgrade() {
                    let mut guard = flows.lock().expect("UDP flows lock poisoned");
                    match guard.get_mut(&client) {
                        Some(flow) => flow.last_active = Instant::now(),
                        None => gone = true,
                    }
                } else {
                    gone = true;
                }
                if gone {
                    break;
                }
            }
            // Emit a flow record (evicted/stopped), with best-effort packet
            // counts (the reader knows the upstream side; the client side is
            // read from the flow table if it still exists).
            let outcome = if stop.load(Ordering::Relaxed) {
                "shutdown"
            } else {
                "evicted"
            };
            if let Some(log) = &access_log {
                let upstream_addr = upstream
                    .peer_addr()
                    .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
                let client_packets = flows
                    .upgrade()
                    .map(|flows| {
                        flows
                            .lock()
                            .expect("UDP flows lock poisoned")
                            .get(&client)
                            .map(|f| f.c2u_packets)
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);
                log(&UdpFlowRecord {
                    ts_ms,
                    listener,
                    client,
                    upstream: upstream_addr,
                    client_packets,
                    upstream_packets: u2c_packets,
                    outcome,
                });
            }
        });
    }
}

/// Bind one UDP listener socket for an L4 listener address.
///
/// `reuse_port` sets `SO_REUSEPORT`, which is what lets several sockets share
/// the address so each receive loop has its own — and, critically, its own
/// kernel receive buffer. The kernel hashes each datagram's 4-tuple to a
/// socket, so every datagram of one client flow lands on the same loop. It is
/// only set when more than one loop is requested, so a single-loop listener
/// keeps the stricter "one bind wins" behaviour and a genuine port conflict is
/// still an error.
///
/// An unspecified IPv6 address is left dual-stack (`IPV6_V6ONLY` off) so one
/// listener serves both families, matching the TCP listeners. `recv_buffer` /
/// `send_buffer` of 0 mean "OS default". Returns a blocking `std` socket:
/// binding happens on the startup thread, before a Tokio runtime exists, so
/// reactor registration is deferred to [`BackgroundService::start`].
fn bind_udp_listener(
    address: SocketAddr,
    reuse_port: bool,
    recv_buffer: usize,
    send_buffer: usize,
) -> Result<std::net::UdpSocket, String> {
    let domain = if address.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
        .map_err(|e| format!("create UDP socket: {e}"))?;
    if reuse_port {
        socket
            .set_reuse_port(true)
            .map_err(|e| format!("set UDP reuseport: {e}"))?;
    }
    if let SocketAddr::V6(v6) = address {
        if v6.ip().is_unspecified() {
            socket
                .set_only_v6(false)
                .map_err(|e| format!("set UDP dual-stack: {e}"))?;
        }
    }
    // Buffer sizes are applied before bind so the first datagram already sees
    // the configured capacity.
    if recv_buffer > 0 {
        let _ = socket.set_recv_buffer_size(recv_buffer);
    }
    if send_buffer > 0 {
        let _ = socket.set_send_buffer_size(send_buffer);
    }
    socket
        .bind(&socket2::SockAddr::from(address))
        .map_err(|e| format!("failed to bind udp listener {address}: {e}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("failed to set udp listener {address} non-blocking: {e}"))?;
    Ok(socket.into())
}

#[cfg(unix)]
/// Atomically publish a bounded handoff manifest with private permissions.
fn write_handoff_file(path: &str, payload: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let temp = format!("{path}.tmp.{}", std::process::id());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| format!("create UDP handoff manifest: {e}"))?;
    file.write_all(payload)
        .map_err(|e| format!("write UDP handoff manifest: {e}"))?;
    file.sync_data()
        .map_err(|e| format!("sync UDP handoff manifest: {e}"))?;
    std::fs::rename(&temp, path).map_err(|e| format!("publish UDP handoff manifest: {e}"))?;
    Ok(())
}

#[cfg(unix)]
/// Receive one complete UDP handoff transaction from the old process.
///
/// `expected_listeners` is this process's receive-loop count. The transfer
/// fails closed when the outgoing process ran a different count, because
/// silently starting with fewer listener sockets than the port needs would
/// leave part of the `SO_REUSEPORT` group unserved.
fn receive_udp_handoff(
    upgrade_sock: &str,
    handoff_id: &str,
    expected_listeners: usize,
) -> Result<UdpHandoffState, String> {
    let manifest_path = format!("{upgrade_sock}.udp.{handoff_id}.manifest");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let manifest = loop {
        if let Ok(bytes) = std::fs::read(&manifest_path) {
            if bytes.len() > 16 * 1024 * 1024 {
                return Err("UDP handoff manifest exceeds 16 MiB".to_string());
            }
            break serde_json::from_slice::<UdpHandoffManifest>(&bytes)
                .map_err(|e| format!("parse UDP handoff manifest: {e}"))?;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "UDP handoff manifest {} did not arrive",
                manifest_path
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if manifest.chunks == 0 || manifest.chunks > 1_000_000 {
        return Err(format!(
            "invalid UDP handoff chunk count {}",
            manifest.chunks
        ));
    }
    if manifest.flows.len() > 30_000_000 {
        return Err("UDP handoff flow count exceeds the safety limit".to_string());
    }
    let expected_chunks = manifest
        .flows
        .len()
        .div_ceil(HANDOFF_FLOWS_PER_CHUNK)
        .max(1);
    if manifest.chunks != expected_chunks {
        return Err(format!(
            "UDP handoff chunk count {} does not match {} flows",
            manifest.chunks,
            manifest.flows.len()
        ));
    }
    // A manifest without the field came from a process that transferred a
    // single descriptor named `listener`.
    let sent_listeners = if manifest.listeners == 0 {
        1
    } else {
        manifest.listeners
    };
    if sent_listeners != expected_listeners {
        return Err(format!(
            "UDP handoff carries {sent_listeners} listener descriptors but this process runs \
             {expected_listeners} receive loops; restart instead of upgrading when the \
             worker-thread count changes"
        ));
    }
    let mut sockets: Vec<Option<Arc<UdpSocket>>> =
        (0..manifest.flows.len()).map(|_| None).collect();
    let mut listener_fds: Vec<std::os::fd::RawFd> = Vec::with_capacity(expected_listeners);
    for chunk in 0..manifest.chunks {
        let path = format!("{upgrade_sock}.udp.{handoff_id}.part.{chunk}");
        let mut fds = Fds::new();
        fds.get_from_sock(path.as_str())
            .map_err(|e| format!("receive UDP handoff chunk {chunk}: {e}"))?;
        if chunk == 0 {
            if manifest.listeners == 0 {
                let fd = fds
                    .get("listener")
                    .copied()
                    .ok_or("UDP handoff is missing the listener fd")?;
                listener_fds.push(fd);
            } else {
                for index in 0..expected_listeners {
                    let fd = fds
                        .get(&format!("listener-{index}"))
                        .copied()
                        .ok_or_else(|| {
                            format!("UDP handoff is missing listener descriptor {index}")
                        })?;
                    listener_fds.push(fd);
                }
            }
        }
        let start = chunk * HANDOFF_FLOWS_PER_CHUNK;
        let end = (start + HANDOFF_FLOWS_PER_CHUNK).min(manifest.flows.len());
        for (index, slot) in sockets.iter_mut().enumerate().take(end).skip(start) {
            let fd = fds
                .get(&format!("flow-{index}"))
                .copied()
                .ok_or_else(|| format!("UDP handoff is missing flow fd {index}"))?;
            // SAFETY: the fd was transferred through SCM_RIGHTS and is now
            // owned by this process.
            let std_socket = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
            std_socket
                .set_nonblocking(true)
                .map_err(|e| format!("set inherited UDP flow nonblocking: {e}"))?;
            let socket = UdpSocket::from_std(std_socket)
                .map_err(|e| format!("adopt inherited UDP flow socket: {e}"))?;
            let actual_upstream = socket
                .peer_addr()
                .map_err(|e| format!("read inherited UDP flow peer: {e}"))?;
            if actual_upstream != manifest.flows[index].upstream {
                return Err(format!(
                    "UDP handoff flow {index} points to {actual_upstream}, expected {}",
                    manifest.flows[index].upstream
                ));
            }
            *slot = Some(Arc::new(socket));
        }
    }
    if listener_fds.len() != expected_listeners {
        return Err(format!(
            "UDP handoff delivered {} of {expected_listeners} listener descriptors",
            listener_fds.len()
        ));
    }
    let listeners = listener_fds
        .into_iter()
        .map(|fd| {
            // SAFETY: the listener fd was transferred through SCM_RIGHTS and is
            // now owned by this process.
            let listener = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
            listener
                .set_nonblocking(true)
                .map_err(|e| format!("set inherited UDP listener nonblocking: {e}"))?;
            Ok(listener)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let flows = manifest
        .flows
        .into_iter()
        .enumerate()
        .map(|(index, flow)| {
            let socket = sockets[index]
                .take()
                .ok_or_else(|| format!("UDP handoff flow {index} was not received"))?;
            Ok((flow, socket))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let _ = std::fs::remove_file(&manifest_path);
    Ok((listeners, flows))
}

/// A deterministic (FNV-1a) hash of bytes for source-IP stickiness.
fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Resolve a UDP listener's upstreams (first address per host; per-address
/// fan-out is a follow-up).
fn resolve_udp_upstreams(udp: &UdpProxyConfig) -> Result<Vec<SocketAddr>, String> {
    let mut upstreams = Vec::with_capacity(udp.upstreams.len());
    for upstream in &udp.upstreams {
        let addrs = resolve_upstream(&upstream.host, upstream.port).map_err(|e| {
            format!(
                "udp listener {}: upstream {}: {e}",
                udp.listen.display(),
                upstream.display()
            )
        })?;
        let Some(addr) = addrs.first() else {
            return Err(format!(
                "udp listener {}: upstream {} resolved to no addresses",
                udp.listen.display(),
                upstream.display()
            ));
        };
        upstreams.push(*addr);
    }
    Ok(upstreams)
}

#[async_trait]
impl BackgroundService for UdpProxy {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        #[cfg(unix)]
        let inherited = if self.upgrade {
            match self.receive_handoff().await {
                Ok(value) => Some(value),
                Err(error) => {
                    tracing::error!(
                        "udp {}: failed to receive upgrade handoff: {error}",
                        self.listener
                    );
                    #[cfg(unix)]
                    {
                        let status = format!("error: {error}");
                        if let Err(status_error) =
                            write_handoff_file(&self.handoff_status_path(), status.as_bytes())
                        {
                            tracing::error!(
                                "udp {}: failed to publish receive failure: {status_error}",
                                self.listener
                            );
                        }
                    }
                    if self.upgrade {
                        std::process::exit(1);
                    }
                    return;
                }
            }
        } else {
            None
        };
        #[cfg(not(unix))]
        let inherited = None;

        #[cfg(unix)]
        if let Some((listeners, flows)) = inherited {
            *self.std_sockets.lock().expect("UDP socket lock poisoned") = listeners;
            let mut guard = self.app.flows.lock().expect("UDP flows lock poisoned");
            for (flow, upstream) in flows {
                guard.insert(
                    flow.client,
                    UdpFlow {
                        upstream,
                        last_active: Instant::now(),
                        c2u_packets: flow.client_packets,
                    },
                );
            }
        }

        // Convert the bound std sockets to tokio here, where a runtime exists.
        // They are cloned rather than consumed so the handoff can still send
        // the original descriptors during an upgrade.
        //
        // A failed clone is fatal rather than skipped: silently dropping one
        // socket would leave the listener running with fewer receive loops than
        // configured, which is exactly the under-provisioned state this fan-out
        // exists to avoid, and nothing would report it.
        let cloned: std::io::Result<Vec<std::net::UdpSocket>> = self
            .std_sockets
            .lock()
            .expect("UDP socket lock poisoned")
            .iter()
            .map(|socket| socket.try_clone())
            .collect();
        let std_sockets = match cloned {
            Ok(sockets) => sockets,
            Err(e) => {
                tracing::error!(
                    "udp {}: failed to clone a listener socket: {e}",
                    self.listener
                );
                if self.upgrade {
                    std::process::exit(1);
                }
                return;
            }
        };
        if std_sockets.is_empty() {
            tracing::error!("udp {}: listener sockets are unavailable", self.listener);
            if self.upgrade {
                std::process::exit(1);
            }
            return;
        }
        let mut sockets = Vec::with_capacity(std_sockets.len());
        for std_socket in std_sockets {
            match UdpSocket::from_std(std_socket) {
                Ok(socket) => sockets.push(Arc::new(socket)),
                Err(e) => {
                    tracing::error!("udp {}: failed to wrap listener socket: {e}", self.listener);
                    if self.upgrade {
                        std::process::exit(1);
                    }
                    return;
                }
            }
        }

        // Restart reader tasks for flows inherited from the old process. Any
        // listener socket can carry the reply — they share the bound address,
        // and `SO_REUSEPORT` only decides which socket *receives* — so the
        // first one is used.
        let inherited_flows: Vec<(SocketAddr, Arc<UdpSocket>)> = self
            .app
            .flows
            .lock()
            .expect("UDP flows lock poisoned")
            .iter()
            .map(|(client, flow)| (*client, flow.upstream.clone()))
            .collect();
        if !inherited_flows.is_empty() {
            let idle_timeout = self.app.current_state().idle_timeout;
            for (client, upstream) in inherited_flows {
                self.app
                    .spawn_flow_reader(sockets[0].clone(), client, upstream, idle_timeout);
            }
        }

        let (handoff_tx, mut handoff_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        #[cfg(unix)]
        if let Some(mut phase) = self
            .phase_watch
            .lock()
            .expect("UDP phase watch lock poisoned")
            .take()
        {
            let handoff_tx = handoff_tx.clone();
            tokio::spawn(async move {
                while let Ok(phase_value) = phase.recv().await {
                    if matches!(phase_value, ExecutionPhase::GracefulUpgradeTransferringFds) {
                        let _ = handoff_tx.send(());
                        break;
                    }
                }
            });
        }

        // One receive loop per `SO_REUSEPORT` socket. `stop` ends them all at
        // once, so shutdown and the upgrade handoff stay single-signal.
        // A `watch` channel rather than a `Notify`: `notify_waiters()` wakes
        // only waiters that are *already registered*, and a freshly spawned
        // loop registers nothing until it is first polled. If shutdown is
        // already signalled when this service starts, the parent below breaks
        // without ever yielding, the wake reaches nobody, and `worker.await`
        // hangs forever. A watch carries the value, so a receiver that
        // registers later still observes it.
        let (stop_tx, _stop_rx) = tokio::sync::watch::channel(false);
        let mut workers = Vec::with_capacity(sockets.len());
        for socket in sockets {
            let app = self.app.clone();
            let stop = stop_tx.subscribe();
            workers.push(tokio::spawn(
                async move { recv_loop(socket, app, stop).await },
            ));
        }

        // Wait for whichever ends the listener: shutdown, or the upgrade
        // handoff. The receive loops keep draining until `stop` is notified.
        tokio::select! {
            signal = handoff_rx.recv() => {
                if signal.is_some() {
                    // Stop the flow readers before transferring their
                    // descriptors so the replacement owns them exclusively.
                    self.app.stop.store(true, Ordering::Relaxed);
                    #[cfg(unix)]
                    if let Err(error) = self.send_handoff().await {
                        tracing::error!(
                            "udp {}: failed to send upgrade handoff: {error}",
                            self.listener
                        );
                    }
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

        let _ = stop_tx.send(true);
        for worker in workers {
            let _ = worker.await;
        }
        // Shutdown: stop flow readers, then close all flows.
        self.app.stop.store(true, Ordering::Relaxed);
        self.app
            .flows
            .lock()
            .expect("UDP flows lock poisoned")
            .clear();
        tracing::info!("udp {}: stopped", self.listener);
    }
}

/// Drain one listener socket until `stop` is notified.
///
/// Each `SO_REUSEPORT` socket gets one of these. The kernel hashes a datagram's
/// 4-tuple to pick the receiving socket, so every datagram of one client flow
/// lands on the same loop, and both the kernel receive buffer and the drain
/// rate scale with the worker-thread count rather than with one socket.
///
/// That scaling is the point: creating a flow means creating and connecting an
/// upstream socket, and datagrams that arrive on this socket while that work is
/// in progress are dropped by the kernel — counted as `UdpRcvbufErrors`, not by
/// any Raddex metric, because the process never sees them.
async fn recv_loop(
    socket: Arc<UdpSocket>,
    app: Arc<UdpApp>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    if *stop.borrow_and_update() {
        return;
    }
    let mut max = app.current_state().max_datagram_size;
    // Buffer one byte larger than the cap so an oversized datagram is
    // detectable (a full read means it was truncated / too big).
    let mut buf = vec![0u8; max.saturating_add(1)];
    loop {
        // The listener topology is fixed, but UDP limits are reloadable.
        // Resize before the next receive so a larger post-reload limit takes
        // effect instead of remaining stuck at the startup buffer size.
        let state = app.current_state();
        if state.max_datagram_size != max {
            max = state.max_datagram_size;
            buf.resize(max.saturating_add(1), 0);
        }
        let received = tokio::select! {
            biased;
            _ = stop.changed() => break,
            result = socket.recv_from(&mut buf) => result,
        };
        let (n, client) = match received {
            Ok(pair) => pair,
            Err(_) => {
                app.metrics.socket_errors.inc();
                continue;
            }
        };
        if n > max {
            app.metrics.oversized_drops.inc();
            continue;
        }
        app.handle_datagram(&socket, &buf[..n], client, &state)
            .await;
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

    /// An address the OS has just confirmed is free.
    fn free_addr() -> SocketAddr {
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("probe bind");
        probe.local_addr().expect("probe addr")
    }

    #[test]
    fn reuse_port_lets_every_receive_loop_own_a_socket() {
        // The fix for the udp_flows error rate depends on this: N receive loops
        // each need their own socket on the same address, because each socket
        // carries its own kernel receive buffer. One socket's buffer is capped
        // by net.core.rmem_max and drained by one task, so a burst of new flows
        // overflows it and the kernel drops the excess (UdpRcvbufErrors).
        let addr = free_addr();
        let first = bind_udp_listener(addr, true, 0, 0).expect("first reuseport bind");
        let second = bind_udp_listener(addr, true, 0, 0).expect("second reuseport bind");
        assert_eq!(
            first.local_addr().expect("addr"),
            second.local_addr().expect("addr")
        );
    }

    #[test]
    fn without_reuse_port_a_second_bind_still_conflicts() {
        // A single-loop listener must keep the stricter "one bind wins"
        // behaviour, so a genuine port conflict is still reported as an error
        // rather than silently sharing the port.
        let addr = free_addr();
        let _first = bind_udp_listener(addr, false, 0, 0).expect("first bind");
        assert!(bind_udp_listener(addr, false, 0, 0).is_err());
    }

    #[test]
    fn buffer_sizes_are_applied_when_configured() {
        // 0 means "OS default"; a non-zero request must at least not fail the
        // bind. The kernel may clamp the value to net.core.rmem_max, so the
        // exact size is deliberately not asserted.
        let addr = free_addr();
        let socket = bind_udp_listener(addr, false, 65_536, 65_536).expect("bind with buffers");
        assert_eq!(socket.local_addr().expect("addr"), addr);
    }
}
