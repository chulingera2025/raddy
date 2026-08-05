# Raddyfile Specification

> The Raddyfile is a **public interface** that is hard to change after release.
> This document is the source of truth.
>
> **Red line**: any syntax not specified here must be added here before it is
> implemented — never decide syntax on the fly.

Status legend: **Available** = implemented; **Planned** = scheduled; **Future** = not yet scheduled.

## 1. Design philosophy: explicit write-order execution

Directives inside a site block **execute strictly in write order**. This is the
core differentiator from Caddy (which has an internal implicit directive-order
table). The trade-off — users must order directives sensibly (e.g. auth before
business routes) — is explicit and predictable, better than an implicit table
that needs separate documentation.

## 2. Matching semantics: terminal / modifier directives, and `handle`

A site block is compiled into two parts at parse time; the runtime does not
interpret it line by line:

- **Terminal directives** (`reverse_proxy`, `file_server`, `redir`) decide
  **which directive serves** the request. They may carry an inline matcher,
  e.g. `reverse_proxy /api/* { to ... }`; an unmatched terminal is a **no-op**
  and execution continues with the next directive; one without a matcher always
  matches. The first matching terminal ends site execution.
- **Modifier directives** (`header_up`, `header_down`, `encode`) are
  **declarative transforms**; they do not take part in the "who serves"
  decision. Wherever they appear in a block (before or after a terminal), they
  apply to whichever terminal serves that block; modifiers inside a `handle`
  block apply only to that block's terminal.
- **`handle /path { ... }`**: a mutually-exclusive matching block. If the path
  matches, the block's directives run and **matching stops**; if not, execution
  continues. `handle` is for path grouping and "match one and stop" scenarios.
- Caddy's `route` (run all matching blocks in order) is **not** introduced — it
  overlaps with the default order semantics and is a major source of Caddy
  confusion.

> Corollary: a modifier may appear after its terminal (e.g. `header_up` after
> `reverse_proxy`) and still take effect on the request headers (see the §7
> example). This is declarative semantics, not positional line-by-line
> interpretation.

```caddyfile
handle /admin/* {
    # auth gate; no further blocks are matched after a hit
}

handle /static/* {
    root /var/www/html
    file_server
}
```

## 3. Global configuration block

A bare `{ ... }` at the start of the file is the global block, carrying
global items (ACME email, log level, etc.):

