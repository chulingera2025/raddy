# Raddexfile Specification

> The Raddexfile is a **public interface** that is hard to change after release.
> This document is the source of truth.
>
> **Red line**: any syntax not specified here must be added here before it is
> implemented — never decide syntax on the fly.

This specification describes the syntax and behavior shipped in `v0.3.5`.
New syntax must be documented here before it is implemented. A feature boundary
or deployment prerequisite belongs in the architecture and operations records,
not in an untracked implementation plan.

Every directive listed as **Available** below is part of the `v0.3.5`
configuration contract. A release may add syntax only by updating this document
and its validation coverage together.

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

- By default, raddex **does not trust** any upstream `X-Forwarded-For`; it uses
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
| `dns_challenge` | `dns_challenge <provider> <value>` or block form `dns_challenge <provider> { <field> <value>... }` — DNS-01 issuance via a DNS provider (Cloudflare today; see Section 5.3) | Available |
| `tls_alpn_challenge` | prove control via TLS-ALPN-01 on port 443 instead of HTTP-01 (see Section 5.8) | Available |
| `tls` | per-site TLS source and options: `tls [<cert> <key> \| internal]`, `min_version`, `max_version`, `ciphers`, `client_auth` (see Section 5.7) | Available |
| `rewrite` | `rewrite <to>` — rewrite the request URI before forwarding; takes no matcher, always applied (modifier; see Section 5.9) | Available |
| `handle_path` | like `handle`, but strips the matched path prefix from the URI (see Section 5.9) | Available |
| `respond` | `respond <matcher> <status> [<body>]` — answer directly with a status/body; accepts an inline matcher (terminal; see Section 5.9) | Available |
| `error` | `error <matcher> [<status>] [<message>]` — trigger the internal error response; accepts an inline matcher (terminal; see Section 5.9) | Available |
| `basic_auth` | `basic_auth <user> <bcrypt-hash>` — HTTP Basic auth guard (see Section 5.10) | Available |
| `forward_auth` | `forward_auth <target>` — delegate auth to an upstream (see Section 5.10) | Available |
| `import` / `(name)` | `import <file\|name>` multi-file includes / snippets, `{$ENV}` expansion (see Section 5.12) | Available |
| `access_log` | `access_log <path> [format=<json|common>]` or `off` (see Section 5.13) | Available |

**Single-instance vs cluster rate limiting**: rate limiting is per-instance
(each instance counts independently); cluster-wide (shared) counting requires
external Redis and is a later optional feature — no grammar slot is reserved
for it here.

**`file_server` runtime semantics**: `file_server` serves the file at
`root` + the **full request path** (including the `handle` prefix) —
`handle /static/* { root /var/www; file_server }` maps `/static/foo` to
`/var/www/static/foo`. Directories serve their `index.html`; `..` traversal is
rejected with 404; only GET/HEAD are allowed. **Hidden files are never
served**: any path segment beginning with `.` (`.env`, `.git/`, `.htaccess`)
is rejected with 404, except the `.well-known` directory (RFC 8615 well-known
URIs are public discovery endpoints). `encode` applies to `file_server`
responses too, and a body smaller than 64 bytes is left uncompressed (the
codec framing would make it larger).

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
  back automatically once restored. **If every upstream is unhealthy, raddex
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

By default, raddex proves domain control with **HTTP-01** on its plain-HTTP
listener. When port 80 is unreachable (a network that blocks it, or a
DNS-only deployment), set `dns_challenge` to prove control by publishing a DNS
TXT record instead.

**Syntax** — two forms, both in the **global block**:

```caddyfile
dns_challenge <provider> <value>        # shorthand: one credential
dns_challenge <provider> { ... }        # block: any number of credentials
```

The shorthand is available only for a provider that needs exactly one
credential, and fills that credential. The block form takes one
`<field> <value>` line per credential and works for every provider.

- **Semantics**: when set, every certificate on this instance is issued via
  **DNS-01** — raddex publishes `_acme-challenge.<host>` TXT records through the
  provider's API while the order is being validated, then removes them. Without
  `dns_challenge`, behavior is unchanged (HTTP-01).
