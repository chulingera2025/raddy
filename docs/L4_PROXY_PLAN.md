# Layer 4 architecture and operations

The filename is retained as a compatibility path for existing links. This file
replaces the historical implementation plan with the current runtime contract
for TCP and UDP listeners. The user-facing syntax is documented in the
[Layer 4 guide](https://chulingera2025.github.io/raddy/guides/layer4/); this
document records the invariants that should remain true when the subsystem
changes.

## Ownership model

```text
HTTP sites     -> Pingora HTTP proxy
TCP listeners  -> Pingora ServerApp over raw streams
UDP listeners  -> Tokio UdpSocket and Raddy flow table
```

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
- TLS termination uses Pingora's TLS listener and then reuses the raw relay.
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

## Reload and upgrade invariants

Reload changes upstream sets, policies, limits, and timeouts for new work while
existing connections and flows keep their selected upstream.

Listener topology is a separate invariant. Adding, removing, or rebinding a
listener is rejected by reload and upgrade preflight. The upgrade driver checks
that every configured listener reports successful handoff before it accepts the
replacement as complete.

Linux UDP upgrade transfers:

1. the UDP listener file descriptor;
2. every connected upstream flow descriptor;
3. bounded JSON metadata that reconstructs the flow table.

The receiver queue stays attached to the transferred listener. Any missing
descriptor, malformed metadata, or listener status error fails the handoff
rather than silently dropping active flows.

## Observability

HTTP and Layer 4 records are distinct. TCP and UDP access records include a
listener identity and normalized outcome; Prometheus metrics use the
`raddy_l4_tcp_*` and `raddy_l4_udp_*` families. The flow table must remain
bounded even when an upstream is unavailable or DNS resolution is slow.

## Explicit non-goals

- HTTP semantics inside a TCP block.
- QUIC or HTTP/3 termination.
- Cluster-wide rate limiting or shared flow state.
- A claim that UDP passthrough provides QUIC connection migration.

Future changes should preserve these boundaries or update the Raddyfile
specification, capability matrix, and integration tests in the same change.
