---
title: Authentication
description: Gate your sites with HTTP Basic auth or by delegating to an auth upstream.
---

Two guard directives control who may be served: `basic_auth` for HTTP Basic
authentication and `forward_auth` for delegating to a dedicated auth service.
Both are guards, so they apply to whichever terminal serves the block, and
inside a `handle` block only to that block's terminal.

## HTTP Basic auth with `basic_auth`

`basic_auth <user> <bcrypt-hash>` requires HTTP Basic authentication. Generate
the bcrypt hash of the password first:

```bash
htpasswd -Bbn admin 's3cret'
# → admin:$2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
```

Then protect a site:

```caddyfile
admin.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:8080
}
```

Several `basic_auth` lines build a **user table** — a request may present
credentials for any of them:

```caddyfile
admin.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    basic_auth jane $2b$12$7r8...HrH9
    reverse_proxy 127.0.0.1:8080
}
```

Requests without valid credentials get **401 Unauthorized** with a
`WWW-Authenticate: Basic` challenge, so browsers show the login prompt.

## Delegating to an auth service with `forward_auth`

`forward_auth <host:port>` forwards each request to a dedicated auth upstream
(e.g. oauth2-proxy, or your own auth service):

```caddyfile
app.example.com {
    forward_auth auth.example.com:4181
    reverse_proxy 127.0.0.1:8080
}
```

How it decides:

- **2xx** from the auth upstream — grant access, forward to the real upstream.
- **403** — passed through to the client unchanged.
- anything else — **401 Unauthorized**.

The request sent to the auth upstream carries the original `Authorization` and
`X-Forwarded-For` headers, so the auth service sees the same credentials and
client as raddex does. **Response headers** from the auth upstream — for example
an identity header such as `X-Auth-User` — are copied onto the request before
the real upstream sees it, so your backend can trust them.

## Scoping a guard to a path

Put a guard inside a `handle` block to protect only part of a site:

```caddyfile
example.com {
    handle /admin/* {
        basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
        reverse_proxy 127.0.0.1:9000
    }

    reverse_proxy 127.0.0.1:8080
}
```

Here `/admin/...` is proxied to the admin backend behind Basic auth; everything
else goes to the main app without a guard.

## Combining with mTLS

HTTP-level auth and [mutual TLS](../https-tls/#mutual-tls-client-certificates)
are independent and compose: mTLS answers "is this client's certificate signed
by our CA?", while `basic_auth` / `forward_auth` answer "who is this client, and
are they allowed?".

```caddyfile
secure.example.com {
    tls client_auth require /etc/certs/clients-ca.pem
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:9000
}
```
