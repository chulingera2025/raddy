---
title: HTTPS and TLS
description: Choose automatic HTTPS, static certificates, mTLS, upstream TLS, or explicit HTTP/2 behavior.
---

Raddy has two separate TLS decisions:

1. **Downstream TLS** protects the connection from the client to Raddy.
2. **Upstream TLS** protects the connection from Raddy to the backend.

Configure them independently. A TLS client connection does not imply that the
backend connection also uses TLS.

## Choose a certificate source

| Configuration | Result | Typical use |
| --- | --- | --- |
| Named site without `tls` | ACME certificate on port 443 | Public HTTPS |
| `tls internal` | Locally generated certificate | Development or a private trust domain |
| `tls <cert> <key>` | Static PEM certificate and key | Externally managed certificates |

## Automatic HTTPS

The smallest public HTTPS configuration is:

```caddyfile
{
    acme_email ops@example.com
}

example.com {
    reverse_proxy 127.0.0.1:8080
}
```

A named site without a port uses port 443 and obtains a certificate through
ACME. Raddy stores certificates and account credentials under `raddy_certs/`
by default; use `--cert-dir` to persist them elsewhere.

The challenge methods are mutually exclusive at the instance level:

| Method | Configuration | Network requirement |
| --- | --- | --- |
| HTTP-01 | Default | The ACME server can reach TCP 80 |
| Cloudflare DNS-01 | `dns_challenge cloudflare <token>` in the global block | The token can edit the authoritative DNS zone |
| TLS-ALPN-01 | `tls_alpn_challenge` in the global block | The ACME server can reach TCP 443; the site is an ACME site on 443 |

HTTP-01 may create an implicit plain-HTTP listener for the ACME challenge when
the configuration has no site on port 80. Add a separate `:80` site when you
also want to redirect normal HTTP traffic:

```caddyfile
:80 {
    redir https://{host}{uri} permanent
}
```

For wildcard certificates, use DNS-01. Raddy's v0.3.5 implementation includes
Cloudflare only; other DNS providers are outside this release.

## Local or private TLS

```caddyfile
dev.example.test:8443 {
    tls internal
    reverse_proxy 127.0.0.1:8080
}
```

The explicit `tls` directive makes an otherwise non-443 named site a TLS
listener. Clients must trust the generated certificate; `curl -k` is suitable
for a local smoke test, not for production policy.

Static certificates use the same shape:

```caddyfile
intranet.example.com {
    tls /etc/raddy/certs/intranet.pem /etc/raddy/certs/intranet.key
    reverse_proxy 127.0.0.1:9000
}
```

Raddy does not renew a static certificate. The external certificate owner must
replace the files and trigger the appropriate reload or restart procedure.

## TLS options and mTLS

TLS options are separate `tls` lines:

```caddyfile
secure.example.com {
    tls min_version 1.2
    tls max_version 1.3
    tls ciphers ECDHE-ECDSA-AES128-GCM-SHA256
    tls client_auth require /etc/raddy/certs/clients-ca.pem
    reverse_proxy 127.0.0.1:9000
}
```

- `min_version` and `max_version` accept `1.2` or `1.3`.
- `ciphers` accepts OpenSSL cipher names; multiple names are joined with `:`.
- `client_auth require <ca>` rejects clients without a valid certificate.
- `client_auth optional <ca>` requests a certificate but also accepts clients
  without one.

## TLS to an upstream

Bare upstreams use HTTP/1.1 without TLS. Select the backend protocol with its
scheme:

```caddyfile
reverse_proxy http://127.0.0.1:8080
reverse_proxy https://127.0.0.1:8443
reverse_proxy h2://127.0.0.1:9443
reverse_proxy h2c://127.0.0.1:9080
```

Use block form for backend identity and trust settings:

```caddyfile
api.example.com {
    reverse_proxy {
        to https://10.0.0.11:8443 https://10.0.0.12:8443
        tls_servername api.internal
        tls_ca /etc/raddy/root-ca.pem
        tls_cert /etc/raddy/client.pem /etc/raddy/client.key
    }
}
```

- `tls_servername` controls the SNI and hostname used for verification.
- `tls_ca` supplies the trusted PEM roots for the backend; when set, it replaces
  the system trust roots.
- `tls_cert` supplies a client certificate for backend mTLS.
- `tls_skip_verify` disables backend certificate verification and should not be
  used in production.

An upstream certificate or protocol mismatch becomes a `502 Bad Gateway`; it
is not silently downgraded to another scheme.

## HTTP/2 and WebSockets

TLS listeners advertise `h2` and fall back to HTTP/1.1 through ALPN. Plain HTTP
listeners remain HTTP/1.1. Upstream HTTP/2 is explicit with `h2://`, while
`h2c://` uses the cleartext HTTP/2 connection preface directly; it does not use
the obsolete HTTP/1.1 `Upgrade: h2c` mechanism.

`reverse_proxy` forwards WebSocket and other HTTP/1.1 upgrade requests without
an extra directive. The backend remains responsible for the upgraded protocol.

## TLS-ALPN-01 details

Enable the method globally:

```caddyfile
{
    acme_email ops@example.com
    tls_alpn_challenge
}
```

Raddy serves a temporary RFC 8737 certificate with the `acme-tls/1` ALPN
protocol on TCP 443 during the challenge. It cannot be combined with
`dns_challenge`, and it does not turn an arbitrary TLS listener into an ACME
challenge endpoint.
