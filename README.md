# Raddex

[![CI](https://github.com/chulingera2025/raddex/actions/workflows/ci.yml/badge.svg)](https://github.com/chulingera2025/raddex/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/chulingera2025/raddex)](LICENSE)
[![Release](https://img.shields.io/github/v/release/chulingera2025/raddex)](https://github.com/chulingera2025/raddex/releases)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange)](https://www.rust-lang.org/)

[Chinese documentation](README.zh_CN.md)

Raddex is a small reverse-proxy gateway written in Rust and built on
[Cloudflare Pingora](https://github.com/cloudflare/pingora). It combines a
readable, Caddy-style configuration file with Pingora's multi-threaded proxy
engine, shared upstream pools, and memory safety.

## The short version

Use Raddex when you want one binary to handle HTTP/HTTPS routing, automatic TLS,
static files, upstream load balancing, and selected TCP/UDP workloads without
turning the configuration into application code.

Raddex's defining rule is simple: directives in a site block are interpreted in
the order you write them. Terminals decide who serves the request; modifiers
and guards describe how that terminal behaves. The model is explicit rather
than dependent on a hidden ordering table.

## Release surface

Raddex `v0.3.5` is a pre-1.0 release. The following table describes what is
implemented and tested in this release; it is not a promise that every public
API is already frozen.

| Area | Included | Boundary |
| --- | --- | --- |
| HTTP reverse proxy | HTTP/1.1, downstream HTTP/2, WebSocket upgrades, load balancing, health checks | Rate limits are per process |
| TLS and ACME | HTTP-01, Cloudflare DNS-01, TLS-ALPN-01, static certificates, internal certificates, mTLS | TLS-ALPN-01 is for eligible ACME sites on port 443 and cannot be combined with DNS-01; other DNS-01 providers are not included |
| Upstream protocols | HTTP/1.1, `https://`, `h2://`, `h2c://` | `h2c://` requires prior-knowledge HTTP/2 upstreams |
| Site routing | Multiple domains, IPv4/IPv6, exact and one-label wildcard matching | Wildcards do not match the apex or multiple labels |
| Layer 4 | TCP, SNI passthrough, TCP TLS termination, UDP datagram proxying | Transparent TCP and UDP handoff are Linux-only integrations |
| Operations | Config check, SIGHUP reload, zero-downtime binary upgrade, JSON/Prometheus output | Listener topology changes require a normal restart; upgrades require unchanged topology |
| QUIC / HTTP/3 | UDP datagram passthrough | HTTP/3 termination and routing require a separate QUIC service or sidecar |

## Five-minute local proxy

Install a release binary, or build from source, then start a local upstream:

```bash
python3 -m http.server 8080 --bind 127.0.0.1
```

Create `Raddexfile`:

```caddyfile
example.local:8090 {
    reverse_proxy 127.0.0.1:8080
}
```

Validate and run Raddex in another terminal:

```bash
raddex check -c Raddexfile
raddex run -c Raddexfile
```

Send a request with the site Host header:

```bash
curl -H 'Host: example.local' http://127.0.0.1:8090/
```

`raddex check` performs the same configuration validation used by reload. Keep
it in deployment scripts and CI before starting or reloading the service.

## Where Raddex actually lands on performance

The repository includes a Docker comparison suite that runs Nginx, Caddy, and
Raddex one at a time against the same origin, scenario matrix, TLS certificate,
and resource limits. Every number below comes from one `full` run and is
normalized against Nginx in its own scenario.

**Summary: Raddex sits between Caddy and Nginx.** It is substantially leaner
than Caddy on every axis measured, and it does not match Nginx on raw
throughput or per-request CPU. If you are replacing Nginx purely for speed,
this data does not justify the switch; if you are choosing between Caddy-style
configuration ergonomics and Nginx-style efficiency, that is the gap Raddex
targets.

Medians across the nine scenarios, plus the one scenario that actually measures
peak throughput (Nginx = 100%):

| Metric | Caddy | Raddex | Better is |
| --- | ---: | ---: | --- |
| Max stable throughput (concurrency scan) | 39.7% | **59.6%** | higher |
| p99 latency (median of 9 scenarios) | 154.5% | **116.6%** | lower |
| CPU per request (median) | 274.7% | **146.5%** | lower |
| Peak memory (median) | 306.2% | **174.0%** | lower |

In absolute terms on the test machine, the concurrency scan peaked at 15 603
QPS for Nginx, 9 297 for Raddex, and 6 201 for Caddy. No target recorded a
non-zero error rate in any scenario.

Two results worth calling out individually:

- **HTTPS with HTTP/2 multiplexing.** At a 4 000 QPS target, Nginx and Raddex
  both sustained it (4 008 QPS); Caddy reached only 1 008 QPS (25.1%) and used
  6.5× Nginx's CPU per request.
- **Connection churn is Raddex's weakest scenario.** p99 is 361% of Nginx
  (5.08 ms vs 1.41 ms) and worse than Caddy's 116%. Keep-alive workloads are
  fine; a workload that opens a fresh connection per request is not where
  Raddex is strong today.

Raddex beats Nginx in exactly one scenario — 1 MiB responses, at 45.8% of its
p99 and 70.8% of its CPU per request.

### How to read the full table

Seven of the nine scenarios drive a **fixed** request rate, so every target
reports `100.0%` throughput there — that is all three hitting the target rate,
not a tie on capacity. Only the concurrency scan searches for maximum stable
throughput, so it is the only throughput figure that means anything. A point
counts as stable only at an error rate ≤ 0.1%.

### What was measured, and what was not

- **Test machine**: 10-core / 7 GiB Debian 12 host, Docker 29.7. Each proxy ran
  under a 2 CPU / 1 GiB limit, with Nginx at 2 workers and Raddex at 2 Pingora
  threads. `oha` 1.16.0 as the load generator; 10 s warm-up, 30 s measurement,
  3 repetitions, aggregated by median.
- **Access logging is off for all three.** This is fair — no target writes logs
  — but it means Raddex's access-log path is not exercised here, and it is not
  free: it serializes on one mutex and flushes per request. Enable it and
  expect a different profile than these numbers.
- **Not covered**: ACME, caching, TCP/UDP, QUIC/HTTP3, and anything
  implementation-specific. Those need their own fairness definitions.
- **Relative only.** The percentages are valid within a scenario on one machine
  and one run. Do not compare absolute QPS across machines, and re-run the
  suite on your own hardware before treating any of this as a capacity plan.

Reproduce it — the host needs only Docker Engine and Compose v2:

```bash
./bench/scripts/run.sh full
```

`quick` also exists, but it is a 3-second smoke test, not a comparable run. See
the [benchmark documentation](docs/PERFORMANCE.md) for the full matrix and the
per-scenario tables, and [`bench/`](bench/) for the scenario definitions.

![Relative maximum stable throughput](page/public/benchmarks/throughput.svg)

## A production-shaped site

```caddyfile
{
    acme_email ops@example.com
    trusted_proxies 10.0.0.0/8 192.168.0.0/16
}

:80 {
    redir https://{host}{uri} permanent
}

api.example.com {
    rate_limit remote_ip 100r/s burst=200

    handle /static/* {
        root /var/www/html
        file_server
        encode zstd gzip
    }

    reverse_proxy {
        to https://10.0.0.11:8443 https://10.0.0.12:8443
        tls_servername api.internal
        health_check {
            interval 5s
            timeout 2s
        }
    }
}
```

The default ACME method is HTTP-01. Use `dns_challenge` when port 80 cannot be
reached, or `tls_alpn_challenge` when the ACME server can reach TCP 443 and the
site is eligible for TLS-ALPN-01.

If either backend uses a private CA, add `tls_ca <path>` to the
`reverse_proxy` block and ensure the file exists before running `raddex check`.

## Documentation map

- [Documentation site](https://chulingera2025.github.io/raddex/) — task-oriented guides and reference.
- [Installation and deployment](docs/INSTALL.md) — release binaries, Docker, systemd, permissions, and upgrades.
- [Raddexfile specification](docs/RADDEXFILE_SPEC.md) — configuration semantics and compatibility source of truth.
- [Architecture and capability boundaries](docs/PINGORA_CAPABILITY_RESEARCH.md) — what is native, application-level, Linux-only, or sidecar-based.
- [Layer 4 architecture](docs/L4_PROXY_PLAN.md) — TCP/UDP runtime model and operational invariants.
- [Performance comparison](docs/PERFORMANCE.md) — the Docker comparison suite and normalized metrics.
- [Release checklist](docs/RELEASE_CHECKLIST_v0.3.5.md) — historical release evidence.

## Build from source

```bash
cargo build --release --locked
./target/release/raddex --version
```

Stable Rust, OpenSSL development libraries, and CMake are required for a
source build. Prebuilt release artifacts currently target Linux GNU on
`x86_64` and `aarch64`.

## Project status

The released tree contains the HTTP/TLS gateway, the Raddexfile parser and
validator, automatic HTTPS, the migration tool, observability, and the tested
TCP/UDP extensions described above. Work that depends on a separate QUIC
transport is intentionally kept outside the Pingora process. See the
[capability document](docs/PINGORA_CAPABILITY_RESEARCH.md) before deploying a
protocol that needs termination rather than passthrough.

## License

[Apache-2.0](LICENSE), matching Pingora.
