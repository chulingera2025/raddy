---
title: Sites, ports & HTTPS
description: How a request is matched to a site, how ports and catch-alls work, and how automatic HTTPS obtains certificates.
---

This page explains how raddy decides *which* site serves a request, how ports
and catch-alls work, and how automatic HTTPS gets you a certificate.

## The global block

A bare `{ ... }` at the top of the file is the **global block**. It holds
site-wide settings:

```caddyfile
{
    acme_email ops@example.com
    log_level info
}
```

A leading `{` token (not a hostname) marks it as the global block. `acme_email`
is required by Let's Encrypt before certificates can be issued.

## Sites and ports

Each site block names a host and, optionally, a port:

```caddyfile
api.example.com { ... }          # default port: 443 (TLS)
api.example.com:8081 { ... }     # explicit port, plain HTTP
:80 { ... }                      # a catch-all for every request on port 80
```

- A **named site without a port** binds **443** and enables automatic HTTPS.
- An **explicit port** (`:8081`) binds that port as plain HTTP — useful for
  local multi-port deployments and testing.
- A **catch-all** (`:80`, `:8443`, …) serves every request on that listener that
  matches no named site.
- A named site with a [`tls` directive](../directives/#tls) serves its port over
  **TLS even when it is not 443** — `intranet.example.com:8443 { tls internal }`
  is a TLS listener on 8443.

## How a request matches a site

Selection happens **per listener**: a request is matched only against the sites
on the port it arrived at. On a plain-HTTP listener, raddy compares the
normalized `Host` header — port stripped, trailing dot stripped, lowercased. On a
TLS listener (443), matching uses the **SNI** name instead.

What happens with each request:

| Situation | Result |
|---|---|
| `Host` matches a named site | that site serves it |
| `Host` is missing or malformed | `400 Bad Request` |
| `Host` is valid but matches nothing | `404 Not Found` |
| `Host` matches nothing and a catch-all exists on the port | the catch-all serves it |

> Because matching is per listener, two sites on different ports never interfere,
> and a catch-all on `:80` doesn't catch HTTPS traffic on 443.

## Automatic HTTPS

A named site on port 443 gets a certificate automatically:

1. **Issuance** — raddy registers with the ACME directory (Let's Encrypt by
   default) and proves control of the domain — by default with the **HTTP-01**
   challenge on a plain-HTTP listener (`/.well-known/acme-challenge/…`), or
   with **DNS-01** via `dns_challenge` in the [global block](#the-global-block)
   when port 80 is unreachable (see the
   [directive reference](../directives/#dns_challenge)). Because HTTP-01 is
   answered on a plain-HTTP listener, a config with named sites but no site on
   port 80 automatically binds an implicit `:80` listener that serves only the
   ACME challenge; `dns_challenge` skips it. Set
   `tls_alpn_challenge` in the global block to use TLS-ALPN-01 on 443
   instead of HTTP-01.
2. **SNI** — each HTTPS request's certificate is selected by the requested
   hostname, so a multi-site server serves the right certificate per site.
3. **Caching** — certificates and account credentials are stored under
   `raddy_certs/` (configurable with `--cert-dir`), so restarts reuse them
   without re-issuing.
4. **Renewal** — certificates are renewed automatically within 30 days of
   expiry; a renewal failure keeps the existing certificate serving.

TLS listeners advertise HTTP/2 (`h2`) via ALPN and serve HTTP/2 to clients that
support it, falling back to HTTP/1.1 otherwise. A site with a static or internal
`tls` source (see the [`tls` directive](../directives/#tls)) is excluded from
ACME issuance for that hostname.

Set your contact email in the [global block](#the-global-block):

```caddyfile
{
    acme_email ops@example.com
}

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

A common companion is an HTTP→HTTPS redirect on port 80, so plain-HTTP visitors
get pointed at the secure site:

```caddyfile
:80 {
    redir https://{host}{uri} permanent
}
```

## Multiple sites, one server

Give each site its own block — they share the process, connection pools, and
certificates:

```caddyfile
api.example.com {
    reverse_proxy 127.0.0.1:8080
}

static.example.com {
    root /var/www/html
    file_server
}
```

Several domains can share one site block. Raddy clones the body into one
independently routable site per host:

```caddyfile
a.example.com, b.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Wildcard names match one left-most label only. An exact site takes precedence
over a wildcard, and a wildcard never matches the apex or a multi-label name.

## IPv6 site addresses

IPv6 literal site names use brackets in both the Raddyfile and the HTTP Host
header:

```caddyfile
[::1]:8080 {
    tls internal
    reverse_proxy [::1]:9000
}
```

HTTP and TLS wildcard listeners are dual-stack, so IPv4 and IPv6 clients use
the same site topology.

## Fixed site-selection fallbacks
- Configurable responses for the site-selection fallbacks (missing or unmatched
  Host → 400 / 404) — those two are fixed. Inside a site, the [`respond`](../directives/#respond)
  and [`error`](../directives/#error) terminals give you full control over
  custom responses.
