---
title: Routing & matchers
description: Route requests by path, host, method, header, query, client IP, or protocol.
---

Routing in raddex is built on **matchers** — terms that select which requests a
directive serves. This guide shows how to combine them to route traffic.

## Matcher terms

A matcher is a sequence of terms; **all terms must match** (AND). Each term has a
kind:

| Term | Matches when |
|---|---|
| `path <prefix>` | The request path equals the prefix or falls under it (`/api` matches `/api` and `/api/...`, not `/apix`); a trailing `*` is stripped |
| `host <host>` | The normalized Host header equals the value |
| `method <method>` | The request method equals the value (e.g. `GET`) |
| `header <name> <value>` | Request header `name` equals `value` |
| `query <key> <value>` | Query parameter `key` equals `value` |
| `remote_ip <cidr>...` | The real client IP is within the listed network(s) |
| `protocol <http\|https>` | The transport of the listener that received the request |

A bare value starting with `/` is shorthand for `path`, so `handle /static/*`
means `handle path /static/*`.

- **Negation** — prefix a term with `!`: `!path /admin/*` matches everything
  except `/admin/...`.
- **No parens, no `&&`** — terms are space-separated. `handle path /status
  method GET` is the syntax; `handle (path /status && method GET)` is not.

## Where matchers attach

Matchers attach to a `handle` / `handle_path` block, and inline to the terminal
directives `reverse_proxy`, `respond`, and `error`:

```caddyfile
reverse_proxy path /api/* { to 127.0.0.1:8080 }
respond method OPTIONS 204
error !path /assets/* 503
```

A terminal whose inline matcher does not match is a **no-op** — execution
continues to the next directive. A terminal without a matcher always matches.

## Grouping requests with `handle`

`handle <matcher> { ... }` runs its block for matching requests and then
**stops**; non-matching requests continue past it. This is the standard
"one path to this terminal, everything else to another" pattern:

```caddyfile
example.com {
    handle /static/* {
        root /var/www/html
        file_server
    }

    reverse_proxy 127.0.0.1:8080
}
```

## Stripping a prefix with `handle_path`

`handle_path <matcher> { ... }` behaves like `handle`, but the matched path
prefix is stripped from the URI before the block's terminal runs — so a backend
doesn't need to know it is mounted under `/api`:

```caddyfile
example.com {
    handle_path /api/* {
        reverse_proxy 127.0.0.1:8080
    }

    reverse_proxy 127.0.0.1:9000
}
```

`GET /api/users/1` is forwarded to the first backend as `/users/1`.

## Rewriting the URI with `rewrite`

`rewrite <to>` is a **modifier** that rewrites the request URI before the
terminal serves it. The placeholders `{host}`, `{uri}`, and `{remote_host}` are
available. Pair it with `handle` for conditional rewrites:

```caddyfile
example.com {
    handle path /docs/* {
        rewrite /v2/{uri}
        reverse_proxy 127.0.0.1:8080
    }

    reverse_proxy 127.0.0.1:8080
}
```

## Answering directly with `respond` and `error`

`respond <status> [<body>]` answers the request directly — no upstream, no file.
Use it for health checks, CORS preflight replies, and fixed maintenance
responses:

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

`error [<status>] [<message>]` serves raddex's internal error response with a
chosen status (default `500`) — useful to block an area with a matcher:

```caddyfile
example.com {
    handle /internal/* {
        error 404 not here
    }

    reverse_proxy 127.0.0.1:8080
}
```

## A full routing example

```caddyfile
api.example.com {
    # Health and CORS are answered locally, before any proxying.
    handle path /health {
        respond 200 ok
    }

    # The API is mounted under /api and balanced across two backends.
    handle_path /api/* {
        reverse_proxy {
            to 10.0.0.1:8000 10.0.0.2:8000
            health_check { interval 5s }
        }
    }

    # Static assets come from disk with compression.
    handle /static/* {
        root /var/www/html
        file_server
        encode zstd gzip
    }

    # Anything left — proxied to the app.
    reverse_proxy 127.0.0.1:8080
}
```

> Write order matters **between terminals**: the first matching terminal ends
> site execution. `respond`, `handle`, and `reverse_proxy` all compete in the
> order you write them.