- **Validation**: the provider keyword and its credential fields are checked by
  `raddex check`. A required credential that is missing or empty, an unknown
  field name, and a duplicated field are all configuration errors.
- **Security**: every credential value is a secret. Raddex redacts them from its
  diagnostic output, but the Raddexfile itself holds them in cleartext — keep it
  out of version control, or inject the values with `{$ENV}` placeholders
  (Section 5.12).

**Providers**

| Provider | Credential | Required | Notes |
|---|---|---|---|
| `cloudflare` | `api_token` | yes | Must have **Zone: DNS: Edit** on the zone. |

Adding a provider does not change this grammar: providers are registry entries
in `src/server/dns/`, and the parser, the validator, and the error messages are
all derived from what the provider declares. See `CONTRIBUTING.md`.

```caddyfile
{
    acme_email ops@example.com
    dns_challenge cloudflare {$CLOUDFLARE_API_TOKEN}
}

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

The block form is equivalent, and is the shape a multi-credential provider
uses:

```caddyfile
{
    acme_email ops@example.com
    dns_challenge cloudflare {
        api_token {$CLOUDFLARE_API_TOKEN}
    }
}
```

### 5.4 Upstream TLS (`reverse_proxy` to HTTPS backends)

**Status: Available.**

Upstreams are plain HTTP/1.1 by default. Prefix an upstream with `https://` to
talk TLS HTTP/1.1 to the backend. Use `h2://` for TLS HTTP/2 or `h2c://` for
cleartext prior-knowledge HTTP/2:

```caddyfile
reverse_proxy https://127.0.0.1:8443
reverse_proxy h2://127.0.0.1:9443
reverse_proxy h2c://127.0.0.1:9080

reverse_proxy {
    to https://10.0.0.1:8443 https://10.0.0.2:8443
    tls_servername api.internal
    tls_ca /etc/raddex/root-ca.pem
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
  - `tls_ca <pem-file>`: root CA(s) used to verify the upstream certificate;
    repeatable. When set, verification trusts **only** the listed CAs — the
    system trust roots are not consulted, so include any system roots you still
    need in the file(s).
  - `tls_cert <cert-file> <key-file>`: a client certificate presented to the
    upstream (mutual TLS to the backend).
- **Semantics**: by default the upstream certificate is verified against the
  system trust roots and its hostname must match `tls_servername` (or the
  upstream host). Verification failures surface as **502 Bad Gateway**.

### 5.5 WebSocket and protocol upgrades

**Status: Available.**

`reverse_proxy` forwards HTTP `Upgrade` requests (WebSocket and similar
`Connection: upgrade` protocols) transparently: the client's upgrade request
goes upstream, and once the upstream answers `101 Switching Protocols` raddex
tunnels the connection bidirectionally.

- No directive is needed — this is the default behavior of `reverse_proxy` for
  HTTP/1.1 upgrade requests.
- Upgrades are end-to-end: raddex does not terminate the upgraded protocol; the
  backend must speak it.
- `header_up` / `header_down` still apply to the upgrade request/response
  headers; `encode` never applies to a `101` (upgraded) response.

### 5.6 HTTP/2

**Status: Available.**

- **Downstream**: TLS listeners (port 443) advertise `h2` via ALPN and serve
  HTTP/2 to clients that support it, falling back to HTTP/1.1 otherwise. This is
  the default.
- **Cleartext (h2c)**: a plain listener remains HTTP/1.1 for clients; upstream
  prior-knowledge h2c is available with the explicit `h2c://` scheme.
- **Upstream**: use `h2://host:port` for TLS HTTP/2 with ALPN `h2`,
  or `h2c://host:port` for cleartext prior-knowledge HTTP/2.
  `https://` and bare targets retain their existing HTTP/1.1 behavior.
- The `h2c://` form does not use the obsolete HTTP/1.1 Upgrade
  mechanism; the upstream must accept the HTTP/2 connection preface directly.
- The advertised ALPN set is fixed (`h2` preferred, `http/1.1` fallback) on
  every TLS listener; it is not configurable per site.

