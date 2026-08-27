---
title: Sites, ports, and HTTPS
description: Understand site keys, listener selection, catch-alls, multi-domain blocks, wildcard matching, and automatic HTTPS.
---

Raddy first selects a listener, then selects a site on that listener. Site keys
are not global routes: the same hostname can be configured differently on
different ports.

## The global block

A leading bare block configures instance-wide behavior:

```caddyfile
{
    acme_email ops@example.com
    log_level info
}
```

The global block must be first when present. Site blocks follow it.

## Site keys and ports

```caddyfile
api.example.com { ... }          # port 443, automatic HTTPS
api.example.com:8081 { ... }     # explicit plain-HTTP port
:80 { ... }                      # catch-all on port 80
```

- A named site without a port uses TLS on port 443 and enters automatic HTTPS.
- An explicit port is plain HTTP unless the site contains a `tls` directive.
- A catch-all such as `:80` handles requests that match no named site on that
  listener.
- A named site with `tls internal` or static `tls <cert> <key>` can terminate
  TLS on a non-443 port.

The HTTP and TLS listener topology is derived from these blocks. Overlapping
binds are rejected before startup.

## Site selection

On plain HTTP, Raddy normalizes the Host header by removing the port and
trailing dot and lowercasing it. On TLS, the SNI name is used for certificate
and site selection.

| Request condition | Result |
| --- | --- |
| Valid Host/SNI matches a named site | That site handles the request |
| Host is missing or malformed | `400 Bad Request` |
| Host is valid but unmatched | `404 Not Found`, unless a catch-all exists |
| Catch-all exists on the listener | The catch-all handles the unmatched request |

An exact site wins over a wildcard. A wildcard such as `*.example.com` matches
`api.example.com`, but not `example.com` or `a.b.example.com`.

## Multiple domains in one block

Comma-separated site keys share the same body while remaining independently
addressable:

```caddyfile
api.example.com, api.example.net {
    reverse_proxy 127.0.0.1:8080
}
```

This is configuration reuse, not a wildcard. Certificate issuance and SNI
matching still consider each concrete hostname.

## Automatic HTTPS

For a named site on port 443, Raddy can obtain and renew an ACME certificate:

1. The configured challenge method proves domain control.
2. The certificate is selected by SNI for the incoming TLS connection.
3. Certificate and account state is cached under `--cert-dir`.
4. Renewal is attempted before expiry; an existing certificate remains in use
   if renewal fails.

The default challenge is HTTP-01. Use `dns_challenge` for Cloudflare DNS-01 or
`tls_alpn_challenge` for TLS-ALPN-01 on port 443. These methods are described in
the [HTTPS and TLS guide](../../guides/https-tls/).

## IPv6 site keys

Bracket IPv6 literals in site keys, upstreams, and Host headers:

```caddyfile
[::1]:8080 {
    tls internal
    reverse_proxy [::1]:9000
}
```

HTTP and TLS wildcard listeners are configured for dual-stack behavior. Verify
the host firewall and upstream address family on the target machine.

## Request routing after site selection

Once a site is selected, its directives follow the Raddyfile execution model.
Read [Concepts](../), [Routing and matchers](../../guides/routing/), and the
[Directive reference](../directives/) for terminal, modifier, and guard
semantics.
