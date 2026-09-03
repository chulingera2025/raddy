# Layer 4 architecture and operations

The filename is retained as a compatibility path for existing links. This file
replaces the historical implementation plan with the current runtime contract
for TCP and UDP listeners. The user-facing syntax is documented in the
[Layer 4 guide](https://chulingera2025.github.io/raddex/guides/layer4/); this
document records the invariants that should remain true when the subsystem
changes.

## Ownership model

```text
HTTP sites     -> Pingora HTTP proxy
TCP listeners  -> Tokio TcpListener and Raddex relay
UDP listeners  -> Tokio UdpSocket and Raddex flow table
```

Both layer-4 transports fan their listener out across the configured worker
threads: one `SO_REUSEPORT` socket per thread, each drained by its own accept
or receive loop. For UDP this is load-bearing rather than an optimization —
each socket carries its own kernel receive buffer, so a burst of new flows that
would overflow one socket's buffer is spread across several. A datagram lost
that way is counted only by the kernel (`UdpRcvbufErrors`), never by a Raddex
metric, because the process never receives it.

Both layer-4 transports are native Tokio end to end: Raddex binds the socket,
accepts, terminates TLS, selects and health-checks upstreams, and relays. No
forwarded byte passes through Pingora, which hosts the process (background
services, shutdown watch, descriptor passing) and runs the HTTP core.

TCP and UDP are separate configuration types and separate listener identities.
They may share an address and port because they use different transports. Two
listeners that overlap for the same transport are rejected, including wildcard
and dual-stack conflicts.

## TCP contract

- Raw TCP relays bytes without parsing HTTP.
- `to` mode selects one upstream per connection using round-robin, random, or
  source-IP hash.
- Health checks are active TCP connects and are applied before selection.
- Connect and idle timeouts bound resource use; idle timeouts reset on traffic
  in either direction.
- `sni` mode reads a bounded ClientHello prefix, selects an exact or one-label
  wildcard route, and forwards the original bytes unchanged.
- TLS termination uses a Raddex-owned OpenSSL acceptor, built once per listener
  and bounded by a handshake timeout, and then reuses the raw relay.
- Transparent mode is a Linux socket/TPROXY integration and is not part of the
  standard listener handoff path.

## UDP contract

- A flow is identified by listener plus client address and port.
- Each flow owns a connected upstream socket so responses can be demultiplexed
  without a global response parser.
- Upstream selection happens once per flow; source-IP hashing does not remove
  the source port from flow identity.
- `max_flows`, `idle_timeout`, and `max_datagram_size` bound memory, lifetime,
  and packet size. Capacity eviction is oldest-first.
- IPv4 and IPv6 upstream families are preserved when creating flow sockets.
- Oversized datagrams are dropped and counted rather than forwarded partially.
- `recv_buffer` and `send_buffer` size each listener socket, so the listener's
  total kernel buffer is the configured value times the worker-thread count.

## Reload and upgrade invariants

Reload changes upstream sets, policies, limits, and timeouts for new work while
existing connections and flows keep their selected upstream.

Listener topology is a separate invariant. Adding, removing, or rebinding a
listener is rejected by reload and upgrade preflight. The upgrade driver checks
that every configured listener reports successful handoff before it accepts the
replacement as complete.

Linux UDP upgrade transfers:

1. every UDP listener file descriptor;
2. every connected upstream flow descriptor;
3. bounded JSON metadata that reconstructs the flow table.

The receiver queue stays attached to the transferred listeners. Any missing
descriptor, malformed metadata, or listener status error fails the handoff
rather than silently dropping active flows. The listener socket count is part
of the contract, so a replacement started with a different worker-thread count
is refused: change `--threads` with a normal restart, not an upgrade.

## Observability

HTTP and Layer 4 records are distinct. TCP and UDP access records include a
listener identity and normalized outcome; Prometheus metrics use the
`raddex_l4_tcp_*` and `raddex_l4_udp_*` families. The flow table must remain
bounded even when an upstream is unavailable or DNS resolution is slow.

## Explicit non-goals

- HTTP semantics inside a TCP block.
- QUIC or HTTP/3 termination.
- Cluster-wide rate limiting or shared flow state.
- A claim that UDP passthrough provides QUIC connection migration.

Future changes should preserve these boundaries or update the Raddexfile
specification, capability matrix, and integration tests in the same change.
