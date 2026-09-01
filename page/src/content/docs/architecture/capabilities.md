---
title: Capability matrix
description: Understand which Raddex features are supported, Linux-only, passthrough-only, or outside the Pingora process.
---

This page is the deployment-oriented summary of Raddex's protocol boundaries.
The configuration reference describes syntax; this page describes what the
runtime actually terminates, forwards, or leaves to another service.

## Status vocabulary

- **Supported** — implemented and covered by the v0.3.5 verification suite.
- **Linux-only** — supported, but requires Linux kernel behavior, privileges,
  or file-descriptor handoff.
- **Passthrough** — Raddex forwards bytes or datagrams without terminating the
  higher-level protocol.
- **Sidecar required** — the feature needs a protocol stack that is not part of
  Pingora 0.8.1 or Raddex's current runtime.

Raddex is pre-1.0. Supported behavior is the release contract for the current
version, while the [Raddexfile specification](../../config/directives/) remains
the source of truth for configuration compatibility.

## HTTP and TLS

| Capability | Status | Notes |
| --- | --- | --- |
| HTTP/1.1 reverse proxy | Supported | Plain HTTP listeners and HTTP/1.1 upstreams |
| Downstream HTTP/2 | Supported | TLS listeners advertise `h2` through ALPN and fall back to HTTP/1.1 |
| Upstream TLS | Supported | `https://` with SNI, CA, and client-certificate options |
| Upstream HTTP/2 | Supported | Use `h2://`; HTTP/2 is explicit for upstreams |
| Upstream h2c | Supported | Use `h2c://` for cleartext prior-knowledge HTTP/2 |
| WebSocket upgrades | Supported | Forwarded transparently by `reverse_proxy` |
| Automatic HTTPS | Supported | HTTP-01 by default, Cloudflare DNS-01, or TLS-ALPN-01 |
| TLS termination | Supported | Per-site certificates, internal certificates, version/cipher limits, and mTLS |

## Site matching

| Capability | Status | Notes |
| --- | --- | --- |
| Multiple domains in one site block | Supported | The body is compiled into independently addressable sites |
| IPv4 and IPv6 listeners | Supported | HTTP/TLS listeners use explicit dual-stack behavior |
| Exact host and SNI matching | Supported | Normalized host names are matched per listener |
| Wildcard host and SNI matching | Supported | One left-most label; exact names win |
| Apex wildcard matching | Not supported | `*.example.com` does not match `example.com` |
| Multi-label wildcard matching | Not supported | `*.example.com` does not match `a.b.example.com` |

## Layer 4

Layer 4 runs on a **native Tokio** data path: Raddex binds the socket, accepts,
terminates TLS, selects and health-checks upstreams, and relays with
`tokio::io`. Nothing it forwards passes through Pingora, which hosts the process
and runs the HTTP core. See the
[architecture record](https://github.com/chulingera2025/raddex/blob/main/docs/PINGORA_CAPABILITY_RESEARCH.md)
for why the two cores are split.

| Capability | Status | Notes |
| --- | --- | --- |
| Raw TCP proxying | Supported | No HTTP parsing; bounded connections and timeouts |
| TCP SNI passthrough | Supported | ClientHello is inspected and forwarded unchanged |
| TCP TLS termination | Supported | TLS is terminated before the raw byte relay |
| UDP datagram proxying | Supported | Per-client flow state and bounded resource usage |
| UDP IPv6 upstreams | Supported | Address family is preserved for flow sockets |
| Transparent TCP | Linux-only | Requires TPROXY rules, policy routing, and `CAP_NET_ADMIN`-equivalent permission |
| L4 load balancing | Supported | Round-robin, random, and consistent-hash `ip_hash` over healthy upstreams |
| L4 active health checks | Supported | TCP-connect probes with consecutive-failure and consecutive-success damping |
| TCP listener upgrade handoff | Linux-only | The listening descriptor is handed off explicitly; a failed transfer fails the upgrade |
| UDP lossless upgrade | Linux-only | Listener, connected flow sockets, and bounded metadata are handed off |
| QUIC datagram passthrough | Passthrough | QUIC packets are treated as ordinary UDP datagrams |
| QUIC / HTTP/3 termination | Sidecar required | Raddex does not implement QUIC handshakes, HTTP/3 routing, or migration |

## Operational boundaries

| Behavior | What to expect |
| --- | --- |
| `raddex check` | Validates the same configuration rules used by reload |
| SIGHUP reload | Replaces routing and runtime policy for new work; existing connections remain on their selected upstream |
| Listener topology change | Rejected by reload and by upgrade preflight; use a normal restart where required |
| Transparent TCP upgrade | Not supported by the standard handoff path; use a normal restart |
| Rate limiting | In-memory and per process; it is not cluster-wide |
| DNS-01 providers | Cloudflare is the implemented provider in v0.3.5 |

## Choosing the right boundary

Use Raddex directly when it needs to terminate HTTP, TLS, TCP, or UDP. Put a
dedicated QUIC/HTTP/3 service in front of or beside Raddex when the deployment
needs HTTP/3 termination, HTTP/3 routing, connection migration, or QUIC-aware
load balancing. Do not infer those capabilities from successful UDP
passthrough.
