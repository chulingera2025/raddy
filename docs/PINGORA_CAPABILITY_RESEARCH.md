# Architecture and capability boundaries

This document records the boundary between Raddex, Pingora 0.8.1, the operating
system, and protocols that require a separate service. It is intentionally
written for deployment and design review rather than as a historical work
plan.

## Runtime layers

```text
                         Raddex
                           |
             +-------------+-------------+
             |                           |
        HTTP / TLS                    Layer 4
             |                           |
       Pingora proxy       +------------+------------+
                           |                         |
                       TCP / TLS                  UDP
                    Pingora ServerApp          Tokio socket
```

Raddex owns configuration, site selection, ACME policy, routing, access
records, and the release/upgrade protocol. Pingora supplies the HTTP proxy,
upstream pools, TLS listener integration, and transport-level server app. The
UDP service is Raddex-owned because Pingora 0.8.1 does not expose a UDP listener
abstraction.

## Capability matrix

| Capability | Boundary | Release behavior |
| --- | --- | --- |
| HTTP/1.1 reverse proxy | Pingora primitive plus Raddex routing | Supported |
| Downstream HTTP/2 | Pingora TLS/HTTP integration | Supported on TLS listeners |
| Upstream HTTP/2 | Pingora connector plus Raddex scheme selection | `h2://` |
| Upstream h2c | Pingora prior-knowledge H2 connector plus Raddex scheme selection | `h2c://` |
| Multi-domain and wildcard sites | Raddex application logic | Exact-first, one-label wildcard matching |
| IPv4/IPv6 HTTP listeners | Pingora listener plus Raddex bind planning | Explicit dual-stack behavior |
| TLS termination for L4 | Pingora TLS listener plus raw relay | Static or internal certificates |
| TLS-ALPN-01 | OpenSSL callbacks plus Raddex challenge store | Temporary RFC 8737 certificate on port 443 |
| Transparent TCP | Linux socket and routing integration | Requires TPROXY, policy routing, and network privilege |
| UDP proxying | Raddex-owned Tokio flow service | Bounded per-client flows |
| UDP lossless upgrade | Raddex-owned fd and metadata handoff | Linux-only, fail-closed verification |
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

The zero-downtime upgrade path transfers compatible listener file descriptors.
Raddex adds a topology digest so a replacement cannot silently start with a
different set of listeners. The standard handoff path does not own transparent
TCP listeners; those deployments use a normal restart. UDP handoff transfers
the listener, connected upstream flow descriptors, and bounded metadata through
a private protocol and reports failure before claiming success.

## Operational consequence

Use the Pingora process when Raddex needs to terminate HTTP, TLS, TCP, or UDP.
Treat Linux transparent routing and UDP handoff as privileged integrations that
need host-level validation. Treat HTTP/3 termination as a separate protocol
service, not as an implied feature of UDP passthrough.
