# Raddy

Raddy is a minimal, high-performance reverse proxy gateway in Rust, built on
[Cloudflare Pingora](https://github.com/cloudflare/pingora). It pairs a
Caddy-style configuration DSL (the Raddyfile) with Pingora's engine: explicit
write-order configuration, automatic HTTPS via ACME, and multi-threaded shared
connection pools.

## Documentation

- [Installation](install.md) — installer script, manual install, Docker
- [Configuration](spec.md) — the Raddyfile specification
- [Performance](performance.md) — reproducible QPS / P99 baseline

## Quick start

```bash
raddy run -c Raddyfile
```

Write a `Raddyfile`, then validate it with `raddy check -c Raddyfile` before
running. See the [Raddyfile specification](spec.md) for the full language.
