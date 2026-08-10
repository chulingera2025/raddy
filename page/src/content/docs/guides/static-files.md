---
title: Serve static files
description: Host a static site from disk with raddy, with compression.
---

## Goal

Serve a directory of static files (HTML, CSS, JS, images) over HTTP — with
compression, no application server required.

## Configuration

Put the files in a directory and point a site at it with `file_server`:

```caddyfile
static.example.com {
    root /var/www/html
    file_server
}
```

`root` sets the directory; `file_server` serves it. A relative path like
`./public` is resolved from raddy's working directory.

To send compressed responses, add `encode` — the first algorithm the client also
supports wins. `gzip`, `zstd`, and Brotli (`br`) are available:

```caddyfile
static.example.com {
    root /var/www/html
    file_server
    encode br zstd gzip
}
```

See the [Compression guide](../compression/) for how negotiation works
and how to choose algorithms.

## Run it

```bash
raddy check -c Raddyfile
raddy run -c Raddyfile
```

## What you get

```bash
curl -H 'Host: static.example.com' http://127.0.0.1:8090/            # index.html
curl -H 'Host: static.example.com' http://127.0.0.1:8090/app.js      # a file
curl -H 'Host: static.example.com' -H 'Accept-Encoding: gzip' \
     http://127.0.0.1:8090/app.js -sD - -o /dev/null                # Content-Encoding: gzip
```

Behavior to rely on:

- **A directory serves its `index.html`** — `/` maps to `index.html`.
- **Only `GET` and `HEAD`** are allowed; other methods are rejected.
- **Path traversal is blocked** — `/../etc/passwd` returns `404`, not your files.
- **`encode` compresses `file_server` responses too**, honoring the client's
  `Accept-Encoding`.
- `file_server` serves `root` + the full request path, including any `handle`
  prefix (see below).

## Serving a subpath

Use `handle` to serve static files under one path while proxying everything
else:

```caddyfile
example.com {
    handle /static/* {
        root /var/www/html
        file_server
        encode zstd gzip
    }

    reverse_proxy 127.0.0.1:8080
}
```

Here `/static/app.js` maps to `/var/www/html/static/app.js` — the `handle`
prefix is kept in the path.
