# Layer 4 Proxy Implementation Plan

Status: Implemented in `v0.3.0`

This document is the implementation and acceptance record for TCP and UDP layer
4 proxying in raddy. It records the architecture, configuration boundary,
runtime semantics, delivery order, and post-release follow-up work.

The P0 raw-TCP, P1 SNI passthrough, and P2 bounded-UDP runtime paths are present
in the released tree. Integration coverage includes TCP admission, half-close
and failed-upstream paths, plus UDP oversize drops, capacity eviction, reload,
IPv6, and typed flow access records. Post-release work is limited to independent
security/operations review and reproducible direct-versus-proxy benchmarks;
neither changes the documented UDP upgrade limitation.

## Decision summary

raddy can add layer 4 proxying without replacing its Pingora-based HTTP stack.
Pingora's HTTP proxy abstraction is not a generic layer 4 proxy, but the locked
Pingora 0.8.1 APIs provide a transport-level `ServerApp` over an accepted
`AsyncRead + AsyncWrite` stream. That is sufficient for raw TCP services.

Pingora 0.8.1 does not provide a UDP listening abstraction. UDP must therefore
be implemented with Tokio `UdpSocket`. A Pingora `BackgroundService` can
supervise startup and shutdown, but Tokio owns the UDP socket and datagram loop.

The target runtime is:

```text
                              raddy
                                |
                 +--------------+--------------+
                 |              |              |
              HTTP/L7         TCP/L4         UDP/L4
                 |              |              |
          Pingora HttpProxy  Pingora         Tokio
                             ServerApp       UdpSocket
                 |              |              |
             HTTP/HTTPS        TCP            UDP
```

Delivery order is intentionally asymmetric:

1. P0: production-capable raw TCP proxying.
2. P1: bounded TLS ClientHello inspection for SNI-based TCP routing.
3. P2: bounded, session-aware UDP proxying.

UDP is not a small extension of TCP. Its flow ownership, response demultiplexing,
resource bounds, eviction, and upgrade behavior form a separate subsystem.

## Compatibility baseline

The initial implementation targets the versions currently locked by the
repository:

- Pingora 0.8.1.
- Tokio 1.53.1 with `io-util`, `net`, `sync`, `time`, and runtime features.

Before adding or changing a dependency, verify the latest stable release and its
current API as required by the repository development policy. Do not hand-roll a
TLS ClientHello parser when a maintained, fuzzed parser is available.

## Architectural boundaries

### HTTP remains an independent subsystem

The existing HTTP pipeline retains ownership of HTTP concepts:

- `SiteKey` and host matching.
- `TerminalKind` and route terminals.
- HTTP request and response modifiers.
- HTTP access log fields, status codes, headers, rate limits, and compression.
- ACME and TLS termination.

Raw TCP and UDP configuration must not be represented as new `TerminalKind`
variants. Doing so would spread transport checks through HTTP-only modules and
make both interfaces shallower and harder to validate.

### Layer 4 owns transport concepts

The new subsystem owns:

- Listener identity and bind planning.
- Upstream endpoint resolution and selection.
- TCP connection and UDP flow limits.
- Transport timeouts and shutdown behavior.
- Layer 4 metrics and structured access records.
- Optional protocol inspection that does not terminate the protocol.

Recommended module layout:

```text
src/layer4/
  mod.rs
  config.rs
  listener.rs
  upstream.rs
  balance.rs
  metrics.rs
  access_log.rs
  tcp/
    mod.rs
    app.rs
    relay.rs
    health.rs
    tls_inspector.rs
  udp/
    mod.rs
    service.rs
    flow.rs
    table.rs
```

Each module should expose a small policy-oriented interface. Socket loops,
activity accounting, eviction details, and protocol parsing remain internal.

## Domain model

Use the following terms consistently in code, configuration, logs, and docs:

- **Listener**: one bound local endpoint and one transport.
- **Listener key**: the normalized `(transport, listen address)` identity.
- **Upstream**: one configured destination endpoint and its metadata.
- **Backend set**: the upstreams available to one listener or SNI route.
- **TCP connection**: one accepted client stream and its selected upstream
  stream.
