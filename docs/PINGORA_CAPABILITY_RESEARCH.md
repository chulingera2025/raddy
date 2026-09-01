# Architecture and capability boundaries

This document records the boundary between Raddex, Pingora 0.8.1, the operating
system, and protocols that require a separate service. It is intentionally
written for deployment and design review rather than as a historical work
plan.

## Runtime layers

Raddex has two independent cores. The HTTP core runs on Pingora; the layer-4
core is native Tokio and forwards no byte through Pingora.

```text
                         Raddex
                           |
             +-------------+-------------+
             |                           |
          L4 Core                     L7 Core
             |                           |
           Tokio                      Pingora
             |                           |
         TCP / UDP                      HTTP
```

Raddex owns configuration, site selection, ACME policy, routing, access
records, and the release/upgrade protocol. Pingora supplies the HTTP proxy,
upstream pools, and TLS listener integration for L7.

The layer-4 core binds its own sockets, runs its own accept loops, terminates
its own TLS, selects and health-checks its own upstreams, and relays with
`tokio::io`. Pingora remains the *process* host — L4 listeners register as
background services, observe the shutdown watch, and use its descriptor-passing
helper for the upgrade handoff — but that is lifecycle, not data.

This split is a measured choice, and the measurement is mixed. On one host
(`bench/l4`, Nginx stream = 100%), holding 10 000 idle connections costs 79.2%
of Nginx's CPU and 113.1% of its memory natively, against 129.9% and 282.1%
through Pingora; UDP forwarding improved on both axes. Those are the workloads a
layer-4 proxy exists for.

Two costs are unresolved. The native listener reaches 44.5% of Nginx's
connection rate where the Pingora path reached 90.5%, and moves 64 KiB payloads
at 46.1% against 81.9%. Giving each worker its own `SO_REUSEPORT` accept loop
removed the connection-rate error rate but not the deficit, so accept
concurrency is not the explanation. Deployments dominated by short connections
or bulk transfer should not treat the native path as faster today. See
[the performance record](PERFORMANCE.md).

HTTP keeps using Pingora, where its proxy engine, connection pooling, and
protocol handling are the reason to depend on it at all.

## Capability matrix

| Capability | Boundary | Release behavior |
| --- | --- | --- |
| HTTP/1.1 reverse proxy | Pingora primitive plus Raddex routing | Supported |
| Downstream HTTP/2 | Pingora TLS/HTTP integration | Supported on TLS listeners |
| Upstream HTTP/2 | Pingora connector plus Raddex scheme selection | `h2://` |
| Upstream h2c | Pingora prior-knowledge H2 connector plus Raddex scheme selection | `h2c://` |
| Multi-domain and wildcard sites | Raddex application logic | Exact-first, one-label wildcard matching |
| IPv4/IPv6 HTTP listeners | Pingora listener plus Raddex bind planning | Explicit dual-stack behavior |
| Raw TCP proxying | Raddex-owned Tokio listener and relay | Native accept, relay, and admission |
| L4 load balancing and health checks | Raddex-owned | Round-robin, random, consistent-hash `ip_hash`; TCP-connect probes |
| TLS termination for L4 | Raddex-owned OpenSSL acceptor plus raw relay | Static or internal certificates |
| TLS-ALPN-01 | OpenSSL callbacks plus Raddex challenge store | Temporary RFC 8737 certificate on port 443 |
| Transparent TCP | Linux socket and routing integration | Requires TPROXY, policy routing, and network privilege |
| UDP proxying | Raddex-owned Tokio flow service | Bounded per-client flows |
| L4 listener upgrade handoff | Raddex-owned descriptor transfer | Linux-only, fail-closed verification |
| QUIC / HTTP/3 termination | Missing in Pingora 0.8.1 | Separate QUIC service or sidecar required |

## What the QUIC boundary means

The UDP listener can forward QUIC packets because it treats them as datagrams.
That is passthrough only. Raddex does not currently provide:

- QUIC handshake or connection state;
- HTTP/3 request parsing or routing;
- HTTP/3 stream lifecycle management;
- QUIC connection migration;
- QUIC-aware load balancing.

A deployment that needs those capabilities must terminate QUIC in a dedicated
service and then hand HTTP/1.1, HTTP/2, or another supported protocol to Raddex.

## Reload and upgrade seam

SIGHUP replaces the compiled routing snapshot and applies new policies to new
work. Existing connections and UDP flows retain their selected upstream.

The zero-downtime upgrade path transfers listener file descriptors, and Raddex
adds a topology digest so a replacement cannot silently start with a different
set of listeners.

HTTP listeners are transferred by Pingora. **Layer-4 listeners are not**: they
are Raddex-owned sockets, outside Pingora's automatic transfer, so each one
hands its descriptor over explicitly during the upgrade's fd-transfer phase and
publishes the outcome. `raddex upgrade` refuses to report success until every
configured TCP and UDP listener has published `ok`, so a failed transfer is a
failed upgrade rather than a silently unserved port. The UDP handoff carries
more: the listener, connected upstream flow descriptors, and bounded flow
metadata.

The handoff path does not own transparent TCP listeners; those deployments use
a normal restart.

## Operational consequence

Use the Raddex process when it needs to terminate HTTP, TLS, TCP, or UDP.
Treat Linux transparent routing and the layer-4 descriptor handoff as
privileged integrations that need host-level validation. Treat HTTP/3
termination as a separate protocol
service, not as an implied feature of UDP passthrough.
