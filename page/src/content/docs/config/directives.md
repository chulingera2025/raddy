---
title: Directive reference
description: Every Raddyfile directive with its syntax, arguments, and a runnable example.
---

This is the complete reference for the Raddyfile. Each directive follows the same
shape: what it does, its syntax, its arguments, and an example. If you are new to
the language, read [Concepts](../) first for the mental model.

## Summary

| Directive | Purpose | Kind |
|---|---|---|
| [`reverse_proxy`](#reverse_proxy) | Proxy a request to one or more upstreams | terminal |
| [`handle`](#handle) | Match a path and serve it with one terminal | terminal (scoped) |
| [`file_server`](#file_server) | Serve static files | terminal |
| [`redir`](#redir) | Redirect the client | terminal |
| [`header_up` / `header_down`](#header_up--header_down) | Rewrite request / response headers | modifier |
| [`encode`](#encode) | Compress responses (gzip / zstd) | modifier |
| [`rate_limit`](#rate_limit) | Reject requests beyond a rate | modifier |
| [`root`](#root) | Set the static file root for a block | helper |
| [`trusted_proxies`](#trusted_proxies) | Trusted networks for the real client IP | config |
| [`dns_challenge`](#dns_challenge) | DNS-01 issuance via a DNS provider (Cloudflare) | config |
| [`log_level`](#log_level) | Global log level | config |
| [`acme_email`](#acme_email) | ACME registration email | config |
| [`snippet` / `import`](#snippet--import) | Reusable snippets and includes | *planned* |

## `reverse_proxy`

**Purpose.** Forward a request to an upstream service — the core of a reverse
proxy.

**Syntax.**

```caddyfile
reverse_proxy <target>

reverse_proxy {
    to <upstream>...
    lb_policy round_robin|random|ip_hash
    health_check { ... }
}
```

**Arguments.**

- `<target>` / `<upstream>` — an upstream address, e.g. `127.0.0.1:8080`.
- `to` — list multiple upstreams for round-robin distribution.
- `lb_policy` — the selection algorithm; defaults to `round_robin`. See
  [load balancing](#lb_policy-and-health_check).
- `health_check { ... }` — active health checking of the upstreams. See
  [health checks](#lb_policy-and-health_check).

**Example.**

```caddyfile
api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

## `handle`

**Purpose.** Run one terminal for a path prefix, then stop. Everything inside a
`handle` block applies only to requests whose path matches.

**Syntax.**

```caddyfile
handle /path/* {
    # terminal (and modifiers) for this path
}
```

**Arguments.** A path matcher — `/static/*` matches any path under `/static/`.

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
(`..`) is rejected with `404`. Only `GET` and `HEAD` are allowed.

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

**Arguments.** `gzip`, `zstd` — in priority order.

**Example.** Prefer zstd, fall back to gzip:

```caddyfile
example.com {
    encode zstd gzip
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

- `<key>` — `remote_ip`, the real client IP (see [Trusted proxies](../trusted-proxies/)).
- `<rate>` — `<count>r/<unit>` where the unit is `s` / `m` / `h` / `d`, e.g.
  `50r/s`, `1200r/m`. The count must be at least 1.
- `burst=<n>` — the token bucket capacity; defaults to the rate count.

**Behavior.** A token bucket per (site, terminal, client IP). It refills
continuously at `<rate>`; a request with no token is rejected with `429`.
`rate_limit` is a modifier: it guards whichever terminal serves the block, and
state survives SIGHUP reloads.

**Example.**

```caddyfile
api.example.com {
    rate_limit remote_ip 100r/s burst=200
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

## `snippet` / `import`

**Purpose.** Reusable snippets and multi-file includes. These are **planned** and
not yet implemented — the grammar is reserved, so do not use them in a config you
ship.

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

## A complete example

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

> `header_up` after `reverse_proxy` still applies — it is a modifier. `encode`
> inside the `handle` block applies only to that block's `file_server`.
> `rate_limit` guards whichever terminal serves the site.
