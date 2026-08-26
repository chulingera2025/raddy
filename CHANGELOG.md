# Changelog

All notable changes to Raddy are documented here, newest first. Releases are
tagged `v*`; this file follows [Keep a Changelog](https://keepachangelog.com/)'s
shape (Added / Changed / Fixed), though the project keeps the "Unreleased"
section short.

## [Unreleased]

> Target release: `v0.3.0`. The current worktree contains the implementation
> and release-candidate tests; no tag has been created yet.

### Added

- **Layer-4 raw TCP proxying** (`tcp <address> { ... }`, L4_PROXY_PLAN P0). A
  top-level listener that relays raw TCP connections to upstreams with
  `lb_policy` (round-robin/random/source-IP hash), `connect_timeout` /
  `idle_timeout` (a true inactivity timeout reset by traffic in either
  direction), `max_connections` admission, and active TCP-connect `health_check`
  probes. IPv6 addresses supported. Prometheus metrics (`raddy_l4_tcp_*`) and
  typed JSON access records per closed connection. A SIGHUP reload applies the
  new upstream set/policy/limits to new connections (existing ones keep their
  upstream); a reload that changes the listener *topology* is rejected.
- **Layer-4 SNI routing** (`sni <name> <host:port>` / `fallback`, L4 P1): a
  `tcp` listener routes TLS connections by the exact ClientHello SNI without
  terminating TLS — a bounded, fuzz-targeted ClientHello inspector extracts the
  SNI and forwards the exact bytes unchanged. Unknown/absent/malformed SNI uses
  the fallback or closes.
- **Layer-4 UDP proxying** (`udp <address> { ... }`, L4 P2): datagram proxy with
  per-client flows (connected upstream sockets demultiplex responses),
  source-IP-hash stickiness, bounded flow tables (capacity + idle eviction),
  oversized-datagram accounting, configurable socket buffers, and typed flow
  records (`raddy_l4_udp_*`). UDP and TCP may share a port. Zero-downtime
  upgrades do not transfer UDP flows (documented restart path).
- **Layer-4 DNS refresh** (L4 plan): hostname upstreams are re-resolved
  periodically; the resolved set is swapped for new connections only, and a
  transient refresh failure keeps last-known-good (`raddy_l4_tcp_dns_refresh_failures_total`).
- **Implicit HTTP-01 listener on :80.** A config with named sites but no site on
  port 80 now binds a plain-HTTP `:80` listener that serves only the ACME
  challenge, so automatic HTTPS actually completes without an explicit `:80`
  catch-all. `dns_challenge` (DNS-01) skips it; an explicit `:80` catch-all is
  never duplicated.
- **Compression minimum size.** Responses smaller than 64 bytes are served
  uncompressed — the codec framing made them larger than the payload.
- **Hidden files are never served by `file_server`.** Any path segment starting
  with `.` (`.env`, `.git/`, `.htaccess`) is rejected with 404.
- CHANGELOG.md, plus a `raddy.service` systemd unit example.

### Changed

- `{$ENV}` placeholders now expand **embedded in a word** — the spec's
  `reverse_proxy https://{$BACKEND_HOST}:8443` example parses correctly, and a
  mid-word reference merges its split fragments back into one argument.
- Block nesting is bounded (`MAX_BLOCK_DEPTH`): deeply nested `handle` blocks
  are rejected with a clear error instead of overflowing the stack on an
  untrusted config.
- `redir /foo 200` (or any trailing token that is not a redirect code) is now a
  parse error instead of silently becoming `/foo200` with 308.
- `install.sh` verifies only the current architecture's checksum line, so the
  two-architecture `SHA256SUMS` no longer makes every install fail; the release
  workflow records `install.sh`'s checksum correctly (and fails loudly if the
  file is missing).

### Fixed

