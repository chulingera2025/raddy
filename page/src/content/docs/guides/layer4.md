---
title: Layer 4 (TCP & UDP)
description: Raw TCP and UDP proxying with the tcp and udp listeners — load balancing, timeouts, health checks, SNI routing, and DNS refresh.
---

Beyond HTTP, raddy can proxy raw TCP connections and UDP datagrams with the
`tcp` and `udp` **top-level listeners** — peers of HTTP site blocks, not
directives inside one. They own transport concepts only: upstream selection,
timeouts, connection/flow limits, and relay. TLS is never terminated (a `tcp`
listener with `sni` routes on the ClientHello and forwards it unchanged).

## Raw TCP

```caddyfile
tcp :3306 {
    to db-1.internal:3306 db-2.internal:3306
    lb_policy round_robin          # round_robin | random | ip_hash
    connect_timeout 3s
    idle_timeout 5m
    max_connections 10000
    health_check {
        interval 10s
        timeout 2s
    }
}
```

- **`to <host>:<port>...`** — at least one upstream. Hostnames are resolved at
  startup and re-resolved periodically (default 60s); a transient DNS failure
  keeps the last-known-good addresses.
- **`lb_policy`** — `round_robin` (default), `random`, or `ip_hash`
  (source-IP stickiness).
- **`connect_timeout`** bounds a single upstream connect; **`idle_timeout`** is
  a *true* inactivity timeout reset by traffic in either direction (a long-lived
  active connection never times out); **`max_connections`** caps concurrent
  connections (excess are rejected and counted).
- **`health_check { ... }`** — active TCP-connect probes; an unhealthy upstream
  is skipped, and when all are unhealthy new connections are refused.
- **Reload** — a SIGHUP reload applies the new upstream set/policy/limits to new
  connections; existing connections keep their upstream. Changing the bind
  address is a topology change and is rejected (restart or `raddy upgrade`).
- Each closed connection writes a typed JSON access-log line and
  `raddy_l4_tcp_*` metrics.

### SNI routing

A `tcp` listener can route TLS connections by the exact ClientHello SNI —
without terminating TLS:

```caddyfile
tcp 0.0.0.0:443 {
    sni api.example.com 10.0.0.1:9001
    sni web.example.com  10.0.0.2:9002
    fallback             10.0.0.3:9003
}
```

The ClientHello is inspected in a bounded prefix (never modified); the exact
bytes are forwarded to the matched upstream. An unknown / absent / malformed
SNI goes to `fallback` when set, otherwise the connection is closed. `sni` and
`to` are mutually exclusive; wildcard names and `health_check` are not
supported in SNI mode (v1).

## UDP

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

- Each client (address + port) maps to a **flow** with its own connected
  upstream socket — the ephemeral local port demultiplexes upstream responses.
  Selection happens once per flow; `ip_hash` pins the client *IP*.
- **Bounds** — `max_flows` caps the flow table (oldest-first eviction),
  `idle_timeout` evicts idle flows, `max_datagram_size` drops and counts
  oversized datagrams, `recv_buffer`/`send_buffer` size the sockets (0 = OS
  default).
- UDP and TCP may share an address and port.
- Metrics: `raddy_l4_udp_*`.
- **Zero-downtime upgrades do not apply to UDP**: the listener socket is bound
  outside the fd-handoff mechanism, so `raddy upgrade` cannot serve a UDP
  configuration — use a plain restart (flows reset).