```caddyfile
{
    acme_email ops@example.com
    log_level info
}

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

- **Detection rule**: a leading `{` token (not a domain) is the global block.
- `admin 127.0.0.1:2019 { ... }` is placeholder syntax for a future admin API;
  only the grammar slot is reserved here.

## 4. Trusted proxies and the real client IP

Rate limiting, access logging, and WAF all depend on whether the client IP is
trusted, so the default must be pinned down:

- By default, raddy **does not trust** any upstream `X-Forwarded-For`; it uses
  the TCP peer address directly.
- Only after `trusted_proxies` networks are configured does it parse the
  `X-Forwarded-For` chain from the rightmost untrusted address.
- Both the global block and site blocks may set it; the site block wins.

## 5. Directives and parameter semantics

| Directive | Syntax | Status |
|---|---|---|
| `reverse_proxy` | `reverse_proxy <target>` or block form `to <upstream>...`; `to` supports **multi-target round-robin**; optional `lb_policy` / `health_check` inside the block (see §5.1) | Available |
| `handle` | mutually-exclusive matching block (see §2) | Available |
| `header_up` / `header_down` | request / response header rewrite | Available |
| `root` | the path, written directly in a scoped block; **no** redundant Caddy `root *` wildcard | Available |
| `file_server` | static file hosting | Available |
| `encode` | argument order = priority: `encode zstd gzip` prefers zstd when the client supports both | Available |
| `redir` | `redir <target> [code]`, default `308`; `code` is a 3xx number or the keywords `permanent`(=308) / `temporary`(=302); placeholders `{host}`, `{uri}` | Available |
| `log_level` | global log level (`info` / `debug` / `warn` / `error`) | Available |
| `acme_email` | ACME registration email (required by Let's Encrypt) | Available |
| `rate_limit` | `rate_limit remote_ip 50r/s burst=100` (**single-instance** rate limit; `remote_ip` matcher in §8) | Planned |
| `jwt` | `jwt { issuer <url> audience <name> }` | Planned |
| `trusted_proxies` | trusted network list (see §4) | Planned |
| `snippet` / `import` | reusable snippets `(name) { ... }` / multi-file includes | Future |

**Single-instance vs cluster rate limiting**: rate limiting is per-instance
(each instance counts independently); cluster-wide (shared) counting requires
external Redis and is a later optional feature — no grammar slot is reserved
for it here.

**`file_server` runtime semantics**: `file_server` serves the file at
`root` + the **full request path** (including the `handle` prefix) —
`handle /static/* { root /var/www; file_server }` maps `/static/foo` to
`/var/www/static/foo`. Directories serve their `index.html`; `..` traversal is
rejected with 404; only GET/HEAD are allowed. `encode` applies to `file_server`
responses too.

### 5.1 `lb_policy` / `health_check` (sub-directives of the `reverse_proxy` block)

- They only appear in the **block form** of `reverse_proxy`; omitted means the
  default round-robin (the v0.1 behavior).
- `lb_policy round_robin | random | ip_hash`: the selection algorithm.
  `round_robin` (default) rotates; `random` picks at random; `ip_hash` is a
  consistent hash on the client IP (per-IP session stickiness).
- `health_check { ... }`: **active health check** (TCP connect probe). Every
  sub-parameter is optional and falls back to its default:
  - `interval <dur>`: probe period, default `5s`.
  - `timeout <dur>`: per-probe timeout, default `2s`.
  - `consecutive_failures <n>`: remove an upstream only after N consecutive
    failures (flapping suppression), default `3`.
  - `consecutive_successes <n>`: restore an upstream only after M consecutive
    successes (flapping suppression), default `2`.
  - `<dur>` is a number plus a unit (`ms` / `s` / `m` / `h`), or a bare number
    meaning seconds.
- Runtime semantics: an upstream marked unhealthy is never selected; it flows
  back automatically once restored. **If every upstream is unhealthy, raddy
  returns 502.** Health state is process-lifetime and survives SIGHUP reloads
  (ADR-011); it is rebuilt only when the upstream list, policy, or health-check
  parameters change.

```caddyfile
reverse_proxy {
    to 10.0.0.1:8000 10.0.0.2:8000
    lb_policy round_robin
    health_check {
        interval 5s
        timeout 2s
        consecutive_failures 3
        consecutive_successes 2
    }
}
```

## 6. Site selection, ports, catch-all, and multiple sites

- **Site selection is scoped per listener**: a request is matched only against
  the candidate set of the listener it arrived on — TLS listeners by SNI, plain
  HTTP listeners by the normalized Host (port stripped, trailing dot stripped,
  ASCII-lowercased). The candidate set is the named sites on that port plus the
  `:port` catch-all.
- **Named sites default to port 443**: `api.example.com` (no port) binds 443
  (TLS). Automatic HTTPS is active: named sites obtain certificates via ACME
  (HTTP-01), SNI returns the matching certificate, and the 443 listener uses
  SNI dynamic certificates (cached in `raddy_certs/`, reused on restart).
  Renewal is deferred; the disk cache is reused across restarts.
- **Explicit named-site ports**: `api.example.com:8081 { ... }` binds a named
  site to a non-standard port (for local multi-port deployment and testing);
  the default is 443 when the port is omitted. IPv6 literal addresses
  (`[::1]:8080`) are not yet supported.
- **Fallbacks**: a missing or malformed Host → `400 Bad Request`; a valid but
  unmatched Host (with no catch-all) → `404 Not Found`. There are no
  configurable error pages.
- **Non-standard ports**: `:8443`.
- **Catch-all**: `:80` serves every request on that listener that matches no
  named site — commonly used for HTTP→HTTPS redirects (part of the auto-HTTPS
  UX).
- **Shared site block for multiple domains** (`a.example.com, b.example.com { ... }`):
  deferred until the first real use case.

## 7. Example (a complete configuration)

```caddyfile
{
    acme_email ops@example.com
    log_level info
}

# HTTP → HTTPS redirect
:80 {
    redir https://{host}{uri} permanent
}

api.example.com {
    handle /static/* {
        root /var/www/html
        file_server
        encode zstd gzip
    }

    reverse_proxy 127.0.0.1:8080
    header_up X-Real-IP {remote_host}
}
```

> Note: `header_up` written after `reverse_proxy` still affects the request
> headers — it is a modifier directive applying to the terminal that serves this
> block (here `reverse_proxy`). `encode zstd gzip` sits inside the `handle`
> block, so it applies only to that block's `file_server`.

> Other planned directives (`rate_limit`, `jwt`, `lb_policy`, `health_check`, …)
> do not appear in the example, so readers never copy an unparseable config.

## 8. Todo

- The `rate_limit` `remote_ip` matcher and the `jwt` sub-directive grammar must
  be finalized here before implementation (`lb_policy` / `health_check` were
  finalized in v0.1.1, see §5.1).
- Any syntax detail not covered here: **document it before implementing**.