- **UDP flow**: the state that maps one client on one listener to one selected
  upstream and its connected upstream socket.
- **Relay outcome**: the normalized reason a TCP connection or UDP flow ended.
- **Inspector**: a bounded parser that reads only enough application bytes to
  select a route, then forwards those original bytes unchanged.

The target compiled model is:

```rust
struct CompiledConfig {
    global: GlobalConfig,
    http: HttpConfig,
    layer4: Vec<Layer4Listener>,
}

enum Layer4Listener {
    Tcp(TcpProxyConfig),
    Udp(UdpProxyConfig),
}

struct ListenerKey {
    transport: SocketTransport,
    address: ListenAddress,
}

enum SocketTransport {
    Tcp,
    Udp,
}

enum ListenAddress {
    Socket(SocketAddr),
    // Reserved for a later TCP Unix-domain listener milestone.
    Unix(PathBuf),
}
```

`TcpProxyConfig` and `UdpProxyConfig` must be separate types. Shared fields may
use small common value objects, but there must not be a large struct containing
`Option<TcpOptions>` and `Option<UdpOptions>`.

An upstream should retain its configured endpoint rather than being reduced to
one `SocketAddr` in the parser. Resolution, multiple returned addresses, health,
and reload behavior belong to the runtime upstream module.

## Configuration interface

Add `tcp` and `udp` as distinct top-level Raddyfile blocks. They are peers of an
HTTP site block, not directives inside one.

```raddyfile
tcp :3306 {
    to db-1.internal:3306 db-2.internal:3306
    lb_policy round_robin
    connect_timeout 3s
    idle_timeout 5m
    max_connections 10000

    health_check {
        interval 10s
        timeout 2s
    }
}

udp :53 {
    to 1.1.1.1:53 8.8.8.8:53
    lb_policy source_ip_hash
    idle_timeout 30s
    max_flows 50000
    max_datagram_size 4096
    recv_buffer 4MiB
    send_buffer 4MiB
}
```

Initial syntax requirements:

- Accept IPv4 and bracketed IPv6 socket addresses.
- Permit TCP and UDP to bind the same address and port.
- Reject two listeners whose socket ownership overlaps for the same transport.
  Validation must account for wildcard and dual-stack binds, not only exact key
  equality.
- Require at least one upstream and reject zero durations and zero limits where
  they would disable forward progress.
- Use explicit byte and duration parsers with overflow checks.
- Resolve hostnames outside the parser through the upstream runtime.
- Keep Unix-domain sockets outside the first release even though the address
  enum reserves a clean extension point.

The Raddyfile specification and both language documentation sets must be updated
in the same change that exposes a directive.

## Shared upstream selection

The current load-balancer pool is keyed by HTTP `SiteKey` and terminal index. It
must not be called directly from layer 4 code. Extract a protocol-neutral
backend-set interface and keep HTTP-specific identity in an adapter.

The layer 4 selector must accept:

```rust
trait BackendSelector {
    fn select(&self, key: Option<&[u8]>) -> Option<SelectedBackend>;
}
```

The concrete interface may differ, but it must preserve these semantics:

- Selection excludes unhealthy backends.
- Round-robin and random operate over backend identities, not only addresses.
- Source-IP hashing includes the complete backend identity, so two TLS identities
  sharing one address remain independently selectable and sticky.
- A TCP backend is selected once per connection.
- A UDP backend is selected once when a flow is created, never once per packet.
- Existing connections and flows retain their selected backend across a reload.

Literal IP endpoints require no resolver. Hostname endpoints must resolve all
usable A and AAAA results at startup, refresh them according to resolver TTLs,
and atomically replace the selectable address set. A transient refresh failure
keeps the last-known-good addresses and emits a metric; it does not discard a
working backend set.

TCP connect health checks are meaningful generically and belong in P0. Generic
UDP health checks are not: receiving no response may be valid for an arbitrary
UDP protocol. Active UDP health checks are deferred until the configuration can
describe a protocol-specific probe and expected response.

