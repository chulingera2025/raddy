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
//! A Tokio `UdpSocket` owns the listener; the datagram loop looks up or creates
//! a flow per client. Each flow has a *connected* upstream UDP socket whose
//! ephemeral local port demultiplexes upstream responses, plus a reader task
//! that sends those responses back to the client.
//!
//! Resources are bounded: the flow table is capped (max_flows, with
//! deterministic oldest-first eviction), oversized datagrams are dropped and
//! counted, and a flow idle for idle_timeout is evicted (its upstream socket
//! released). Selection (source-IP hash / round-robin / random) happens once
//! per new flow. A SIGHUP reload updates the upstream set for new flows
//! (existing flows keep their socket). On Linux, zero-downtime upgrades transfer
//! the listener, connected upstream sockets, and bounded flow metadata through
//! a dedicated handoff protocol.

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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    flows: Vec<UdpHandoffFlow>,
}

#[cfg(unix)]
type UdpHandoffState = (std::net::UdpSocket, Vec<(UdpHandoffFlow, Arc<UdpSocket>)>);

/// Keep the number of descriptors in every SCM_RIGHTS message below Pingora's
/// receiver limit, leaving room for the listener descriptor in chunk zero.
#[cfg(unix)]
const HANDOFF_FLOWS_PER_CHUNK: usize = 30;

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
        "raddy_l4_udp_client_datagrams_total",
        "UDP datagrams received from clients",
        &["listener"]
    )
    .expect("register counter vec")
});
static UPSTREAM_DATAGRAMS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddy_l4_udp_upstream_datagrams_total",
        "UDP datagrams sent to clients (upstream responses)",
        &["listener"]
    )
    .expect("register counter vec")
});
static FLOWS_CREATED: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddy_l4_udp_flows_created_total",
        "UDP flows created",
        &["listener"]
    )
    .expect("register counter vec")
});
static CAPACITY_EVICTIONS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddy_l4_udp_capacity_evictions_total",
        "UDP flows evicted because the table was full",
        &["listener"]
    )
    .expect("register counter vec")
});
static IDLE_EVICTIONS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddy_l4_udp_idle_evictions_total",
        "UDP flows evicted because they were idle too long",
        &["listener"]
    )
    .expect("register counter vec")
});
static OVERSIZED_DROPS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddy_l4_udp_oversized_drops_total",
        "UDP datagrams dropped because they exceeded max_datagram_size",
        &["listener"]
    )
    .expect("register counter vec")
});
static NO_UPSTREAM_DROPS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddy_l4_udp_no_upstream_drops_total",
        "UDP datagrams dropped because no upstream could be selected",
        &["listener"]
    )
    .expect("register counter vec")
});
static SOCKET_ERRORS: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
    prometheus::register_int_counter_vec!(
        "raddy_l4_udp_socket_errors_total",
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

/// The UDP proxy background service for one listener.
pub struct UdpProxy {
    listener: String,
    /// The bound std socket. Converted to tokio in start (where a runtime
    /// exists); normal processes bind in new, while upgrade replacements
    /// receive the socket during startup.
    std_socket: Mutex<Option<std::net::UdpSocket>>,
    /// True when this process must receive the UDP socket and flow table from
    /// the old process during a Pingora upgrade.
    upgrade: bool,
    #[cfg(unix)]
    upgrade_sock: String,
    #[cfg(unix)]
    phase_watch: Mutex<Option<tokio::sync::broadcast::Receiver<ExecutionPhase>>>,
    listen: ListenAddress,
    config_store: Arc<ConfigStore>,
    /// Reload-updatable selection state.
    state: Mutex<UdpState>,
    /// Bounded flow table keyed by client address. `Arc` so flow reader tasks
    /// hold a `Weak` to detect when their flow is evicted. A single lock is a
    /// P2 simplification; the critical sections are short (lookup + insert).
    flows: Arc<Mutex<HashMap<SocketAddr, UdpFlow>>>,
    /// Set on shutdown so flow reader tasks stop and release their sockets.
    stop: Arc<AtomicBool>,
    /// Round-robin / random cursor (persists across reloads and datagrams).
    cursor: AtomicUsize,
    metrics: Arc<UdpMetrics>,
    access_log: Option<Arc<AccessLogSink>>,
}

impl UdpProxy {
    /// Bind the listener socket and build the proxy. The socket is bound here
    /// (at startup) so a failed bind is a startup error, like TCP listeners.
    pub fn new(
        udp: &UdpProxyConfig,
        config_store: Arc<ConfigStore>,
        access_log: Option<Arc<AccessLogSink>>,
        upgrade: bool,
        #[cfg(unix)] upgrade_sock: String,
        #[cfg(unix)] phase_watch: Option<tokio::sync::broadcast::Receiver<ExecutionPhase>>,
    ) -> Result<Self, String> {
        let listener = format!("udp/{}", udp.listen.display());
        let mut socket = if upgrade {
            None
        } else {
            let socket = std::net::UdpSocket::bind(udp.listen.display()).map_err(|e| {
                format!("failed to bind udp listener {}: {e}", udp.listen.display())
            })?;
            socket.set_nonblocking(true).map_err(|e| {
                format!(
                    "failed to set udp listener {} non-blocking: {e}",
                    udp.listen.display()
                )
            })?;
            Some(socket)
        };
        // Configure socket buffers via socket2 (0 = OS default); `std::net`
        // no longer exposes the buffer setters directly.
        if udp.recv_buffer > 0 || udp.send_buffer > 0 {
            if let Some(bound) = socket.take() {
                let sock = socket2::Socket::from(bound);
                if udp.recv_buffer > 0 {
                    let _ = sock.set_recv_buffer_size(udp.recv_buffer);
                }
                if udp.send_buffer > 0 {
                    let _ = sock.set_send_buffer_size(udp.send_buffer);
                }
                socket = Some(sock.into());
            }
        }
        let upstreams = resolve_udp_upstreams(udp)?;
        let state = Mutex::new(UdpState {
            configured: udp.upstreams.clone(),
            upstreams,
            policy: udp.lb_policy,
            idle_timeout: udp.idle_timeout,
            max_flows: udp.max_flows,
            max_datagram_size: udp.max_datagram_size,
        });
        Ok(Self {
            cursor: AtomicUsize::new(0),
            listener,
            std_socket: Mutex::new(socket),
            upgrade,
            #[cfg(unix)]
            upgrade_sock,
            #[cfg(unix)]
            phase_watch: Mutex::new(phase_watch),
            listen: udp.listen.clone(),
            config_store,
            state,
            flows: Arc::new(Mutex::new(HashMap::new())),
            stop: Arc::new(AtomicBool::new(false)),
            metrics: UdpMetrics::register(&format!("udp/{}", udp.listen.display())),
            access_log,
        })
    }

    /// Return the manifest path used by the UDP upgrade handoff.
    pub fn handoff_manifest_path(upgrade_sock: &str) -> String {
        format!("{upgrade_sock}.udp.manifest")
    }

    /// Return the Unix socket path used for one UDP handoff descriptor chunk.
    pub fn handoff_part_path(upgrade_sock: &str, chunk: usize) -> String {
        format!("{upgrade_sock}.udp.part.{chunk}")
    }

    /// The current selection state, rebuilt on reload when the upstream set or
    /// policy changed (new flows use it; existing flows keep their socket).
    fn current_state(&self) -> UdpState {
        let mut state = self.state.lock().expect("UDP state lock poisoned");
        if let Some(udp) = self
            .config_store
            .load()
            .layer4
            .iter()
            .find_map(|l| match l {
                Layer4Listener::Udp(udp) if udp.listen == self.listen => Some(udp),
                _ => None,
            })
        {
            let spec_changed = udp.upstreams != state.configured
                || udp.lb_policy != state.policy
                || udp.idle_timeout != state.idle_timeout
                || udp.max_flows != state.max_flows
                || udp.max_datagram_size != state.max_datagram_size;
            if spec_changed {
                if let Ok(upstreams) = resolve_udp_upstreams(udp) {
                    tracing::info!("udp {}: upstream set changed on reload", self.listener);
                    state.configured = udp.upstreams.clone();
                    state.upstreams = upstreams;
                    state.policy = udp.lb_policy;
                    state.idle_timeout = udp.idle_timeout;
                    state.max_flows = udp.max_flows;
                    state.max_datagram_size = udp.max_datagram_size;
                }
            }
        }
        UdpState {
            configured: state.configured.clone(),
            upstreams: state.upstreams.clone(),
            policy: state.policy,
            idle_timeout: state.idle_timeout,
            max_flows: state.max_flows,
            max_datagram_size: state.max_datagram_size,
        }
    }

    #[cfg(unix)]
    /// Transfer the listener and all connected upstream flow sockets to the
    /// replacement process. The manifest is written before descriptor chunks,
    /// so the receiver knows exactly how many bounded SCM_RIGHTS messages to
    /// await.
    async fn send_handoff(&self) -> Result<(), String> {
        let (listener_fd, flow_fds, manifest) = {
            let socket_guard = self.std_socket.lock().expect("UDP socket lock poisoned");
            let socket = socket_guard
                .as_ref()
                .ok_or("UDP listener socket is unavailable")?;
            let flows = self.flows.lock().expect("UDP flows lock poisoned");
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
            (
                socket.as_raw_fd(),
                fds,
                UdpHandoffManifest {
                    chunks: chunks.max(1),
                    flows: metadata,
                },
            )
        };
        let manifest_path = Self::handoff_manifest_path(&self.upgrade_sock);
        let payload = serde_json::to_vec(&manifest)
            .map_err(|e| format!("serialize UDP handoff manifest: {e}"))?;
        write_handoff_manifest(&manifest_path, &payload)?;

        let upgrade_sock = self.upgrade_sock.clone();
        tokio::task::spawn_blocking(move || {
            for chunk in 0..manifest.chunks {
                let start = chunk * HANDOFF_FLOWS_PER_CHUNK;
                let end = (start + HANDOFF_FLOWS_PER_CHUNK).min(manifest.flows.len());
                let mut fds = Fds::new();
                if chunk == 0 {
                    fds.add("listener".to_string(), listener_fd);
                }
                for (index, fd) in flow_fds.iter().enumerate().take(end).skip(start) {
                    fds.add(format!("flow-{index}"), *fd);
                }
                let path = Self::handoff_part_path(&upgrade_sock, chunk);
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
    /// Receive the inherited UDP listener and connected flow sockets.
    async fn receive_handoff(&self) -> Result<UdpHandoffState, String> {
        let upgrade_sock = self.upgrade_sock.clone();
        tokio::task::spawn_blocking(move || receive_udp_handoff(&upgrade_sock))
            .await
            .map_err(|e| format!("UDP handoff receiver failed: {e}"))?
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
    async fn handle_datagram(&self, socket: &Arc<UdpSocket>, data: &[u8], client: SocketAddr) {
        let state = self.current_state();
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
        let Some(upstream_addr) = self.select(&state, client) else {
            self.metrics.no_upstream_drops.inc();
            return;
        };
        let usock = match UdpSocket::bind(("0.0.0.0", 0)).await {
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

#[cfg(unix)]
/// Atomically publish a bounded handoff manifest with private permissions.
fn write_handoff_manifest(path: &str, payload: &[u8]) -> Result<(), String> {
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
fn receive_udp_handoff(upgrade_sock: &str) -> Result<UdpHandoffState, String> {
    let manifest_path = UdpProxy::handoff_manifest_path(upgrade_sock);
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
    let mut sockets: Vec<Option<Arc<UdpSocket>>> =
        (0..manifest.flows.len()).map(|_| None).collect();
    let mut listener_fd = None;
    for chunk in 0..manifest.chunks {
        let path = UdpProxy::handoff_part_path(upgrade_sock, chunk);
        let mut fds = Fds::new();
        fds.get_from_sock(path.as_str())
            .map_err(|e| format!("receive UDP handoff chunk {chunk}: {e}"))?;
        if chunk == 0 {
            listener_fd = fds.get("listener").copied();
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
    let listener_fd = listener_fd.ok_or("UDP handoff is missing the listener fd")?;
    // SAFETY: the listener fd was transferred through SCM_RIGHTS and is now
    // owned by this process.
    let listener = unsafe { std::net::UdpSocket::from_raw_fd(listener_fd) };
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set inherited UDP listener nonblocking: {e}"))?;
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
    Ok((listener, flows))
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
                    return;
                }
            }
        } else {
            None
        };
        #[cfg(not(unix))]
        let inherited = None;

        #[cfg(unix)]
        if let Some((listener, flows)) = inherited {
            *self.std_socket.lock().expect("UDP socket lock poisoned") = Some(listener);
            let mut guard = self.flows.lock().expect("UDP flows lock poisoned");
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

        // Convert the bound std socket to tokio here, where a runtime exists.
        let std_socket = self
            .std_socket
            .lock()
            .expect("UDP socket lock poisoned")
            .as_ref()
            .and_then(|socket| socket.try_clone().ok());
        let socket = match std_socket {
            Some(s) => match UdpSocket::from_std(s) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    tracing::error!("udp {}: failed to wrap listener socket: {e}", self.listener);
                    return;
                }
            },
            None => {
                tracing::error!("udp {}: listener socket is unavailable", self.listener);
                return;
            }
        };
        let inherited_clients: Vec<SocketAddr> = self
            .flows
            .lock()
            .expect("UDP flows lock poisoned")
            .keys()
            .copied()
            .collect();
        for client in inherited_clients {
            let upstream = self
                .flows
                .lock()
                .expect("UDP flows lock poisoned")
                .get(&client)
                .map(|flow| flow.upstream.clone());
            if let Some(upstream) = upstream {
                self.spawn_flow_reader(
                    socket.clone(),
                    client,
                    upstream,
                    self.current_state().idle_timeout,
                );
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
        let mut max = self.current_state().max_datagram_size;
        // Buffer one byte larger than the cap so an oversized datagram is
        // detectable (a full read means it was truncated / too big).
        let mut buf = vec![0u8; max.saturating_add(1)];
        loop {
            // The listener topology is fixed, but UDP limits are reloadable.
            // Resize before the next receive so a larger post-reload limit is
            // effective instead of remaining stuck at the startup buffer size.
            let current_max = self.current_state().max_datagram_size;
            if current_max != max {
                max = current_max;
                buf.resize(max.saturating_add(1), 0);
            }
            tokio::select! {
                signal = shutdown.changed() => {
                    if signal.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                signal = handoff_rx.recv() => {
                    if signal.is_some() {
                        self.stop.store(true, Ordering::Relaxed);
                        #[cfg(unix)]
                        if let Err(error) = self.send_handoff().await {
                            tracing::error!("udp {}: failed to send upgrade handoff: {error}", self.listener);
                        }
                        break;
                    }
                }
                r = socket.recv_from(&mut buf) => {
                    let (n, client) = match r {
                        Ok(x) => x,
                        Err(_) => {
                            self.metrics.socket_errors.inc();
                            continue;
                        }
                    };
                    if n > max {
                        self.metrics.oversized_drops.inc();
                        continue;
                    }
                    self.handle_datagram(&socket, &buf[..n], client).await;
                }
            }
        }
        // Shutdown: stop flow readers, then close all flows.
        self.stop.store(true, Ordering::Relaxed);
        self.flows.lock().expect("UDP flows lock poisoned").clear();
        tracing::info!("udp {}: stopped", self.listener);
    }
}

/// The current wall clock in epoch milliseconds.
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
