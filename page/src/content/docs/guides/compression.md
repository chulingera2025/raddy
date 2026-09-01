---
title: Compression
description: Compress responses with gzip, zstd, and Brotli.
---

The `encode` directive compresses responses before they leave raddex. It supports
three algorithms — **gzip**, **zstd**, and **Brotli** (`br`) — and applies to
both proxied responses and static files.

## How it works

`encode` takes algorithms **in priority order**. raddex negotiates against the
client's `Accept-Encoding` header and uses the first algorithm the client also
supports:

```caddyfile
example.com {
    encode br zstd gzip
    reverse_proxy 127.0.0.1:8080
}
```

Here a client that accepts Brotli gets `br`; one that doesn't falls back to
`zstd`, then `gzip`. A client that supports none of them gets the response
uncompressed.

The directive applies to whichever terminal serves the block — proxied APIs and
static files alike:

```caddyfile
static.example.com {
    root /var/www/html
    file_server
    encode zstd gzip
}
```

## Choosing algorithms

| Algorithm | Trade-off |
|---|---|
| `br` (Brotli) | Best compression ratio; universally supported by modern browsers |
| `zstd` | Fast, strong compression; supported by current browsers and many HTTP clients |
| `gzip` | The baseline; supported everywhere |

A good default for web traffic is `encode br zstd gzip`. `encode` never applies
to a `101` (upgraded, e.g. WebSocket) response.

## What you get

```bash
curl -H 'Host: static.example.com' -H 'Accept-Encoding: br' \
     http://127.0.0.1:8090/app.js -sD - -o /dev/null
```

```http
HTTP/1.1 200 OK
content-encoding: br
```

`file_server` honors the same negotiation, so it serves `index.html` compressed
and skips compression for clients that don't ask for it.