## P0: raw TCP proxy

### Runtime design

Create one Pingora listening `Service<ServerApp>` per raw TCP listener. This
keeps listener identity and per-listener policy explicit at the application
boundary. Pingora owns accept, socket options, shutdown notification, and
inherited TCP listener handling. The layer 4 app owns upstream selection,
connect, relay, accounting, and connection limits.

The relay boundary should be one deep interface:

```rust
async fn relay_tcp(
    client: Stream,
    upstream: TcpStream,
    policy: RelayPolicy,
    shutdown: ShutdownWatch,
) -> RelayOutcome;
```

`tokio::io::copy_bidirectional` proves the transport path is feasible, but it is
not by itself the production interface. The relay must additionally implement:

- Independent client-to-upstream and upstream-to-client byte accounting.
- A true inactivity timeout reset by traffic in either direction. Wrapping the
  entire relay in `tokio::time::timeout` would incorrectly impose a maximum
  connection lifetime.
- TCP half-close propagation without truncating the other direction.
- Cancellation on forced shutdown after the configured drain period.
- Stable outcome classification for logs and metrics.
- Bounded task and buffer use.

Connection admission uses a per-listener semaphore. The permit is acquired
before upstream connect and held until the relay ends. Rejection due to the limit
is explicit in metrics and logs.

### TCP startup and shutdown

Startup must fail atomically when a required listener cannot bind or no upstream
can be resolved. A transiently unhealthy upstream does not prevent startup when
another usable upstream exists.

Graceful shutdown proceeds in this order:

1. Stop accepting new connections.
2. Notify active relays to drain.
3. Allow existing bidirectional relays to finish within the server grace period.
4. Cancel remaining relays and record `shutdown_timeout` outcomes.

The existing zero-downtime upgrade path must be verified to hand raw TCP
listener file descriptors to the new process. Existing accepted connections stay
with the old process and drain; new connections go to the new process.

### TCP observability

Emit one structured JSON access record when a connection closes. Required fields:

- Timestamp, listener identity, client address, selected upstream.
- Connection and relay duration.
- Bytes sent in each direction.
- Connect latency.
- Relay outcome and error class.

Required metrics:

- Accepted, rejected, active, and completed connections.
- Upstream connect attempts, failures, and latency.
- Idle timeouts and shutdown cancellations.
- Bytes in each direction.
- Backend health state.

HTTP common-log format is not extended to layer 4. A shared sink/rotation layer
may be reused, but HTTP and layer 4 records remain distinct typed events.

### P0 implementation sequence

1. Add the layer 4 AST, parser blocks, validation, and compiled model.
2. Add global listener planning and cross-protocol bind conflict validation.
3. Extract the protocol-neutral backend identity and selector.
4. Implement TCP endpoint resolution and active connect health checks.
5. Implement the Pingora `ServerApp`, admission limit, upstream connect timeout,
   and relay module.
6. Integrate TCP services into startup, graceful shutdown, reload validation, and
   zero-downtime upgrade.
7. Add typed access records, metrics, documentation, and examples.
8. Add unit, integration, upgrade, and failure-path tests.

P0 is complete only when the TCP acceptance criteria below pass in CI and the
upgrade test demonstrates both new connection acceptance and old connection
draining.

## P1: TLS passthrough and SNI routing

TLS passthrough is an optional TCP inspector, not TLS termination. It must never
interact with the HTTP certificate store, ACME, or HTTP TLS callbacks.

The inspector performs this sequence:

1. Read into a bounded prefix buffer until a complete ClientHello is available.
2. Extract SNI without modifying the message.
3. Select the configured backend set or fallback behavior.
4. Connect upstream.
5. Write the exact buffered bytes to upstream.
6. Enter the ordinary raw TCP relay.

Do not reconstruct or synthesize a ClientHello. Do not depend on OS-level
`peek`, because Pingora exposes a boxed transport stream rather than a portable
peekable `TcpStream`.

