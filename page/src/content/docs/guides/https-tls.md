---
title: HTTPS & TLS
description: Automatic HTTPS, the tls directive, upstream TLS, mutual TLS, and HTTP/2.
---

This guide covers everything TLS in raddy: automatic HTTPS with ACME, the `tls`
directive for per-site control (self-signed or static certificates, protocol
versions, ciphers, mutual TLS), TLS to your upstream backends, and HTTP/2
downstream.

## Automatic HTTPS in one line

A named site without a port binds **443** and gets an ACME certificate
automatically:

```caddyfile
example.com {
    reverse_proxy 127.0.0.1:8080
}
```

raddy registers with the ACME directory, proves control of the domain — with
**HTTP-01** on its plain-HTTP listener by default, or **DNS-01** via
[`dns_challenge`](../../config/directives/#dns_challenge) when port 80 is
unreachable — and renews the certificate within 30 days of expiry. Set your
contact email in the [global block](../../config/sites/#the-global-block), and add
an HTTP→HTTPS redirect if you want port 80 visitors pointed at the secure site.
The full matching model is on [Sites, ports & HTTPS](../../config/sites/).

## The `tls` directive

The [`tls` directive](../../config/directives/#tls) in a site block customizes TLS
for that site. It has three certificate **sources**:

| Source | When to use |
|---|---|
| *(omitted)* | ACME, the default |
| `tls internal` | A self-signed certificate generated at startup — development only; clients must trust it |
| `tls <cert-file> <key-file>` | A static PEM certificate chain and key; you handle renewal |

```caddyfile
dev.example.com {
    tls internal
    reverse_proxy 127.0.0.1:8080
}

intranet.example.com {
    tls /etc/certs/intranet.pem /etc/certs/intranet.key
    reverse_proxy 127.0.0.1:9000
}
```

A named site with a `tls` directive binds its port as a **TLS listener**, so a
static or self-signed site can serve TLS on a non-443 port:

```caddyfile
dev.local:8443 {
    tls internal
    reverse_proxy 127.0.0.1:8080
}
```

```bash
curl -k -H 'Host: dev.local' https://127.0.0.1:8443/
```

### Protocol versions and ciphers

Restrict the negotiated TLS version and the cipher suites per site. Each option
is its own `tls` line:

```caddyfile
secure.example.com {
    tls min_version 1.2
    tls max_version 1.3
    tls ciphers ECDHE-ECDSA-AES128-GCM-SHA256
    reverse_proxy 127.0.0.1:9000
}
```

`min_version` / `max_version` accept `1.2` or `1.3`. `ciphers` takes an OpenSSL
cipher suite list; space-separated names are joined with `:`.

### Mutual TLS (client certificates)

Require — or optionally request — a client certificate signed by a CA you trust:

```caddyfile
secure.example.com {
    tls client_auth require /etc/certs/clients-ca.pem
    reverse_proxy 127.0.0.1:9000
}
```

- `client_auth require <ca-file>` — reject clients without a valid certificate.
- `client_auth optional <ca-file>` — request one, but accept clients without.

The same CA file can be reused across sites. See the
[Authentication guide](../auth/) for the other half of "who may connect"
— HTTP-level auth guards.

## TLS to your backends (upstream TLS)

Upstreams are plain HTTP by default. Prefix an upstream with `https://` to talk
TLS to the backend:

```caddyfile
api.example.com {
    reverse_proxy https://127.0.0.1:8443
}
```

For backends that need a specific SNI name, a private CA, or a client
certificate, use the block form with the [upstream TLS
options](../../config/directives/#upstream-tls-options):

```caddyfile
api.example.com {
    reverse_proxy {
        to https://10.0.0.1:8443 https://10.0.0.2:8443
        tls_servername api.internal
        tls_ca /etc/raddy/root-ca.pem
        tls_cert /etc/raddy/client.pem /etc/raddy/client.key
    }
}
```

- `tls_servername` — the SNI sent to the upstream (default: the upstream host).
  Required when the address is an IP but the certificate is for a name.
- `tls_ca` — extra root CA(s) for verifying the upstream certificate; system
  roots are always trusted in addition.
- `tls_cert <cert-file> <key-file>` — a client certificate for upstream mTLS.
- `tls_skip_verify` — disables verification entirely; never use in production.

Upstream certificate verification failures surface as `502 Bad Gateway`, so a
mismatched `tls_servername` or missing `tls_ca` is loud, not silent.

## HTTP/2 downstream

TLS listeners advertise HTTP/2 (`h2`) via ALPN and serve HTTP/2 to clients that
support it, falling back to HTTP/1.1 otherwise — no configuration needed. Plain
HTTP listeners stay HTTP/1.1 (cleartext h2c is not supported), and raddy talks
HTTP/1.1 to upstreams today.

## WebSockets over TLS

WebSocket upgrades work over both HTTP and HTTPS listeners — `reverse_proxy`
forwards `Upgrade` requests transparently. See [WebSocket and protocol
upgrades](../../config/directives/#websocket-and-protocol-upgrades) and the
[API proxy guide](../api-proxy/) for an example.
