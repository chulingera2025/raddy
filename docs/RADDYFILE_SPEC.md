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

- **Terminal directives** (`reverse_proxy`, `file_server`, `redir`, `respond`,
  `error`) decide **which directive serves** the request. They may carry an inline
  matcher (see Section 5.9), e.g. `reverse_proxy /api/* { to ... }`; an unmatched
  terminal is a **no-op** and execution continues with the next directive; one
  without a matcher always matches. The first matching terminal ends site
  execution.
- **Modifier directives** (`header_up`, `header_down`, `encode`, `rewrite`) are
  **declarative transforms**; `rate_limit`, `basic_auth`, and `forward_auth` are
  **declarative guards** (see Sections 5.2 / 5.10). None of them takes part in the
  "who serves" decision. Wherever they appear in a block (before or after a
  terminal), they apply to whichever terminal serves that block; modifiers inside
  a `handle` block apply only to that block's terminal.
- **`handle /path { ... }`**: a mutually-exclusive matching block. If the path
  matches, the block's directives run and **matching stops**; if not, execution
  continues. `handle` is for path grouping and "match one and stop" scenarios.
- Caddy's `route` (run all matching blocks in order) is **not** introduced — it
  overlaps with the default order semantics and is a major source of Caddy
  confusion.

> Corollary: a modifier may appear after its terminal (e.g. `header_up` after
> `reverse_proxy`) and still take effect on the request headers (see the Section 7
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

## 4. Trusted proxies and the real client IP

Rate limiting, access logging, and WAF all depend on whether the client IP is
trusted, so the default must be pinned down:

- By default, raddy **does not trust** any upstream `X-Forwarded-For`; it uses
  the TCP peer address directly.
- Only after `trusted_proxies` networks are configured does it parse the
  `X-Forwarded-For` chain from the rightmost untrusted address.
- Both the global block and site blocks may set it; the site block wins.

**Syntax** (global or site block):

```caddyfile
{
    trusted_proxies 10.0.0.0/8 172.16.0.0/12 127.0.0.1
}
```

- `trusted_proxies <cidr>...`; each `<cidr>` is `<address>/<prefix>` or a bare
  address (a single host). IPv4 and IPv6 are both accepted. A later occurrence
  overrides an earlier one in the same scope.
- A site-block `trusted_proxies` overrides the global list **for that site
  only**; other sites keep the global list.
- **Semantics**: the real client IP is the TCP peer unless the peer is a
  trusted proxy; then it is the rightmost entry of the `X-Forwarded-For` chain
  that is **not** a trusted proxy (malformed entries are skipped). When the
  whole chain is trusted or absent, the trusted peer itself is used.

## 5. Directives and parameter semantics

| Directive | Syntax | Status |
|---|---|---|
| `reverse_proxy` | `reverse_proxy <target>` or block form `to <upstream>...`; `to` supports **multi-target round-robin**; optional `lb_policy` / `health_check` inside the block (see Section 5.1); `https://` upstreams with `tls_servername` / `tls_skip_verify` / `tls_ca` / `tls_cert` (see Section 5.4) | Available |
| `handle` | mutually-exclusive matching block (see Section 2) | Available |
| `header_up` / `header_down` | request / response header rewrite | Available |
| `root` | the path, written directly in a scoped block; **no** redundant Caddy `root *` wildcard | Available |
| `file_server` | static file hosting | Available |
| `encode` | argument order = priority: `encode zstd gzip` prefers zstd when the client supports both; `br` (Brotli) is a valid algorithm | Available |
| `redir` | `redir <target> [code]`, default `308`; `code` is a 3xx number or the keywords `permanent`(=308) / `temporary`(=302); placeholders `{host}`, `{uri}` | Available |
| `log_level` | global log level (`info` / `debug` / `warn` / `error`) | Available |
| `acme_email` | ACME registration email (required by Let's Encrypt) | Available |
| `rate_limit` | `rate_limit <key> <rate> [burst=<n>]` (**single-instance** rate limit; see Section 5.2); key is `remote_ip` or `header <name>` | Available |
| `trusted_proxies` | trusted network list (see Section 4) | Available |
| `dns_challenge` | `dns_challenge cloudflare <api_token>` — DNS-01 issuance via a DNS provider (Cloudflare today; see Section 5.3) | Available |
| `tls_alpn_challenge` | prove control via TLS-ALPN-01 on port 443 instead of HTTP-01 (see Section 5.8) | Planned |
| `tls` | per-site TLS source and options: `tls [<cert> <key> | internal]`, `min_version`, `max_version`, `ciphers`, `alpn`, `client_auth` (see Section 5.7) | Planned |
| `rewrite` | `rewrite <matcher> <to>` — rewrite the request URI before forwarding (modifier; see Section 5.9) | Planned |
| `handle_path` | like `handle`, but strips the matched path prefix from the URI (see Section 5.9) | Planned |
| `respond` | `respond <status> [<body>]` — answer directly with a status/body (terminal; see Section 5.9) | Planned |
| `error` | `error [<status>] [<message>]` — trigger the internal error response (terminal; see Section 5.9) | Planned |
| `basic_auth` | `basic_auth <user> <bcrypt-hash>` — HTTP Basic auth guard (see Section 5.10) | Planned |
| `forward_auth` | `forward_auth <target>` — delegate auth to an upstream (see Section 5.10) | Planned |
| `import` / `(name)` | `import <file|name>` multi-file includes / snippets (see Section 5.12) | Planned |
| `access_log` | `access_log <path> [format=<json|common>]` or `off` (see Section 5.13) | Planned |

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
; it is rebuilt only when the upstream list, policy, or health-check
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

### 5.2 `rate_limit` (declarative guard)

- Syntax: `rate_limit <key> <rate> [burst=<n>]`.
- `<key>` selects what is counted. Two keys are supported:
  - `remote_ip` — the real client IP per the Section 4 trust model (the v0.1.2 key).
  - `header <name>` — the value of request header `<name>` (e.g. `header X-API-Key`). Requests without that header share a single bucket.
- `<rate>`: `<count>r/<unit>`, where the unit is `s` (second), `m` (minute),
  `h` (hour), or `d` (day) — e.g. `50r/s`, `1200r/m`. The count must be ≥ 1.
- `burst=<n>`: the token-bucket capacity, `n ≥ 1`; **default = the rate
  count**. Omitted or explicit.
- Semantics: a **single-node, in-memory token bucket** per (site, terminal,
  key value). The bucket refills continuously at `<rate>` and holds at most
  `burst` tokens; a request that finds no token is rejected with
  **429 Too Many Requests**. State is process-lifetime and survives SIGHUP
  reloads.
- It is a **modifier** (guard): a site-level `rate_limit` guards whichever
  terminal serves the block; inside a `handle` block it guards only that
  block's terminal. Requests that match no terminal (404) are not rate limited.
  When several `rate_limit` directives are in scope each keeps its own counter.

```caddyfile
api.example.com {
    rate_limit remote_ip 100r/s burst=200
    reverse_proxy 127.0.0.1:8080
}
```

### 5.3 `dns_challenge` (DNS-01 via a DNS provider)

By default, raddy proves domain control with **HTTP-01** on its plain-HTTP
listener. When port 80 is unreachable (a network that blocks it, or a
DNS-only deployment), set `dns_challenge` to prove control by publishing a DNS
TXT record instead:

- **Syntax**: `dns_challenge <provider> <api_token>`, in the **global block**.
- **Provider**: `cloudflare` (the only provider today). The token must have
  **Zone: DNS: Edit** permission.
- **Semantics**: when set, every certificate on this instance is issued via
  **DNS-01** — raddy publishes `_acme-challenge.<host>` TXT records through the
  provider's API while the order is being validated, then removes them. Without
  `dns_challenge`, behavior is unchanged (HTTP-01).
- **Security**: the API token is a secret — keep the Raddyfile out of version
  control or protect it accordingly.

```caddyfile
{
    acme_email ops@example.com
    dns_challenge cloudflare <api_token>
}

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

### 5.4 Upstream TLS (`reverse_proxy` to HTTPS backends)

**Status: Available.**

Upstreams are plain HTTP by default. Prefix an upstream with `https://` to talk
TLS to the backend:

```caddyfile
reverse_proxy https://127.0.0.1:8443

reverse_proxy {
    to https://10.0.0.1:8443 https://10.0.0.2:8443
    tls_servername api.internal
    tls_ca /etc/raddy/root-ca.pem
}
```

- **Syntax**: an upstream target accepts an `https://` (or `http://`) scheme
  prefix; the scheme decides whether the upstream connection is TLS. A bare
  `host:port` remains plain HTTP (backward compatible).
- TLS options only appear in the **block form** of `reverse_proxy`, as
  sub-directives:
  - `tls_servername <host>`: the SNI/hostname sent to the upstream (default: the
    upstream host). Required when the upstream address is an IP but its
    certificate is issued for a name.
  - `tls_skip_verify`: disable upstream certificate *and* hostname verification
    (never use in production).
  - `tls_ca <pem-file>`: additional root CA(s) used to verify the upstream
    certificate; repeatable. System roots are always trusted in addition.
  - `tls_cert <cert-file> <key-file>`: a client certificate presented to the
    upstream (mutual TLS to the backend).
- **Semantics**: by default the upstream certificate is verified against the
  system trust roots and its hostname must match `tls_servername` (or the
  upstream host). Verification failures surface as **502 Bad Gateway**.

### 5.5 WebSocket and protocol upgrades

`reverse_proxy` forwards HTTP `Upgrade` requests (WebSocket and similar
`Connection: upgrade` protocols) transparently: the client's upgrade request
goes upstream, and once the upstream answers `101 Switching Protocols` raddy
tunnels the connection bidirectionally.

- No directive is needed — this is the default behavior of `reverse_proxy` for
  HTTP/1.1 upgrade requests.
- Upgrades are end-to-end: raddy does not terminate the upgraded protocol; the
  backend must speak it.
- `header_up` / `header_down` still apply to the upgrade request/response
  headers; `encode` never applies to a `101` (upgraded) response.

### 5.6 HTTP/2

- **Downstream**: TLS listeners (port 443) advertise `h2` via ALPN and serve
  HTTP/2 to clients that support it, falling back to HTTP/1.1 otherwise. This is
  the default.
- **Cleartext (h2c)**: not supported — a plain-HTTP listener stays HTTP/1.1.
- **Upstream**: raddy talks HTTP/1.1 to upstreams today; upstream HTTP/2 is
  future work.
- `alpn <h2 http/1.1>` in the `tls` directive (Section 5.7) overrides the
  advertised ALPN set for a site.

### 5.7 `tls` directive (per-site TLS options, manual certificates, mTLS)

Named sites normally get certificates automatically from ACME. The `tls`
directive in a site block customizes TLS for that site:

```caddyfile
api.example.com {
    tls internal
    reverse_proxy 127.0.0.1:8080
}

intranet.example.com {
    tls /etc/certs/intranet.pem /etc/certs/intranet.key
    reverse_proxy 127.0.0.1:9000
}

secure.example.com {
    tls client_auth require /etc/certs/clients-ca.pem
    reverse_proxy 127.0.0.1:9000
}
```

- **Sources** (at most one):
  - *(omitted)* — ACME (the default; unchanged).
  - `tls internal` — a self-signed certificate generated at startup for
    development; clients must be configured to trust it. No ACME is attempted.
  - `tls <cert-file> <key-file>` — a static PEM certificate chain + private key
    served for this site instead of ACME. Renewal is the operator's job.
- **Options** (all optional; combine freely):
  - `tls min_version <1.2|1.3>` / `tls max_version <1.2|1.3>` — restrict the
    negotiated TLS protocol version.
  - `tls ciphers <list>` — an OpenSSL cipher suite list (e.g.
    `ECDHE-ECDSA-AES128-GCM-SHA256`).
  - `tls alpn <protocols...>` — the advertised ALPN list (e.g. `h2 http/1.1`);
    overrides the Section 5.6 default.
  - `tls client_auth <optional|require> <ca-file>` — mutual TLS: verify the
    client certificate against `ca-file`. `require` rejects clients without a
    valid certificate; `optional` requests one but accepts clients without.
- A site whose `tls` source is static or internal is excluded from ACME
  issuance for that hostname (both startup and on-demand).

### 5.8 TLS-ALPN-01 challenge

Domain control can also be proven by answering an ACME challenge over the
`acme-tls/1` ALPN protocol on port 443, when HTTP-01 (port 80) is blocked but
port 443 is reachable:

- **Syntax**: `tls_alpn_challenge`, in the **global block**.
- **Semantics**: when set, certificate issuance uses **TLS-ALPN-01**: the ACME
  server opens a TLS connection to port 443 offering the `acme-tls/1` ALPN, and
  raddy answers with a validation certificate whose key matches the key
  authorization. HTTP-01 remains available as a fallback when the ACME server
  does not support TLS-ALPN-01.

### 5.9 Matchers, `rewrite`, `handle_path`, `respond`, `error`

**Matchers** generalize the path-only inline matcher. A matcher is a sequence of
matcher terms; all terms must match (AND). A bare value starting with `/` is
shorthand for `path`:

- `path <glob>...` — the request path matches any glob (`*` within a segment,
  `**` across segments). `path /api/*` — shorthand `handle /api/*`.
- `host <host>...` — the normalized Host header equals any value (port stripped,
  ASCII-lowercased).
- `method <method>...` — the request method is one of the listed values (e.g.
  `GET POST`).
- `header <name> <value>` — request header `name` equals `value` (case-insensitive
  name; exact value).
- `query <key> <value>` — a query parameter `key` whose value equals `value`.
- `remote_ip <cidr>...` — the real client IP (per the Section 4 trust model) is
  within any listed network.
- `protocol <http|https>` — the transport of the listener that received the
  request.
- A term prefixed with `!` negates it (e.g. `!path /admin/*`).

Matchers attach to a directive or `handle` block: `handle <matcher> { ... }`,
`reverse_proxy <matcher> { to ... }`. Multiple matcher terms may follow a
directive directly: `handle path /a/* host example.com { ... }`.

New directives introduced with matchers:

- `rewrite <matcher> <to>` or `rewrite <to>`: a **modifier** that rewrites the
  request URI before it is forwarded. The terminal still serves the request, but
  the upstream sees the rewritten path. Matching is not re-run after a rewrite.
- `handle_path <matcher> { ... }`: like `handle`, but the matched path prefix is
  stripped from the URI before the block's terminal runs — so
  `handle_path /api/* { reverse_proxy }` forwards `/users/1`, not
  `/api/users/1`.
- `respond <status> [<body>]`: a **terminal** that answers directly with the
  given status and optional body (a `3xx` status sets the body as `Location`).
- `error [<status>] [<message>]`: a **terminal** that triggers raddy's internal
  error response with the given status (default **500**) and message.

```caddyfile
api.example.com {
    handle_path /api/* {
        reverse_proxy 127.0.0.1:8080
    }
    handle (path /status && method GET) {
        respond 200 ok
    }
    rewrite path /old/* /new/{uri:path}
    reverse_proxy 127.0.0.1:9000
}
```

### 5.10 `basic_auth` / `forward_auth`

- `basic_auth <user> <bcrypt-hash>`: a **guard** requiring HTTP Basic
  authentication. Several `basic_auth` directives build the user table; a request
  must present credentials for one of them whose password verifies against the
  bcrypt hash, otherwise **401** with `WWW-Authenticate: Basic`. Generate hashes
  with `htpasswd -B` (or raddy's own helper once shipped).
- `forward_auth <target>`: a **guard** that delegates authentication to upstream
  `target`: raddy forwards the request (carrying `X-Forwarded-For` and the auth
  headers) and grants access only on a **2xx** response; a **401** is passed
  through as 401 and a **403** as 403. Response headers from the auth upstream
  (e.g. an identity header) are copied onto the request before the real upstream
  sees it.

Both are guards like `rate_limit`: they apply to whichever terminal serves the
block, and inside a `handle` block only to that block's terminal.

```caddyfile
api.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:8080
}
```

### 5.11 `encode` algorithms

`encode` accepts `br` (Brotli) in addition to `gzip` and `zstd`; argument order
is still the server preference — `encode br zstd gzip` prefers Brotli. An
algorithm is used only when listed; it is negotiated against the client's
`Accept-Encoding`.

### 5.12 `import`, snippets, and environment variables

- **`import <file>`**: splices the contents of another Raddyfile at that point.
  Paths are relative to the importing file. Imports may nest (depth-limited). A
  site block may import a file whose directives belong to that site.
- **Snippets**: a top-level block named `(name) { ... }` defines a reusable
  snippet; `import name` splices it at that point. Snippets are local to the
  file that defines them.
- **Environment variables**: a directive argument `{$ENV_VAR}` is replaced by
  the value of `ENV_VAR` at parse time; a missing variable is a validation
  error. This works anywhere an argument appears (upstream targets, `root`,
  `tls` paths, …).

```caddyfile
(base) {
    rate_limit remote_ip 100r/s
    header_up X-Raddy true
}

{
    acme_email ops@example.com
}

api.example.com {
    import base
    reverse_proxy https://{$BACKEND_HOST}:8443
}
```

### 5.13 Access log configuration

The `--access-log` CLI flag appends JSON access logs. The Raddyfile can
configure access logging more precisely:

- Global block: `access_log <path> [format=<json|common>]` sets the log file and
  format (default `json`); `access_log off` disables it for the whole instance.
  The `--access-log` flag still wins when both are set.
- Site block: `access_log off` disables access logging for that site only.
- The `common` format is the classic combined log line (`%h %l %u %t "%r" %>s
  %b "%{Referer}i" "%{User-Agent}i"`).

## 6. Site selection, ports, catch-all, and multiple sites

- **Site selection is scoped per listener**: a request is matched only against
  the candidate set of the listener it arrived on — TLS listeners by SNI, plain
  HTTP listeners by the normalized Host (port stripped, trailing dot stripped,
  ASCII-lowercased). The candidate set is the named sites on that port plus the
  `:port` catch-all.
- **Named sites default to port 443**: `api.example.com` (no port) binds 443
  (TLS). Automatic HTTPS is active: named sites obtain certificates via ACME —
  HTTP-01 by default, DNS-01 when `dns_challenge` is configured (Section 5.3),
  or TLS-ALPN-01 when `tls_alpn_challenge` is set (Section 5.8). A `tls` source
  of static or internal certs (Section 5.7) opts a site out of ACME. SNI returns
  the matching certificate, and the 443 listener uses SNI dynamic certificates
  (cached in `raddy_certs/`, reused on restart). Certificates are renewed
  automatically within 30 days of expiry.
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
    trusted_proxies 127.0.0.1
}

# HTTP → HTTPS redirect
:80 {
    redir https://{host}{uri} permanent
}

api.example.com {
    rate_limit remote_ip 50r/s burst=100

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
> block, so it applies only to that block's `file_server`. `rate_limit` is a
> declarative guard (modifier): it applies to whichever terminal serves the
> site.

> Planned and future directives (`snippet` / `import`, …) do not appear in the
> example, so readers never copy an unparseable config.

## 8. Todo

- Any syntax detail not covered here: **document it before implementing**.
- DNS-01 providers beyond Cloudflare are **deferred** — one GitHub issue per
  provider (community contributions welcome).
- Upstream HTTP/2 is future work (Section 5.6); cleartext h2c is not planned.