The inspector must handle fragmented TLS records and ClientHello messages, an
absent SNI, malformed input, read timeout, and a configurable maximum inspection
size. Parser code and all framing boundaries require fuzz coverage.

The P1 configuration design must define these outcomes explicitly:

- Exact SNI match.
- No SNI.
- Unknown SNI.
- Malformed or oversized ClientHello.
- Inspection timeout.

The safe default is to use an explicitly configured fallback backend; otherwise
close the connection and record the reason. Wildcard SNI patterns and PROXY
protocol support are separate follow-up features and are not required for the
first SNI-routing release.

## P2: UDP proxy

### Runtime design

Implement UDP with Tokio `UdpSocket` inside a Pingora `BackgroundService` so it
participates in common startup and shutdown. This does not mean Pingora owns or
accepts the UDP listener.

For a fixed generic UDP listener, the canonical flow key is:

```rust
struct UdpFlowKey {
    listener: ListenerKey,
    client: SocketAddr,
}
```

The selected upstream belongs in the flow value, not necessarily the key. A
full five-tuple becomes necessary only for transparent proxying, original
destination routing, or another mode in which the destination varies per packet.
Those modes are outside this plan.

Each flow owns or leases a connected upstream UDP socket. The socket's ephemeral
local port provides unambiguous upstream-response demultiplexing, while the flow
stores the original client address for replies. This design makes file descriptor
and flow limits mandatory.

```text
client datagram
      |
      v
listener socket -> flow lookup/create -> connected upstream socket
      ^                                      |
      +----------- client response ----------+
```

### Flow table invariants

The flow table must be bounded and sharded or otherwise avoid one global lock.
It owns:

- Idle deadline and last-activity tracking.
- Selected backend and connected upstream socket.
- Packet and byte counters in each direction.
- Cancellation and expiry state.
- A stable eviction reason.

Required controls:

- Maximum flows per listener and a process-wide safety ceiling.
- Idle timeout with efficient deadline management rather than full-table scans on
  every packet.
- Deterministic eviction behavior when capacity is reached.
- Maximum accepted datagram size and explicit oversized-packet accounting. The
  receive path must use an API that reports truncation instead of silently
  accepting a truncated `recv_from` buffer as a complete datagram.
- Configurable receive and send socket buffers with validated upper bounds.
- Bounded queues or explicit dropping when consumers cannot keep up.

Source-IP hash uses the client IP as its key, while flow identity still includes
the client port. A selected upstream remains stable for the flow's lifetime.

### UDP shutdown and upgrade

Graceful shutdown stops accepting new client datagrams, allows a short bounded
response-drain period, then closes all flows and records their outcomes.

The first UDP release must not claim lossless zero-downtime upgrade. A Pingora
background service does not automatically transfer its Tokio UDP listener, and
transferring only the listener would not preserve connected upstream sockets or
flow ownership. `SO_REUSEPORT` alone is not a flow-state handoff mechanism.

Until listener and flow-state transfer is designed, UDP-enabled configurations
must use a documented restart path for upgrades, with an explicit warning that
active flows are reset. A later milestone may add coordinated UDP file descriptor
and flow handoff; it is not part of P2 acceptance.

### UDP observability

Emit one structured flow record on expiry or eviction with listener, client,
upstream, duration, packets, bytes, and outcome. Required metrics include:

- Client and upstream datagrams and bytes.
- Active and created flows.
- Capacity, idle, shutdown, and error evictions.
- Oversized, queue-full, malformed, and socket-error drops.
- Upstream selection failures.

### P2 implementation sequence

1. Add UDP configuration and listener validation.
2. Implement the Tokio background service and socket option validation.
3. Implement the connected-upstream flow object and response path.
4. Implement the bounded flow table, deadline management, and eviction policy.
5. Integrate backend selection once per new flow.
6. Add shutdown behavior, explicit upgrade limitations, metrics, and flow logs.
7. Add concurrency, resource-bound, and packet-loss-path tests.

## Reload semantics

SIGHUP reload remains a configuration-snapshot reload, not a listener-topology
reload.

