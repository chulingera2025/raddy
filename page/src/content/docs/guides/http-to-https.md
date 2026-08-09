---
title: Redirect HTTP → HTTPS
description: Force every plain-HTTP visitor onto HTTPS with a catch-all site.
---

## Goal

Visitors who type `http://example.com` should land on `https://example.com` —
same host, same path. You have a secure site on 443 (see [Sites, ports &
HTTPS](../../config/sites/)) and want the plain-HTTP listener to redirect.

## Configuration

A `:80` catch-all site redirects every request that reaches port 80. Because the
catch-all matches anything not claimed by a named site on that port, this is the
complete HTTP→HTTPS story:

```caddyfile
{
    acme_email ops@example.com
}

# HTTP → HTTPS redirect
:80 {
    redir https://{host}{uri} permanent
}

example.com {
    reverse_proxy 127.0.0.1:8080
}
```

- `{host}` and `{uri}` placeholders preserve the hostname and the full path
  (including the query string).
- `permanent` sends a **308 Permanent Redirect**, so clients and search engines
  remember the new URL.
- `temporary` (302) is available when you don't want the redirect cached.

## Run it

```bash
raddy check -c Raddyfile
raddy run -c Raddyfile
```

## What you get

```bash
curl -sI http://localhost/
```

```http
HTTP/1.1 308 Permanent Redirect
location: https://localhost/
```

Request paths are carried through:

```
http://example.com/posts/1?ref=home  →  308  →  https://example.com/posts/1?ref=home
```

## Not just port 80

The same catch-all pattern works on any plain-HTTP port, for example to redirect
a legacy port to a current one:

```caddyfile
:8080 {
    redir https://{host}{uri} permanent
}
```
