---
title: Directive reference
description: Every Raddyfile directive with its syntax, arguments, and a runnable example.
---

This is the complete reference for the Raddyfile. Each directive follows the same
shape: what it does, its syntax, its arguments, and an example. If you are new to
the language, read [Concepts](../) first for the mental model, and
[Routing & matchers](../../guides/routing/) for how matchers and routing directives
fit together.

## Summary

| Directive | Purpose | Kind |
|---|---|---|
| [`reverse_proxy`](#reverse_proxy) | Proxy a request to one or more upstreams (incl. TLS backends) | terminal |
| [`handle`](#handle) | Match a matcher and serve it with one terminal | terminal (scoped) |
| [`handle_path`](#handle_path) | Like `handle`, but strip the matched path prefix | terminal (scoped) |
| [`respond`](#respond) | Answer directly with a status and optional body | terminal |
| [`error`](#error) | Trigger the internal error response | terminal |
| [`file_server`](#file_server) | Serve static files | terminal |
| [`redir`](#redir) | Redirect the client | terminal |
| [`rewrite`](#rewrite) | Rewrite the request URI before forwarding | modifier |
| [`header_up` / `header_down`](#header_up--header_down) | Rewrite request / response headers | modifier |
| [`encode`](#encode) | Compress responses (gzip / zstd / br) | modifier |
| [`rate_limit`](#rate_limit) | Reject requests beyond a rate (`remote_ip` or `header <name>`) | guard |
| [`basic_auth`](#basic_auth) | Require HTTP Basic authentication | guard |
| [`forward_auth`](#forward_auth) | Delegate authentication to an upstream | guard |
| [`root`](#root) | Set the static file root for a block | helper |
| [`tls`](#tls) | Per-site TLS source, options, and mTLS | config (site) |
| [`access_log`](#access_log) | Configure access logging (global), or disable per site | config |
| [`trusted_proxies`](#trusted_proxies) | Trusted networks for the real client IP | config |
| [`dns_challenge`](#dns_challenge) | DNS-01 issuance via a DNS provider (Cloudflare) | config |
| [`tls_alpn_challenge`](#tls_alpn_challenge) | TLS-ALPN-01 issuance on port 443 | config |
| [`log_level`](#log_level) | Global log level | config |
| [`acme_email`](#acme_email) | ACME registration email | config |
| [`import` / `(name)`](#import-and-snippets) | Multi-file includes / reusable snippets | config (DX) |
| `{$ENV}` | Environment-variable substitution in any argument | token |

## Matchers

**Purpose.** Select which requests a directive or `handle` block applies to.
Matchers generalize the path-only inline matcher from earlier versions.

**Syntax.** A matcher is a sequence of **matcher terms**; all terms must match
(AND). A bare value starting with `/` is shorthand for `path`:

```caddyfile
handle path /status method GET { ... }        # ANDed terms, no parentheses
handle /static/* { ... }                       # bare prefix = path shorthand
reverse_proxy !path /admin/* { to 127.0.0.1:8080 }
```

**Matcher terms.**

| Term | Matches when |
|---|---|
| `path <prefix>` | The request path equals the prefix or falls under it (`/api` matches `/api` and `/api/...`, not `/apix`). A trailing `*` is stripped (`/api/*` ≡ `/api`); the prefix `/` matches every path. |
| `host <host>` | The normalized Host header (port stripped, trailing dot stripped, ASCII-lowercased) equals the value. |
| `method <method>` | The request method equals the value (e.g. `GET`). |
| `header <name> <value>` | Request header `name` equals `value` (name case-insensitive; value exact). |
| `query <key> <value>` | A query parameter `key` whose value equals `value`. |
| `remote_ip <cidr>...` | The **real client IP** (see [Trusted proxies](../trusted-proxies/)) is within the listed network(s). |
| `protocol <http\|https>` | The transport of the listener that received the request. |

A term prefixed with `!` negates it (`!path /admin/*`). Terms are
**space-separated and ANDed** — there are no parentheses or `&&` operators:

```caddyfile
handle path /status method GET { ... }   # correct
handle (path /status && method GET) { ... }  # invalid — no parens / && syntax
```

**Where matchers attach.** Matchers attach to a `handle` / `handle_path` block
and, as inline matchers, to terminal directives — `reverse_proxy`, `respond`, and
`error`:

```caddyfile
reverse_proxy path /api/* { to 127.0.0.1:8080 }
respond method OPTIONS 204
error !path /assets/* 503
```

An inline matcher that does not match makes the terminal a **no-op**: execution
continues with the next directive. A terminal without a matcher always matches.

## `reverse_proxy`

**Purpose.** Forward a request to an upstream service — the core of a reverse
proxy.

**Syntax.**

```caddyfile
reverse_proxy <target>

reverse_proxy [<matcher>] {
    to <upstream>...
    lb_policy round_robin|random|ip_hash
    health_check { ... }
}
```

**Arguments.**

- `<target>` / `<upstream>` — an upstream address. Upstreams are plain HTTP by
  default; prefix one with `https://` to talk TLS to the backend (see
  [Upstream TLS options](#upstream-tls-options)). A bare `host:port` stays plain
  HTTP.
- `to` — list multiple upstreams for round-robin distribution.
- `lb_policy` — the selection algorithm; defaults to `round_robin`. See
  [load balancing](#lb_policy-and-health_check).
- `health_check { ... }` — active health checking of the upstreams. See
  [health checks](#lb_policy-and-health_check).
- `<matcher>` — an optional inline [matcher](#matchers).

**Behavior.** Upstreams are HTTP/1.1 by default. Use `h2://host:port` for
TLS HTTP/2 or `h2c://host:port` for cleartext prior-knowledge HTTP/2.
The h2c form requires the upstream to accept the HTTP/2 connection preface
directly; it does not use HTTP/1.1 Upgrade. WebSocket and other HTTP
`Upgrade` requests are forwarded transparently — see
[WebSocket and protocol upgrades](#websocket-and-protocol-upgrades).

**Example.**

```caddyfile
api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

## `handle`

**Purpose.** Run one terminal for a matching request, then stop. Everything
inside a `handle` block applies only to requests whose matcher matches.

**Syntax.**

```caddyfile
handle <matcher> {
    # terminal (and modifiers) for matching requests
}
```

**Arguments.** A [matcher](#matchers). `handle /static/*` is shorthand for
`handle path /static/*`.

**Behavior.** If the matcher matches, the block's directives run and **matching
stops** — the rest of the site is not consulted. If it does not match, execution
continues past the block. `handle` is the way to group a path (or any matcher)
with a terminal.

**Example.** Serve static assets from disk, proxy everything else to the app:

```caddyfile
example.com {
    handle /static/* {
        root /var/www/html
        file_server
    }

    reverse_proxy 127.0.0.1:8080
}
```

## `handle_path`

**Purpose.** Like `handle`, but the matched path prefix is **stripped** from the
URI before the block's terminal runs.

**Syntax.**

```caddyfile
handle_path <matcher> {
    # terminal (and modifiers)
}
```

**Arguments.** A [matcher](#matchers).

**Behavior.** The block's terminal sees the request without the matched prefix,
so a backend does not have to know the prefix it is mounted under. A trailing
`*` on the path matcher is stripped first.

**Example.** Forward `/api/users/1` to the backend as `/users/1`:

```caddyfile
example.com {
    handle_path /api/* {
        reverse_proxy 127.0.0.1:8080
    }

    reverse_proxy 127.0.0.1:9000
}
```

## `respond`

**Purpose.** Answer the request directly with a status code and an optional
body — no upstream, no file.

**Syntax.**

```caddyfile
respond [<matcher>] <status> [<body>]
```

**Arguments.**

- `<matcher>` — an optional inline [matcher](#matchers).
- `<status>` — a 3-digit HTTP status (100–599).
- `<body>` — an optional response body.

**Behavior.** A terminal: the first matching terminal ends site execution. Use it
for health endpoints, CORS preflight replies, maintenance banners, and other
fixed responses.

**Example.** A health endpoint and a CORS preflight reply on one site:

```caddyfile
api.example.com {
    handle path /health {
        respond 200 ok
    }

    handle method OPTIONS {
        respond 204
    }

    reverse_proxy 127.0.0.1:8080
}
```

## `error`

**Purpose.** Trigger raddy's internal error response with a chosen status and an
optional message.

**Syntax.**

```caddyfile
error [<matcher>] [<status>] [<message>]
```

**Arguments.**

- `<matcher>` — an optional inline [matcher](#matchers).
- `<status>` — the status of the error response; defaults to `500`.
- `<message>` — an optional message included in the response.

**Behavior.** A terminal: it serves the error and ends site execution. Use it to
gate paths with a matcher and return a specific status, for example a `403` for
an area you want to block.

**Example.**

```caddyfile
example.com {
    handle /internal/* {
        error 404 not here
    }

    reverse_proxy 127.0.0.1:8080
}
```

## `file_server`

**Purpose.** Serve files from disk.

**Syntax.**

```caddyfile
file_server
```

**Arguments.** None. The root directory comes from [`root`](#root) in the same
scoped block.

**Behavior.** Serves `root` + the full request path, including any `handle`
prefix — `handle /static/* { root /var/www; file_server }` maps `/static/foo` to
`/var/www/static/foo`. A directory serves its `index.html`. Path traversal
(`..`) is rejected with `404`. Hidden files are never served — any path segment
starting with `.` (`.env`, `.git/`, `.htaccess`) is rejected with `404`, except
the `.well-known` directory (RFC 8615 well-known URIs are public by design).
Only `GET` and `HEAD` are allowed. `encode` applies to `file_server` responses
too; a body smaller than 64 bytes is left uncompressed.

**Example.**

```caddyfile
static.example.com {
    root /var/www/html
    file_server
}
```

## `redir`

**Purpose.** Send the client an HTTP redirect.

**Syntax.**

```caddyfile
redir <target> [code]
```

**Arguments.**

- `<target>` — the redirect location. Placeholders: `{host}`, `{uri}`.
- `<code>` — a 3xx status or a keyword; defaults to `308`. `permanent` = `308`,
  `temporary` = `302`.

**Example.** Redirect every request to HTTPS, preserving host and path:

```caddyfile
:80 {
    redir https://{host}{uri} permanent
}
```

## `rewrite`

**Purpose.** Rewrite the request URI before it is forwarded. A modifier: the
terminal still serves the request, but the upstream sees the rewritten path.

**Syntax.**

```caddyfile
rewrite <to>
```

**Arguments.** `<to>` — the rewritten URI. Placeholders: `{host}`, `{uri}`,
`{remote_host}`.

**Behavior.** Rewrites happen before the terminal runs, so both proxying and
file serving see the new path. For a conditional rewrite, put it inside a
`handle` block.

**Example.** Prefix every request with a version path:

```caddyfile
example.com {
    rewrite /v1/{uri}
    reverse_proxy 127.0.0.1:8080
}
```

## `header_up` / `header_down`

**Purpose.** Add, set, or remove headers on the upstream request
(`header_up`) and on the response sent to the client (`header_down`).

**Syntax.**

```caddyfile
header_up <name> <value>
header_down <name> <value>
```

**Arguments.** `<name>` is the header name; `<value>` is the value or a
placeholder: `{remote_host}` (the TCP client socket address — the direct peer IP,
*not* the trusted-proxy effective client IP), `{host}`, `{uri}`.

**Example.** Pass the client socket address through to the backend:

```caddyfile
api.example.com {
    reverse_proxy 127.0.0.1:8080
    header_up X-Real-IP {remote_host}
}
```

## `encode`

**Purpose.** Compress responses. The **argument order is the priority** — raddy
uses the first algorithm the client also supports.

**Syntax.**

```caddyfile
encode <algorithm>...
```

**Arguments.** `gzip`, `zstd`, `br` (Brotli) — in priority order.

**Behavior.** An algorithm is used only when listed, and is negotiated against
the client's `Accept-Encoding`; if the client supports none of them, the
response is sent uncompressed. `encode` applies to `reverse_proxy` and
`file_server` responses, and never to a `101` (upgraded, e.g. WebSocket)
response.

**Example.** Prefer Brotli, fall back to zstd, then gzip:

```caddyfile
example.com {
    encode br zstd gzip
    reverse_proxy 127.0.0.1:8080
}
```

`encode` applies to `file_server` responses as well:

```caddyfile
static.example.com {
    root /var/www/html
    file_server
    encode zstd gzip
}
```

## `rate_limit`

**Purpose.** Reject requests that exceed a rate with `429 Too Many Requests`.

**Syntax.**

```caddyfile
rate_limit <key> <rate> [burst=<n>]
```

**Arguments.**

- `<key>` — what is counted:
  - `remote_ip` — the real client IP (see [Trusted proxies](../trusted-proxies/)).
  - `header <name>` — the value of request header `<name>` (e.g.
    `header X-API-Key`). Requests without that header share a single bucket.
- `<rate>` — `<count>r/<unit>` where the unit is `s` / `m` / `h` / `d`, e.g.
  `50r/s`, `1200r/m`. The count must be at least 1.
- `burst=<n>` — the token bucket capacity; defaults to the rate count.

**Behavior.** An in-memory token bucket per (site, terminal, key value). It
refills continuously at `<rate>` and holds at most `burst` tokens; a request
with no token is rejected with `429`. `rate_limit` is a guard (modifier): it
guards whichever terminal serves the block, and state survives SIGHUP reloads.
Requests that match no terminal (404) are not rate limited. Several
`rate_limit` directives in scope each keep their own counter. Rate limiting is
per-instance — it is not shared across a cluster.

**Example.** Limit per API key:

```caddyfile
api.example.com {
    rate_limit header X-API-Key 100r/s burst=200
    reverse_proxy 127.0.0.1:8080
}
```

Or per client IP:

```caddyfile
api.example.com {
    rate_limit remote_ip 100r/s burst=200
    reverse_proxy 127.0.0.1:8080
}
```

## `basic_auth`

**Purpose.** Require HTTP Basic authentication for the block.

**Syntax.**

```caddyfile
basic_auth <user> <bcrypt-hash>
```

**Arguments.** `<user>` — the username; `<bcrypt-hash>` — the bcrypt hash of the
password. Several `basic_auth` directives build the user table: a request must
present credentials for one of them whose password verifies against its hash.

**Behavior.** A guard: requests without valid credentials get **401
Unauthorized** with a `WWW-Authenticate: Basic` challenge. Guarded like
`rate_limit` — it applies to whichever terminal serves the block. Generate hashes
with `htpasswd -B`:

```bash
htpasswd -Bbn admin 's3cret'
```

**Example.**

```caddyfile
admin.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:8080
}
```

## `forward_auth`

**Purpose.** Delegate authentication to a dedicated upstream service.

**Syntax.**

```caddyfile
forward_auth <host:port>
```

**Arguments.** `<host:port>` — the auth upstream, e.g. `auth.example.com:4181`.

**Behavior.** A guard: raddy forwards a request to the auth upstream, carrying
the original `Authorization` and `X-Forwarded-For` headers, and grants access
only on a **2xx** response. A **403** from the auth upstream is passed through to
the client; anything else yields **401**. Response headers from the auth
upstream (for example an identity header) are copied onto the request before the
real upstream sees it.

**Example.**

```caddyfile
app.example.com {
    forward_auth 127.0.0.1:4181
    reverse_proxy 127.0.0.1:8080
}
```

## `root`

**Purpose.** Set the filesystem root that [`file_server`](#file_server) serves.

**Syntax.**

```caddyfile
root <path>
```

**Arguments.** `<path>` — a filesystem path, written directly in the scoped
block. There is no `root *` wildcard.

**Example.**

```caddyfile
static.example.com {
    root /var/www/html
    file_server
}
```

## `tls`

**Purpose.** Customize TLS for a site: choose where its certificate comes from,
restrict the protocol and ciphers, and require client certificates (mTLS).

**Syntax.**

```caddyfile
tls                              # ACME (the default; optional)
tls internal                     # self-signed certificate for development
tls <cert-file> <key-file>       # static PEM certificate + key
tls min_version <1.2|1.3>
tls max_version <1.2|1.3>
tls ciphers <cipher-list>
tls client_auth <optional|require> <ca-file>
```

**Arguments.**

- **Sources** (at most one):
  - *(omitted)* — ACME, the default. A named site gets a certificate
    automatically (see [Sites, ports & HTTPS](../sites/)).
  - `tls internal` — a self-signed certificate generated at startup for
    development; clients must be configured to trust it. No ACME is attempted.
  - `tls <cert-file> <key-file>` — a static PEM certificate chain and private
    key served for this site instead of ACME. Renewal is the operator's job.
- **Options** (all optional; combine freely; each on its own `tls` line):
  - `min_version <1.2|1.3>` / `max_version <1.2|1.3>` — restrict the negotiated
    TLS protocol version for this site.
  - `ciphers <list>` — an OpenSSL cipher suite list, e.g.
    `ECDHE-ECDSA-AES128-GCM-SHA256`. Space-separated names are joined with `:`.
  - `client_auth <optional|require> <ca-file>` — mutual TLS: verify the client
    certificate against `ca-file`. `require` rejects clients without a valid
    certificate; `optional` requests one but accepts clients without.

**Behavior.** A named site that has a `tls` directive binds its port as a
**TLS listener** (in addition to the default port 443), so
`intranet.example.com:8443 { tls internal }` serves TLS on 8443. Sites with a
static or internal source are excluded from ACME issuance for that hostname.
Options are applied per SNI during the handshake and update on reload without
rebuilding the listener.

**Examples.**

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

## `access_log`

**Purpose.** Configure access logging in the Raddyfile instead of (or in
addition to) the `--access-log` CLI flag.

**Syntax.**

```caddyfile
# global block: enable with a path and format, or disable
access_log <path> [format=<json|common>]
access_log off

# site block: disable access logging for that site only
access_log off
```

**Arguments.**

- `<path>` — the log file to append to.
- `format=<json|common>` — `json` (default) writes one JSON object per request;
  `common` writes the classic combined log line
  (`%h %l %u %t "%r" %>s %b "%{Referer}i" "%{User-Agent}i"`).

**Behavior.** In the global block, `access_log <path>` sets the instance log file
and format; `access_log off` disables access logging for the whole instance. In
a site block, `access_log off` disables access logging for that site only. When
both the Raddyfile and the `--access-log` flag are set, the flag wins. See
[Access log](../../operations/access-log/) for the JSON fields.

**Example.**

```caddyfile
{
    access_log /var/log/raddy/access.log format=common
}

api.example.com {
    access_log off        # this site stays quiet
    reverse_proxy 127.0.0.1:8080
}
```

## `trusted_proxies`

**Purpose.** Declare which networks are trusted proxies, so raddy can derive the
real client IP from `X-Forwarded-For`. See [Trusted proxies](../trusted-proxies/).

**Syntax.**

```caddyfile
trusted_proxies <cidr>...
```

**Arguments.** One or more networks — `<address>/<prefix>` or a bare address.
IPv4 and IPv6 both work. A site-block value overrides the global list for that
site only.

## `dns_challenge`

**Purpose.** Issue certificates via **DNS-01** instead of HTTP-01, proving
domain control by publishing a TXT record through a DNS provider. Useful when
port 80 is unreachable. See [Sites, ports & HTTPS](../sites/).

**Syntax.**

```caddyfile
dns_challenge cloudflare <api_token>
```

**Arguments.** The provider (`cloudflare` — the only one today) and the
provider's API token, which must have **Zone: DNS: Edit** permission. Lives in
the [global block](../sites/#the-global-block).

**Behavior.** When set, every certificate on the instance is issued via DNS-01:
raddy publishes `_acme-challenge.<host>` TXT records while the order is being
validated and removes them afterwards. Without `dns_challenge`, HTTP-01 is used
as before.

> **Security:** the API token is a secret — keep the Raddyfile out of version
> control or protect it accordingly.

**Example.**

```caddyfile
{
    acme_email ops@example.com
    dns_challenge cloudflare <api_token>
}

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

## `tls_alpn_challenge`

**Purpose.** Use ACME TLS-ALPN-01 instead of HTTP-01 when port 80 is
unreachable.

**Syntax.**

```caddyfile
{
    tls_alpn_challenge
}
```

**Behavior.** The challenge is served on the standard TLS port 443 with a
temporary certificate containing the RFC 8737 `acmeIdentifier` extension and the
`acme-tls/1` ALPN protocol. It is mutually exclusive with
`dns_challenge` and requires ACME sites to use port 443. There is no
HTTP-01 fallback.

## `log_level`

**Purpose.** Set the global log level.

**Syntax.**

```caddyfile
log_level <level>
```

**Arguments.** `info` (default) | `debug` | `warn` | `error`.

## `acme_email`

**Purpose.** Set the ACME registration email (required by Let's Encrypt).

**Syntax.**

```caddyfile
acme_email <address>
```

**Arguments.** An email address. Lives in the [global block](../sites/#the-global-block).

## `import` and snippets

**Purpose.** Split configuration across files and reuse blocks with
`snippet`-style named blocks.

**Syntax.**

```caddyfile
import <file|name>

(name) {
    # reusable directives
}
```

**Behavior.**

- `import <file>` splices the contents of another Raddyfile at that point. Paths
  are relative to the importing file. Imports may nest (depth-limited). A site
  block may import a file whose directives belong to that site.
- A top-level block named `(name) { ... }` defines a reusable **snippet**;
  `import name` splices it at that point. Snippets are local to the file that
  defines them.

**Example.** A snippet that carries shared guards, imported into two sites:

```caddyfile
(base) {
    rate_limit remote_ip 100r/s
    header_up X-Raddy true
}

api.example.com {
    import base
    reverse_proxy 127.0.0.1:8080
}

admin.example.com {
    import base
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:9000
}
```

## Environment variables

**Purpose.** Inject values from the environment into a Raddyfile at parse time.

**Syntax.** A directive argument of the form `{$ENV_VAR}` is replaced by the
value of `ENV_VAR`. A missing variable is a validation error, so a config with a
typoed variable fails `raddy check` instead of starting with a bogus value.

**Behavior.** Works anywhere an argument appears — upstream targets, `root`
paths, `tls` certificate paths, and so on.

**Example.**

```caddyfile
api.example.com {
    reverse_proxy https://{$BACKEND_HOST}:8443
}
```

## `lb_policy` and `health_check`

Sub-directives of the [`reverse_proxy`](#reverse_proxy) block.

**`lb_policy`** — the upstream selection algorithm.

- `round_robin` (default) — rotate through the upstreams in order.
- `random` — pick uniformly at random.
- `ip_hash` — consistent hash on the client IP (per-IP stickiness).

**`health_check { ... }`** — active health checks (a TCP connect probe). Every
parameter is optional:

| Parameter | Default | Meaning |
|---|---|---|
| `interval <dur>` | `5s` | How often to probe |
| `timeout <dur>` | `2s` | Per-probe timeout |
| `consecutive_failures <n>` | `3` | Remove an upstream only after N consecutive failures |
| `consecutive_successes <n>` | `2` | Restore an upstream only after M consecutive successes |

Durations are a number plus a unit (`ms` / `s` / `m` / `h`), or a bare number
meaning seconds.

**Behavior.** An unhealthy upstream is never selected and flows back automatically
once restored. If **every** upstream is unhealthy, raddy returns `502`. Health
state survives SIGHUP reloads and is rebuilt only when the upstream list, policy,
or health-check parameters change.

**Example.**

```caddyfile
api.example.com {
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
}
```

## Upstream TLS options

Sub-directives of the `reverse_proxy` block that configure TLS to `https://`
backends. They require at least one `https://` upstream, otherwise the config is
rejected as meaningless.

```caddyfile
reverse_proxy {
    to https://10.0.0.1:8443 https://10.0.0.2:8443
    tls_servername api.internal
    tls_ca /etc/raddy/root-ca.pem
}
```

| Sub-directive | Meaning |
|---|---|
| `tls_servername <host>` | The SNI/hostname sent to the upstream (default: the upstream host). Required when the upstream address is an IP but its certificate is issued for a name. |
| `tls_skip_verify` | Disable upstream certificate *and* hostname verification (never use in production). |
| `tls_ca <pem-file>` | Root CA(s) used to verify the upstream certificate; repeatable. When set, these CAs replace the system trust roots. |
| `tls_cert <cert-file> <key-file>` | A client certificate presented to the upstream (mutual TLS to the backend). |

By default the upstream certificate is verified against the system trust roots
and its hostname must match `tls_servername` (or the upstream host). Verification
failures surface as **502 Bad Gateway**.

**Example.** Proxy to an HTTPS backend that serves an internal certificate:

```caddyfile
api.example.com {
    reverse_proxy {
        to https://10.0.0.1:8443
        tls_servername api.internal
        tls_ca /etc/raddy/root-ca.pem
    }
}
```

## WebSocket and protocol upgrades

**Purpose.** `reverse_proxy` forwards HTTP `Upgrade` requests (WebSocket and
similar `Connection: upgrade` protocols) transparently.

**Behavior.** The client's upgrade request goes upstream; once the upstream
answers `101 Switching Protocols`, raddy tunnels the connection
bidirectionally. No directive is needed — this is the default behavior of
`reverse_proxy` for HTTP/1.1 upgrade requests.

- Upgrades are **end-to-end**: raddy does not terminate the upgraded protocol;
  the backend must speak it.
- `header_up` / `header_down` still apply to the upgrade request/response
  headers; `encode` never applies to a `101` (upgraded) response.

**Example.**

```caddyfile
chat.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

The same site serves both WebSocket and regular HTTP traffic; raddy upgrades
only requests that carry an `Upgrade` header.

## A complete example

```caddyfile
{
    acme_email ops@example.com
    log_level info
    trusted_proxies 127.0.0.1
    access_log /var/log/raddy/access.jsonl format=json
}

(base) {
    rate_limit remote_ip 50r/s burst=100
    header_up X-Raddy true
}

# HTTP → HTTPS redirect
:80 {
    redir https://{host}{uri} permanent
}

api.example.com {
    import base

    handle /health {
        respond 200 ok
    }

    handle_path /api/* {
        reverse_proxy https://{$API_BACKEND}:8443
        tls_servername api.internal
    }

    handle /static/* {
        root /var/www/html
        file_server
        encode br zstd gzip
    }

    reverse_proxy 127.0.0.1:8080
    header_up X-Real-IP {remote_host}
}

admin.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    tls client_auth require /etc/certs/clients-ca.pem
    reverse_proxy 127.0.0.1:9000
}
```

> `header_up` after `reverse_proxy` still applies — it is a modifier. `encode`
> inside the `handle` block applies only to that block's `file_server`.
> `rate_limit` (imported from the `base` snippet) guards whichever terminal
> serves the site.