- **ACME certificates for named sites on non-443 TLS ports now serve.** Issuance,
  persistence, and the SNI lookup all use the same `host:port` store key, so a
  site like `foo.com:8443 { tls }` no longer issues a certificate it can never
  present (A1).
- **The renewal scheduler no longer hijacks static / `tls internal`
  certificates.** Operator-supplied certificates are tracked as such and never
  re-issued via ACME; a non-443 static cert is also no longer fed to ACME as the
  invalid identifier `host:port` (A2/A3).
- **`ip_hash` stickiness survives multiple TLS peers sharing one address.** The
  same client now stays pinned to one peer instead of alternating SNI identities
  (A4).
- **Malformed Host ports are a 400.** `Host: example.com:notaport` no longer
  silently strips the port and matches the named site (RFC 9112 §3.2).
- **HTTP/2 requests are routed correctly.** raddy advertises `h2` but site
  selection read only the `Host` header, which HTTP/2 clients do not send (the
  authority travels in the `:authority` pseudo-header) — so every HTTP/2 request
  fell through to a 400. The handler now falls back to the URI authority.
- **The ACME/DNS-01 HTTP clients no longer panic on first TLS use.** Feature
  unification enables both rustls crypto backends (`aws-lc-rs` via instant-acme,
  `ring` via rustls-platform-verifier), which rustls 0.23 refuses to auto-select;
  the aws-lc-rs provider is now installed explicitly at startup.
- The ACME root CA PEM is written into the private `cert_dir` instead of a fixed
  `/tmp` path (symlink/overwrite hardening).
- The access-log docs no longer claim a rename-based rotation is followed; the
  log handle is held for the process lifetime (use logrotate `copytruncate`).

## [v0.2.10] — 2026-08-14

### Added

- TLS to upstreams (`https://` targets with `tls_servername` / `tls_skip_verify`
  / `tls_ca` / `tls_cert`), per-site `tls` options (min/max version, ciphers,
  mTLS `client_auth`), HTTP/2 ALPN on TLS listeners, WebSocket upgrades.
- Routing matchers (`path`, `host`, `method`, `header`, `query`, `remote_ip`,
  `protocol`, `!` negation), `rewrite`, `handle_path`, `respond`, `error`.
- Auth guards: `basic_auth` (bcrypt) and `forward_auth`.
- Compression: gzip / zstd / brotli with `encode` priority ordering.
- `rate_limit` header keys (`rate_limit header X-API-Key`), site-scoped
  `trusted_proxies`, access log (`json` / `common` formats, `access_log off`).

### Fixed

- Audit fixes from the v0.2.10 review pass (see the commit history).

## [v0.2.1] — 2026-08-05

### Added

- Proxy hardening: bounded issuance queue, health-check runner, `lb_policy`
  (`round_robin` / `random` / `ip_hash`) and `health_check` sub-directives.

## [v0.1.2] — 2026-08-05

### Added

- `rate_limit remote_ip` single-node token-bucket limiting with `trusted_proxies`
  (the real-client-IP trust model).
- `raddy import caddyfile|nginx` migration tool.
- Community workflows (issue/PR templates, CODEOWNERS).

## [v0.1.0] — 2026-08-05

### Added

- Initial release: Raddyfile DSL, `reverse_proxy`, `file_server`, `encode`,
  `redir`, SIGHUP hot reload, ACME automatic HTTPS (HTTP-01, verified against
  Pebble), structured access log, Prometheus metrics, release installer.

[Unreleased]: https://github.com/chulingera2025/raddy/compare/v0.2.10...HEAD
[v0.2.10]: https://github.com/chulingera2025/raddy/compare/v0.2.1...v0.2.10
[v0.2.1]: https://github.com/chulingera2025/raddy/compare/v0.1.2...v0.2.1
[v0.1.2]: https://github.com/chulingera2025/raddy/compare/v0.1.0...v0.1.2
[v0.1.0]: https://github.com/chulingera2025/raddy/releases/tag/v0.1.0
