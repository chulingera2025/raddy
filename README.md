# Raddy

[![CI](https://github.com/chulingera2025/raddy/actions/workflows/ci.yml/badge.svg)](https://github.com/chulingera2025/raddy/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/chulingera2025/raddy)](LICENSE)
[![Version](https://img.shields.io/badge/version-v0.1.0-blue)]()
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange)]()
[中文文档](README.zh_CN.md)

A minimal, high-performance reverse proxy gateway in Rust, built on
[Cloudflare Pingora](https://github.com/cloudflare/pingora). Raddy combines the
**developer experience of Caddy** (an explicit, write-order config DSL + native
automatic HTTPS) with the **performance and memory safety of Pingora** (Rust,
no GC, multi-threaded shared connection pools).

## Why Raddy

The Rust gateway landscape splits into two camps: hand-written Pingora code
(flexible but no config model) and existing distributions (diverse config
models, limited extensibility). Raddy takes a different answer on three things:

1. **Predictable configuration** — directives execute strictly in write order,
   with no implicit ordering table. The result is predictable without looking
   anything up.
2. **Automatic HTTPS as a first-class citizen** — native ACME issuance and SNI
   dynamic certificates, out of the box.
3. **Pingora's hardcore engine** — multi-threaded shared connection pools, no
   GC, and (future) zero-downtime binary upgrades.

## Features

- **Explicit write-order DSL** (`Raddyfile`) — terminal vs modifier directives,
  `handle` mutual-exclusion blocks, and no hidden ordering rules. See the
  [Raddyfile specification](docs/RADDYFILE_SPEC.md).
- **Automatic HTTPS** — ACME (HTTP-01) issuance for named sites, SNI dynamic
  certificates, and an on-demand `ask` authorization callback.
- **Config hot reload** — SIGHUP swaps the routing snapshot atomically; the
  upstream connection pools survive reloads (zero-interrupt).
- **Static file serving + compression** — `file_server` with traversal
  protection, and `encode` with gzip/zstd honoring `Accept-Encoding`.
- **Observability** — structured JSON access logs and a Prometheus `/metrics`
  endpoint (QPS + latency).
- **Forwarding** — `reverse_proxy` with `to` multi-target round-robin, header
  rewrites, redirects, and clean 400/404 fallbacks.
- **Edge protection** — `rate_limit remote_ip` single-node token-bucket rate
  limiting, with `trusted_proxies` for the real client IP.
- **Migration** — `raddy import caddyfile|nginx <file>` converts a common
  Caddyfile / nginx.conf subset into a Raddyfile (independent converter, never
  changes the Raddyfile grammar).

## Quick start

Install the latest release (checksum-verified installer, no `curl | sudo bash`):

```bash
curl -fsSL -O https://github.com/chulingera2025/raddy/releases/latest/download/install.sh
./install.sh
raddy --version
```

Or build from source:

```bash
cargo build --release
./target/release/raddy --version
```

Write a `Raddyfile`:

```
:8080 {
    reverse_proxy 127.0.0.1:9000
}
```

Run it:

```bash
raddy run -c Raddyfile
curl http://127.0.0.1:8080/
```

For automatic HTTPS, configure a named site and run with ACME (requires a
publicly reachable domain on ports 80/443):

```
raddy.test {
    reverse_proxy 127.0.0.1:9000
}
```

```bash
raddy run -c Raddyfile --acme-directory https://acme-v02.api.letsencrypt.org/directory
```

## Configuration

See the [Raddyfile specification](docs/RADDYFILE_SPEC.md) for the full syntax
and semantics: sites, `handle`, headers, static files, compression, redirects,
and the global block.

## Documentation

- [Installation](docs/INSTALL.md) — installer script, manual install, Docker
- [Raddyfile specification](docs/RADDYFILE_SPEC.md) — the config language
- [Performance](docs/PERFORMANCE.md) — reproducible QPS / P99 baseline
- [中文文档](README.zh_CN.md)

## Development status

The core implementation is complete: forwarding + hot reload, the Raddyfile
parser (fuzz-verified, with line/column errors), ACME automatic HTTPS (verified
against a local Pebble test CA), static hosting + compression, observability,
and release tooling.

## License

[Apache-2.0](LICENSE) — matching Pingora.