### 5.7 `tls` directive (per-site TLS options, manual certificates, mTLS)

**Status: Available.**

Named sites normally get certificates automatically from ACME. The `tls`
directive in a site block customizes TLS for that site; a named site that has a
`tls` directive serves its port over **TLS** (port 443 by default, or the
site's explicit port when one is given):

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
- **Options** (all optional; combine freely; each on its own `tls` line):
  - `tls min_version <1.2|1.3>` / `tls max_version <1.2|1.3>` — restrict the
    negotiated TLS protocol version for this site.
  - `tls ciphers <list>` — an OpenSSL cipher suite list (e.g.
    `ECDHE-ECDSA-AES128-GCM-SHA256`); space-separated names are joined with `:`.
  - `tls client_auth <optional|require> <ca-file>` — mutual TLS: verify the
    client certificate against `ca-file`. `require` rejects clients without a
    valid certificate; `optional` requests one but accepts clients without.
- Options are keyed **per (host, port)** and applied during the handshake when a
  client connects to that site; a reload updates them without rebuilding the
  listener. The advertised ALPN set is not per-site — Section 5.6's
  `h2`/`http/1.1` default applies to every TLS listener.
- A site whose `tls` source is static or internal is excluded from ACME
  issuance for that hostname (both startup and on-demand).

### 5.8 TLS-ALPN-01 challenge

**Status: Available.** `tls_alpn_challenge` uses the OpenSSL listener's
low-level ClientHello and ALPN callbacks together with a temporary RFC 8737
challenge certificate. It is mutually exclusive with `dns_challenge` and
requires ACME sites to use the standard TLS port 443.

Domain control could otherwise be proven by answering an ACME challenge over the
`acme-tls/1` ALPN protocol on port 443, when HTTP-01 (port 80) is blocked but
port 443 is reachable:

- **Syntax**: `tls_alpn_challenge`, in the **global block**.
- **Semantics**: when set, certificate issuance uses **TLS-ALPN-01**: the ACME
  server opens a TLS connection to port 443 offering only `acme-tls/1`, and
  raddex answers with a short-lived validation certificate containing the
  `acmeIdentifier` extension. HTTP-01 is not enabled as a fallback.

### 5.9 Matchers, `rewrite`, `handle_path`, `respond`, `error`

**Status: Available.**

**Matchers** generalize the path-only inline matcher. A matcher is a sequence of
matcher terms; all terms must match (AND). A bare value starting with `/` is
shorthand for `path`:

- `path <prefix>` — the request path equals the prefix or falls under it
  (`/api` matches `/api` and `/api/...`, not `/apix`). A trailing `*` is
  stripped (`/api/*` ≡ `/api`); the prefix `/` matches every path.
- `host <host>` — the normalized Host header (port stripped, trailing dot
  stripped, ASCII-lowercased) equals the value.
- `method <method>` — the request method equals the value (e.g. `GET`).
- `header <name> <value>` — request header `name` equals `value` (name
  case-insensitive; value exact).
- `query <key> <value>` — a query parameter `key` whose value equals `value`.
- `remote_ip <cidr>...` — the real client IP (per the Section 4 trust model) is
  within the listed network.
- `protocol <http|https>` — the transport of the listener that received the
  request.
- A term prefixed with `!` negates it (e.g. `!path /admin/*`).

Matchers attach to a directive or `handle` block: `handle <matcher> { ... }`,
`reverse_proxy <matcher> { to ... }`. Multiple matcher terms may follow a
directive directly: `handle path /a/* host example.com { ... }`.

New directives introduced with matchers:

- `rewrite <to>`: a **modifier** that rewrites the request URI before it is
  forwarded. It takes no matcher and **always applies** to whichever terminal
  serves the block. The terminal still serves the request, but the upstream
  sees the rewritten path (placeholders `{host}`, `{uri}`, `{remote_host}` are
  supported). Conditional rewrites belong inside a `handle` block.
- `handle_path <matcher> { ... }`: like `handle`, but the matched path prefix is
  stripped from the URI before the block's terminal runs — so
  `handle_path /api/* { reverse_proxy }` forwards `/users/1`, not
  `/api/users/1`.
- `respond <matcher> <status> [<body>]`: a **terminal** that answers directly
  with the given status and optional body (the matcher is optional — omitted
  means always match).
- `error <matcher> [<status>] [<message>]`: a **terminal** that triggers raddex's
  internal error response with the given status (default **500**) and optional
  message.

```caddyfile
api.example.com {
    handle_path /api/* {
        reverse_proxy 127.0.0.1:8080
    }
    handle path /status method GET {
        respond 200 ok
    }
    rewrite /app/{uri}
    reverse_proxy 127.0.0.1:9000
}
```

> Matcher terms are space-separated and ANDed — there are no parentheses or `&&`
> operators (`handle path /status method GET`, not `handle (path /status && method GET)`).
> Matchers also attach to `reverse_proxy`, `respond`, and `error` the same way.

### 5.10 `basic_auth` / `forward_auth`

**Status: Available.**

- `basic_auth <user> <bcrypt-hash>`: a **guard** requiring HTTP Basic
  authentication. Several `basic_auth` directives build the user table; a request
  must present credentials for one of them whose password verifies against the
  bcrypt hash, otherwise **401** with `WWW-Authenticate: Basic`. Generate hashes
  with `htpasswd -B`.
- `forward_auth <target>`: a **guard** that delegates authentication to upstream
  `target` (`host:port`): raddex forwards a request (carrying the original
  `Authorization` and `X-Forwarded-For`) and grants access only on a **2xx**
  response; a **403** is passed through and anything else yields **401**. Response
  headers from the auth upstream (e.g. an identity header) are copied onto the
  request before the real upstream sees it.

Both are guards like `rate_limit`: they apply to whichever terminal serves the
block, and inside a `handle` block only to that block's terminal.

```caddyfile
api.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:8080
}
```

### 5.11 `encode` algorithms

**Status: Available.**

`encode` accepts `br` (Brotli) in addition to `gzip` and `zstd`; argument order
is still the server preference — `encode br zstd gzip` prefers Brotli. An
algorithm is used only when listed; it is negotiated against the client's
`Accept-Encoding`.

### 5.12 `import`, snippets, and environment variables

**Status: Available.**

- **`import <file>`**: splices the contents of another Raddexfile at that point.
  Paths are relative to the importing file. Imports may nest only to a bounded
  depth; an import cycle (tracked by canonical path) and an imported file over
  the size limit are **errors** — never a silent truncation. A site block may
  import a file whose directives belong to that site.
- **Snippets**: a top-level block named `(name) { ... }` defines a reusable
  snippet; `import name` splices it at that point. Snippets are local to the
  file that defines them.
- **Environment variables**: a directive argument `{$ENV_VAR}` is replaced by
  the value of `ENV_VAR` at parse time; a missing variable is a validation
  error. This works anywhere an argument appears (upstream targets, `root`,
  `tls` paths, …). Expansion is **token-level**: the value becomes a single
  argument, so a value containing spaces, `#`, braces, or newlines cannot
  change the configuration's structure.

```caddyfile
(base) {
    rate_limit remote_ip 100r/s
    header_up X-Raddex true
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

**Status: Available.**

The `--access-log` CLI flag appends JSON access logs. The Raddexfile can
configure access logging more precisely:

- Global block: `access_log <path> [format=<json|common>]` sets the log file and
  format (default `json`); `access_log off` disables it for the whole instance.
  The `--access-log` flag still wins when both are set.
- Site block: `access_log off` disables access logging for that site only —
  every terminal type (`reverse_proxy`, `file_server`, `redir`, `respond`,
  `error`) is excluded.
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
  or TLS-ALPN-01 when `tls_alpn_challenge` is configured (Section 5.8). A `tls` source
  of static or internal certs (Section 5.7) opts a site out of ACME. SNI returns
  the matching certificate, and the 443 listener uses SNI dynamic certificates
  (cached in `raddex_certs/`, reused on restart). Certificates are renewed
  automatically within 30 days of expiry.
- **Implicit HTTP-01 listener on :80**: HTTP-01 is answered on a plain-HTTP
  listener, so when a config has named sites but no site on port 80, raddex
  binds an implicit plain-HTTP `:80` listener that serves only the ACME
  challenge (other requests to it get 404). Without it, the ACME server could
  never reach the challenge and issuance would hang. Configuring `dns_challenge`
  (DNS-01) skips the implicit listener — DNS deployments chose it precisely
  because port 80 is unavailable. An explicit `:80` catch-all already answers
  the challenge, so it is never duplicated.
- **Explicit named-site ports**: `api.example.com:8081 { ... }` binds a named
  site to a non-standard port (for local multi-port deployment and testing);
  the default is 443 when the port is omitted. IPv6 literal addresses
  (`[::1]:8080`) are supported; the Host header uses the bracketed form. A named site with a `tls` directive
  (Section 5.7) serves its port over TLS even when it is not 443.
- **Fallbacks**: a missing or malformed Host → `400 Bad Request`; a valid but
  unmatched Host (with no catch-all) → `404 Not Found`. There are no
  configurable error pages.
- **Non-standard ports**: `:8443`.
- **Catch-all**: `:80` serves every request on that listener that matches no
  named site — commonly used for HTTP→HTTPS redirects (part of the auto-HTTPS
  UX).
- **Shared site block for multiple domains** (`a.example.com, b.example.com { ... }`):
  available. The body is cloned into one independently addressable named site
  per host; duplicate host/port pairs are rejected.
- **Wildcard site names** (`*.example.com`) match exactly one left-most
  label, never the apex or a multi-label prefix. Exact names take precedence
  over wildcards, and a more-specific wildcard suffix wins.

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

> The example keeps ACME on its default HTTP-01 path. Set
> `tls_alpn_challenge` in the global block when port 80 is unavailable.

## 8. Layer-4 listeners (TCP, SNI passthrough, and UDP)

**Status: Available (TCP/SNI/UDP/TLS termination/transparent TCP) in
`v0.3.5`.**

A `tcp` block is a **top-level listener**, a peer of an HTTP site block. It
proxies raw TCP connections (no HTTP parsing) to one or more upstreams. TLS is
passed through by default; an optional `tls` line terminates it before relay.

```caddyfile
tcp :3306 {
    to db-1.internal:3306 db-2.internal:3306
    lb_policy round_robin          # round_robin | random | ip_hash
    connect_timeout 3s
    idle_timeout 5m
    max_connections 10000
    health_check {
        interval 10s
        timeout 2s
        consecutive_failures 3
        consecutive_successes 2
    }
}

tcp :8443 {
    tls internal
    to 127.0.0.1:9443
}
```

- **Listen address**: an IP literal, or `:port` for all interfaces; IPv6 is
  bracketed (`tcp [::1]:8080`, `udp [::1]:53`). TCP and UDP may share an address and port, but
  two TCP listeners whose binds overlap (a wildcard overlaps any specific bind)
  are rejected, as is a raw-TCP listener on an HTTP site's port.
- **`to <host>:<port>...`**: at least one upstream. Hostnames are resolved at
  startup and re-resolved periodically (default 60s; the resolved address set
  is swapped for new connections only); a transient refresh failure keeps the
  last-known-good addresses (`raddex_l4_tcp_dns_refresh_failures_total` counts
  it). An unresolvable upstream at startup is an error.
- **SNI routing** (`sni <name> <host:port>` + optional
  `fallback <host:port>`, L4 P1): a `tcp` listener with `sni` lines routes TLS
  connections by the ClientHello's exact or one-label wildcard SNI — without
  terminating TLS (the ClientHello is inspected in a bounded prefix and
  forwarded unchanged). Each `sni` maps a name to its own upstream; an
  unknown/absent/malformed/oversized SNI goes to `fallback` when set, otherwise
  the connection is closed. Exact names win over wildcards, and `sni` and
  `to` are mutually exclusive. `health_check` does not apply to SNI mode.
- **L4 TLS termination**: add `tls internal` or
  `tls <cert-file> <key-file>` inside a `tcp` block. Pingora completes
  the TLS handshake and the app receives the decrypted byte stream; this mode
  uses the shared `to` upstream set and cannot be combined with SNI
  passthrough or `transparent`.
- **Transparent TCP mode**: add `transparent` to a `tcp` block together
  with a `to` fallback. On Linux, raddex binds a socket with
  `IP_TRANSPARENT`, reads the original destination from the Pingora socket
  digest, and binds outbound connections to the original client address. It
  requires `CAP_NET_ADMIN` (or an equivalent service capability),
  netfilter TPROXY rules, and policy routing. It is custom Linux integration
  and is not available on Windows. Because the listener is custom-owned, a
  transparent TCP configuration must use a normal restart rather than
  `raddex upgrade`.
- **`lb_policy`** reuses the HTTP policies: `round_robin` (default), `random`,
  and `ip_hash` (source-IP stickiness — the same client stays on the same
  upstream).
- **`connect_timeout`** bounds a single upstream connect (default `5s`);
  **`idle_timeout`** is a *true* inactivity timeout reset by traffic in either
  direction (default `5m`, so a long-lived active connection never times out);
  **`max_connections`** caps concurrent connections (default `10000`; rejected
  connections are counted in metrics).
- **`health_check { ... }`** runs active TCP-connect probes with the same
  defaults as HTTP (`5s` interval, `2s` timeout, `3` consecutive failures,
  `2` consecutive successes). An unhealthy upstream is skipped; when every
  upstream is unhealthy the connection is refused.
- Each closed connection emits a typed access-log line (JSON, distinct from the
  HTTP access log) and Prometheus metrics (`raddex_l4_tcp_*`, labelled by
  listener).
- **UDP proxying** (`udp <address> { to ... lb_policy idle_timeout max_flows
  max_datagram_size recv_buffer send_buffer }`, L4 P2): proxies datagrams. Each
  client (address + port) maps to a **flow** with its own connected upstream
  socket (the ephemeral local port demultiplexes responses). Selection happens
  once per flow — `ip_hash` pins the client *IP* while the flow identity still
  includes the port. Bounds: `max_flows` caps the table (oldest-first
  eviction), `idle_timeout` evicts idle flows, `max_datagram_size` drops and
  counts oversized datagrams, and `recv_buffer`/`send_buffer` size the sockets
  (0 = OS default). IPv4 and IPv6 upstreams are supported. UDP and TCP may
  share an address and port. Metrics:
  `raddex_l4_udp_*`. UDP zero-downtime upgrades are available on Linux:
  raddex transfers the listener fd, every connected upstream flow fd, and
  bounded flow metadata through a private handoff manifest. The kernel receive
  queue remains attached to the transferred listener, so flows continue without
  a rebind gap. If the handoff fails, the upgrade fails closed rather than
  claiming success.
- **QUIC passthrough**: the UDP proxy can forward QUIC packets as ordinary
  datagrams, but Pingora 0.8.1 has no native QUIC/HTTP/3 stack. This does not
  terminate QUIC, route HTTP/3 requests, or support QUIC connection migration;
  use a dedicated QUIC/HTTP/3 sidecar for those functions.
- **Reload semantics**: a SIGHUP reload applies the new upstream set, policy,
  limits, and timeouts to *new* connections; existing connections keep their
  selected upstream. Changing a listener's bind address is a **topology
  change** and is rejected with an error directing the operator to use a normal
  restart. The zero-downtime upgrade requires the same listener topology.

## 9. Compatibility and boundaries

- Any syntax detail not covered here must be documented before implementation.
- The Cloudflare DNS-01 provider is the provider included in `v0.3.5`; other
  providers are outside this release's configuration contract.
- UDP can carry QUIC datagrams as passthrough, but QUIC/HTTP-3 termination,
  HTTP/3 routing, and connection migration require a separate protocol service.
- Listener topology changes are not reloadable. Use a normal restart; a
  zero-downtime upgrade is valid only when the listener topology is unchanged.
