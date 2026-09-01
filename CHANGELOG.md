# Changelog

All notable changes to Raddex are documented here, newest first. Releases are
tagged `v*`; this file follows [Keep a Changelog](https://keepachangelog.com/)'s
shape (Added / Changed / Fixed), though the project keeps the "Unreleased"
section short.

## [Unreleased]

### Added

- `dns_challenge` accepts a block form carrying any number of named
  credentials, alongside the existing one-line shorthand:

  ```caddyfile
  dns_challenge cloudflare {
      api_token {$CLOUDFLARE_API_TOKEN}
  }
  ```

  The single-credential shorthand (`dns_challenge cloudflare <api_token>`) is
  unchanged, so existing configurations keep working. The block form is what
  lets a provider requiring several credentials — an access key plus a secret
  plus a region — be configured at all.

- `CONTRIBUTING.md`, with a step-by-step walkthrough for adding a DNS-01
  provider.

- `bench/l4/`, a Linux-only forwarding benchmark comparing Nginx stream, Caddy
  layer4, Raddex, and Linux NAT / nftables as a kernel reference.

- Published benchmark numbers. The README and the performance documents now
  carry the measured results of a `full` run — including where Raddex loses —
  instead of an unlabelled chart. Headline: Raddex reaches 59.6% of Nginx's max
  stable throughput and 146.5% of its CPU per request (medians), against
  Caddy's 39.7% and 274.7%. Connection churn is Raddex's weakest scenario
  (p99 361% of Nginx, worse than Caddy); 1 MiB responses are its best (p99
  45.8%, CPU 70.8%).

### Changed

- **The layer-4 data path is now native Tokio.** Raw TCP listeners bind their
  own socket, run their own accept loop, terminate their own TLS, select and
  health-check their own upstreams, and relay with `tokio::io`. Nothing they
  forward passes through Pingora, which remains the process host and the HTTP
  engine. Raddex now has two cores: L4 on Tokio, L7 on Pingora.

  The reason is measurable, not architectural taste. Measured on one host
  (`bench/l4`, `quick` profile, Nginx stream = 100%), against the Pingora path
  it replaces:

  | Metric | Pingora path | Native path |
  | --- | ---: | ---: |
  | Long-lived 10K · CPU | 129.9% | **79.2%** |
  | Long-lived 10K · memory | 282.1% | **113.1%** |
  | UDP flows 10K · memory | — | **56.4%** |
  | UDP packets/s · CPU | — | **86.2%** |
  | TCP connect rate | 90.5% | **44.5%** |
  | TCP throughput 64 KiB | 81.9% | **46.1%** |
  | TCP p99 / 64 B | 50.0% | **250.0%** |

  **This is a trade, not a clean win.** Holding 10 000 idle connections now
  costs 79.2% of Nginx's CPU and 113.1% of its memory, against 129.9% and
  282.1% through Pingora — the workload the layer-4 proxy exists for. UDP
  improved on both axes. But the native listener accepts more slowly than the
  Pingora path did and moves large payloads more slowly, and neither has been
  explained yet: adding one `SO_REUSEPORT` accept loop per worker eliminated the
  connection-rate error rate but did not raise the rate itself, which points at
  something other than accept concurrency.

  Deployments dominated by long-lived connections or UDP benefit today.
  Deployments dominated by short connections or bulk transfer should not upgrade
  for performance yet. The `quick` profile runs one repetition and shows visible
  run-to-run variance — Caddy's connection rate moved from 105.5% to 77.0%
  between two runs of the same commit-pair — so these figures will be replaced
  by a repeated `full`-profile run before any of them is advertised.

  No configuration changes. `tcp` listeners, `lb_policy`, `health_check`,
  `sni`, `tls`, `transparent`, timeouts, and limits all behave as documented.

- `ip_hash` for layer-4 listeners is now consistent hashing over a 160-vnode
  ring, so adding or removing a backend no longer reshuffles unrelated clients.

- DNS-01 providers are now registry entries rather than hard-coded branches.
  Each provider declares its credential fields in `src/server/dns/mod.rs`, and
  the `dns_challenge` grammar, the "unknown provider" error, the
  required/unknown/duplicate-credential checks, and `raddex check` are all
  derived from that declaration. Adding a provider touches one new file plus
  one registry entry — the parser and the validator do not change.

- DNS-01 credential values are redacted from diagnostic output. They previously
  sat in a `Debug`-derived config struct, so a token could reach a log line or
  a panic message.

- **Renamed the project from `raddy` to `raddex`.** The crate `raddy` on
  crates.io is an unrelated automatic-differentiation library, so the old name
  could never be published or installed with `cargo install`. `raddex` is
  unclaimed on both crates.io and GitHub. This renames the binary, the crate,
  the config file, the metric prefix, the environment variables, and the
  default paths:

  | Before | After |
  | --- | --- |
  | `raddy` binary and crate | `raddex` |
  | `Raddyfile` | `Raddexfile` |
  | `raddy_*` Prometheus metrics | `raddex_*` |
  | `RADDY_*` environment variables | `RADDEX_*` |
  | `raddy_certs/` default cert dir | `raddex_certs/` |
  | `/tmp/raddy_upgrade.sock` | `/tmp/raddex_upgrade.sock` |
  | `/etc/raddy/`, `raddy.service` | `/etc/raddex/`, `raddex.service` |
  | `docs/RADDYFILE_SPEC.md` | `docs/RADDEXFILE_SPEC.md` |

  A config named `Raddyfile` is still loaded when no `Raddexfile` sits beside
  it, with a deprecation warning. **This fallback is removed in `v0.4.0`** —
  rename the file. Nothing else falls back: Prometheus dashboards scraping
  `raddy_*` and units referencing `/etc/raddy/` must be updated at upgrade
  time. Existing GitHub URLs keep working through GitHub's rename redirect.

### Fixed

- Layer-4 `ip_hash` distributed client IPs from one subnet very unevenly.
  Selection hashed with FNV-1a, whose high-bit avalanche is weak for inputs
  sharing a long prefix — which is exactly what client IPs are. Measured, 400
  addresses in one `/24` landed in 16% of the hash space and every one of them
  was routed to a single backend. Selection now applies an avalanche finalizer.
  (The UDP selector was checked for the same defect and does not have it.)

- `TransparentTcpProxy` panicked on startup with "there is no reactor running":
  it registered its listener with Tokio from the startup thread, before the
  runtime existed. Both layer-4 TCP listeners now bind a blocking socket and
  register it once their service starts.

## [v0.3.5] — 2026-08-26

### Added

- Upstream HTTP/2 and cleartext prior-knowledge h2c via `h2://` and
  `h2c://`.
- Shared multi-domain site blocks, IPv6 HTTP listeners and site addresses, and
  exact-plus-one-label wildcard matching for HTTP, TLS, and L4 SNI.
- ACME TLS-ALPN-01 with temporary RFC 8737 challenge certificates and mixed
  `acme-tls/1` / HTTP ALPN selection.
- Raw TCP TLS termination with `tls internal` or a static certificate pair.
- Linux transparent TCP routing with original-destination lookup and
  source-preserving outbound sockets.
- Linux UDP zero-downtime upgrades: listener fd, connected flow fds, and
  bounded flow metadata are transferred through an isolated handoff protocol.
- QUIC datagram passthrough remains available through the UDP proxy; HTTP/3
  termination is explicitly documented as a separate sidecar boundary because
  Pingora 0.8.1 has no native QUIC transport.

### Changed

- HTTP and TLS listeners bind the IPv6 wildcard with dual-stack behavior,
  keeping IPv4 and IPv6 clients on the same listener topology.
- Wildcard ACME identifiers use DNS-01's base-domain TXT record name when the
  existing Cloudflare DNS-01 provider is selected.
- The release and capability documentation now records the Pingora-native,
  application, and custom-integration boundaries for each transport.

### Fixed

- Upstream peer identity and load-balancer reuse now include the selected HTTP
  protocol version, so H1 and H2 peers sharing an address cannot be conflated.
- UDP upgrades no longer reset active flows or rebind the listener, preserving
  the kernel receive queue across a replacement process.

## [v0.3.0] — 2026-08-26

### Added

- **Layer-4 raw TCP proxying** (`tcp <address> { ... }`, L4_PROXY_PLAN P0). A
  top-level listener that relays raw TCP connections to upstreams with
  `lb_policy` (round-robin/random/source-IP hash), `connect_timeout` /
  `idle_timeout` (a true inactivity timeout reset by traffic in either
  direction), `max_connections` admission, and active TCP-connect `health_check`
  probes. IPv6 addresses supported. Prometheus metrics (`raddex_l4_tcp_*`) and
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
  records (`raddex_l4_udp_*`). UDP and TCP may share a port. Zero-downtime
  upgrades do not transfer UDP flows (documented restart path).
- **Layer-4 DNS refresh** (L4 plan): hostname upstreams are re-resolved
  periodically; the resolved set is swapped for new connections only, and a
  transient refresh failure keeps last-known-good (`raddex_l4_tcp_dns_refresh_failures_total`).
- **Implicit HTTP-01 listener on :80.** A config with named sites but no site on
  port 80 now binds a plain-HTTP `:80` listener that serves only the ACME
  challenge, so automatic HTTPS actually completes without an explicit `:80`
  catch-all. `dns_challenge` (DNS-01) skips it; an explicit `:80` catch-all is
  never duplicated.
- **Compression minimum size.** Responses smaller than 64 bytes are served
  uncompressed — the codec framing made them larger than the payload.
- **Hidden files are never served by `file_server`.** Any path segment starting
  with `.` (`.env`, `.git/`, `.htaccess`) is rejected with 404.
- CHANGELOG.md, plus a `raddex.service` systemd unit example.

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
- **HTTP/2 requests are routed correctly.** raddex advertises `h2` but site
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
- `raddex import caddyfile|nginx` migration tool.
- Community workflows (issue/PR templates, CODEOWNERS).

## [v0.1.0] — 2026-08-05

### Added

- Initial release: Raddexfile DSL, `reverse_proxy`, `file_server`, `encode`,
  `redir`, SIGHUP hot reload, ACME automatic HTTPS (HTTP-01, verified against
  Pebble), structured access log, Prometheus metrics, release installer.

[Unreleased]: https://github.com/chulingera2025/raddex/compare/v0.3.5...HEAD
[v0.3.5]: https://github.com/chulingera2025/raddex/compare/v0.3.0...v0.3.5
[v0.3.0]: https://github.com/chulingera2025/raddex/compare/v0.2.10...v0.3.0
[v0.2.10]: https://github.com/chulingera2025/raddex/compare/v0.2.1...v0.2.10
[v0.2.1]: https://github.com/chulingera2025/raddex/compare/v0.1.2...v0.2.1
[v0.1.2]: https://github.com/chulingera2025/raddex/compare/v0.1.0...v0.1.2
[v0.1.0]: https://github.com/chulingera2025/raddex/releases/tag/v0.1.0