- If the normalized listener key set changes, reject the reload with an error
  that directs the operator to restart or use the supported upgrade path.
- New TCP connections use the new upstream set, policy, limits, and timeouts.
- Existing TCP connections retain their selected upstream and relay policy.
- New UDP flows use the new upstream set and policy.
- Existing UDP flows retain their upstream socket until expiry or shutdown.
- Removing an upstream stops new selection but does not terminate an existing
  connection or flow solely because of reload.
- Lowering a connection or flow limit does not kill existing work. New admissions
  remain blocked until usage falls below the new limit.

Listener topology may become dynamically reloadable in a separate design. It is
not required by this plan.

## Test plan

### Configuration tests

- Parse and compile valid TCP and UDP blocks.
- Reject missing upstreams, invalid addresses, invalid durations, and overflow.
- Allow TCP and UDP on the same address and port.
- Reject duplicate and overlapping binds for the same socket transport.
- Reject HTTP/raw-TCP bind collisions.
- Round-trip or snapshot the compiled TCP and UDP models without exposing secrets.

### TCP tests

- Bidirectional echo and large transfers.
- Client and upstream half-close behavior.
- True idle timeout while long-lived active traffic continues.
- Connect timeout and refused upstream classification.
- Per-listener connection limit and permit release on every exit path.
- Round-robin, random, and source-IP sticky selection.
- Same-address/different-identity source-IP stickiness.
- Health removal and recovery without dropping existing connections.
- DNS refresh replaces selectable addresses and preserves last-known-good state
  during a transient resolver failure.
- Reload affects only new connections.
- Shutdown drains existing connections and cancels after the grace period.
- Zero-downtime upgrade accepts new connections while old connections drain.

### TLS inspector tests

- Fragmented TLS records and fragmented ClientHello messages.
- Exact preservation and forwarding of every buffered input byte.
- Known SNI, unknown SNI, absent SNI, fallback, and reject behavior.
- Malformed, oversized, non-TLS, and timeout inputs.
- Fuzz target for framing and ClientHello parsing.

### UDP tests

- Multiple clients receive only their own upstream responses.
- One flow remains pinned to one upstream.
- Idle expiry and activity-based deadline refresh.
- Capacity eviction and file descriptor bounds.
- Oversized datagram and backpressure drop accounting.
- Reload affects only new flows.
- Shutdown stops new flows and closes existing flows after the drain period.
- IPv4 and IPv6 listener/upstream combinations.

### Quality gates

Every phase must pass the existing repository gates:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Add reproducible direct-versus-proxy TCP and UDP benchmarks before declaring the
respective subsystem production-ready. Record throughput, latency, CPU, memory,
connection/flow count, and packet or byte loss; do not make an unstable absolute
QPS value a CI gate.

## Deferred scope

The following features require separate designs and are not hidden requirements
of P0-P2:

- Layer 4 TLS termination or ACME integration.
- Generic protocol inspection beyond TLS ClientHello SNI.
- Transparent proxying and original-destination recovery.
- QUIC-aware proxying.
- Protocol-specific UDP active health checks.
- Lossless UDP process upgrades.
- Dynamic listener topology changes through SIGHUP.
- Unix datagram sockets.
- PROXY protocol v1/v2 ingress or egress.
- Wildcard SNI routing.

## Definition of done

The layer 4 initiative is complete when:

- The HTTP pipeline has no transport-condition branches for TCP or UDP behavior.
- Configuration validation prevents ambiguous or conflicting socket ownership.
- TCP limits, timeout, health, reload, shutdown, and upgrade guarantees are
  demonstrated by automated tests.
- TLS inspection is bounded, fuzzed, and forwards the original ClientHello bytes.
- UDP memory, tasks, file descriptors, queues, and flow lifetime are all bounded.
- UDP upgrade limitations are explicit in CLI output and operations docs.
- Metrics and structured logs explain every rejection, timeout, eviction, and
  abnormal relay termination without recording payload data.
- English and Chinese user documentation describe only behavior that the tests
  enforce.
