---
title: Layer 4 (TCP and UDP)
description: Proxy raw TCP and UDP with explicit routing, limits, timeouts, TLS modes, and Linux boundaries.
---

Layer 4 listeners are top-level `tcp` and `udp` blocks. They are peers of HTTP
site blocks and do not enter the HTTP routing pipeline.

| Mode | Raddex does | Raddex does not do |
| --- | --- | --- |
| Raw TCP | Connects and relays bytes | Parse HTTP or terminate TLS by default |
| SNI passthrough | Inspects a bounded ClientHello prefix and routes it | Decrypt or modify the TLS stream |
| TCP TLS termination | Terminates TLS, then relays decrypted bytes | Expose HTTP routing inside the TCP block |
| UDP proxy | Tracks bounded client flows and relays datagrams | Provide QUIC or HTTP/3 termination |

## Raw TCP

```caddyfile
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
```

- `to` accepts one or more upstreams. Hostnames resolve at startup and refresh
  periodically; an initial resolution failure prevents startup.
- `lb_policy` supports `round_robin`, `random`, and `ip_hash`.
- `connect_timeout` limits one upstream connection.
- `idle_timeout` measures inactivity in either direction, not connection age.
- `max_connections` bounds concurrent TCP connections. The slot is taken when
  the connection is accepted, so on a TLS-terminating listener it also bounds
  connections still completing their handshake.
- `health_check` uses active TCP connects and skips unhealthy upstreams. A
  backend leaves the rotation only after `consecutive_failures` probes fail and
  rejoins only after `consecutive_successes` succeed, so one blip does not flap
  it. `ip_hash` uses consistent hashing, so adding or removing a backend does
  not reshuffle unrelated clients.

Changing upstreams, policies, limits, or timeouts applies to new connections;
existing connections keep their selected upstream. A listener bind change is a
topology change and requires a normal restart; an upgrade is valid only when the
listener topology is unchanged.

## SNI passthrough

Use `sni` routes when the TLS connection must reach the backend unchanged:

```caddyfile
tcp :443 {
    sni api.example.com 10.0.0.1:9001
    sni *.example.com 10.0.0.2:9002
    fallback 10.0.0.3:9003
}
```

SNI mode and `to` mode are mutually exclusive. Exact names win over wildcard
names; a wildcard matches one left-most label only. An absent, malformed, or
unknown SNI uses `fallback` when present, otherwise the connection is closed.
SNI mode does not support the upstream health-check block.

The ClientHello is inspected in a bounded prefix and then forwarded unchanged.
This is routing by SNI, not TLS termination.

## TCP TLS termination

```caddyfile
tcp :8443 {
    tls internal
    to 127.0.0.1:9443
}
```

Use `tls internal` or `tls <cert-file> <key-file>`. Raddex completes the TLS
handshake and passes the decrypted byte stream to the raw relay. This mode uses
the ordinary `to` upstream set and cannot be combined with SNI passthrough or
transparent mode.

## Transparent TCP

```caddyfile
tcp :15000 {
    transparent
    to 127.0.0.1:8080
}
```

Transparent mode is a Linux-only integration. It uses `IP_TRANSPARENT`, the
original destination supplied by TPROXY socket metadata, and the original
client address on the outbound connection. Deployment also needs:

- `CAP_NET_ADMIN` or an equivalent privilege;
- netfilter TPROXY rules;
- policy routing for marked packets;
- a route to the selected upstream.

Because this mode owns a custom listener, `raddex upgrade` is not available for
transparent TCP. Use a normal restart after validating the new configuration.

## UDP flows

```caddyfile
udp :53 {
    to 1.1.1.1:53 8.8.8.8:53
    lb_policy ip_hash
    idle_timeout 30s
    max_flows 50000
    max_datagram_size 4096
    recv_buffer 4MiB
    send_buffer 4MiB
}
```

Each client address and port creates a flow with a connected upstream socket.
The upstream is selected once per flow; `ip_hash` pins the client IP while the
flow identity still includes the source port.

- `max_flows` bounds the flow table; oldest flows are evicted at capacity.
- `idle_timeout` removes flows with no traffic in either direction.
- `max_datagram_size` drops and counts oversized datagrams.
- `recv_buffer` and `send_buffer` set socket buffers; `0` keeps the OS default.
- IPv4 and IPv6 upstreams are supported.
- TCP and UDP may use the same address and port because they are different
  transports.

On Linux, a zero-downtime upgrade transfers layer-4 listening sockets
explicitly. Raw TCP listeners hand over their listening descriptors; UDP hands
over the listener, its connected upstream flow sockets, and bounded flow
metadata. `raddex upgrade` does not report success until every configured
layer-4 listener has confirmed its transfer, so a failed handoff fails the
upgrade rather than leaving a port silently unserved.

## QUIC boundary

UDP forwarding can carry QUIC datagrams, but it is only passthrough. Pingora
0.8.1 and Raddex do not terminate QUIC, route HTTP/3 requests, or manage QUIC
connection migration. Use a dedicated QUIC/HTTP/3 service when those functions
are required.

## Performance

The layer-4 core is built directly on native Tokio rather than Pingora,
providing high throughput (156%–177% of Nginx stream on bulk TCP) and low
memory consumption. See the [Performance Comparison](../../performance/#layer-4-forwarding-benchmark)
for the full benchmark matrix and normalized metrics.

