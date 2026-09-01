---
title: Trusted proxies
description: Tell raddex which networks are trusted so it can derive the real client IP from X-Forwarded-For.
---

Features that key on the client — [rate limiting](../directives/#rate_limit),
`ip_hash` load balancing, access logs — need the *real* client IP, not the
address of an intermediate proxy. This page explains how raddex decides that.

## The default

By default raddex **trusts nothing**: it uses the TCP peer address directly and
ignores any `X-Forwarded-For` header. This is safe by default — an attacker
cannot forge a client IP unless you explicitly trust the proxy they came through.

## When you sit behind a proxy

If raddex is behind a CDN or a load balancer that sets `X-Forwarded-For`, declare
that proxy's network as trusted:

```caddyfile
{
    trusted_proxies 10.0.0.0/8 172.16.0.0/12 127.0.0.1
}
```

Once a network is trusted, raddex derives the real client IP as follows:

1. If the TCP peer is **not** in the trusted list, the peer address *is* the
   client (no `X-Forwarded-For` parsing).
2. If the peer **is** trusted, raddex walks the `X-Forwarded-For` chain from the
   right and takes the rightmost entry that is **not** a trusted proxy (malformed
   entries are skipped).
3. If the whole chain is trusted — or the header is absent — the trusted peer
   itself is used.

**Syntax.**

```caddyfile
trusted_proxies <cidr>...
```

Each `<cidr>` is `<address>/<prefix>` or a bare address (a single host). IPv4 and
IPv6 both work. Later entries override earlier ones within the same scope.

## Per-site overrides

`trusted_proxies` can be set in a site block to override the global list **for
that site only**:

```caddyfile
{
    trusted_proxies 10.0.0.0/8
}

api.example.com {
    trusted_proxies 127.0.0.1   # only this site trusts loopback
    reverse_proxy 127.0.0.1:8080
}
```

## Example

With the config below, a request arriving through your CDN (`203.0.113.0/24`)
and a `X-Forwarded-For: 198.51.100.9, 10.0.0.5` header is recorded and rate
limited as coming from `198.51.100.9` — the rightmost entry that is not a trusted
proxy:

```caddyfile
{
    trusted_proxies 203.0.113.0/24 10.0.0.0/8
}

api.example.com {
    rate_limit remote_ip 100r/s
    reverse_proxy 127.0.0.1:8080
}
```
