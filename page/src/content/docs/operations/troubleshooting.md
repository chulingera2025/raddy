---
title: Troubleshooting
description: Diagnose configuration, binding, TLS, upstream, reload, and upgrade failures in Raddy.
---

Start with the process output or service journal, then reproduce the smallest
configuration that still fails. `raddy check` is the first diagnostic command.

## Configuration fails

```bash
raddy check -c Raddyfile
```

The parser reports a file, line, and column. Common causes include:

- a global block that is not the first block;
- a directive in the wrong scope;
- an HTTP site and a TCP listener trying to own overlapping TCP binds;
- an SNI listener mixing `sni` routes with `to`;
- an invalid duration, limit, CIDR, or bracketed IPv6 address;
- an upstream hostname that cannot resolve during startup.

Fix the reported configuration error before testing runtime behavior.

## The listener will not start

If the error says the address is already in use, identify the process that owns
the exact address and transport. HTTP/TCP listeners cannot overlap; UDP may
share an address and port with TCP. A wildcard bind can overlap a specific
address even when the strings differ.

If a privileged port fails, either grant the service permission to bind it or
place a port-aware load balancer in front. Do not make the service root merely
to hide an unknown bind failure.

## The request returns 400 or 404

- `400 Bad Request` usually means the HTTP request has no valid Host header.
- `404 Not Found` means the Host is valid but no site on that listener matches.
- A catch-all such as `:80 { ... }` handles unmatched requests on that port.
- TLS site selection uses SNI before HTTP Host routing; verify both the client
  SNI and the certificate name.

Use curl explicitly while debugging:

```bash
curl -v -H 'Host: example.local' http://127.0.0.1:8090/
curl -vk --resolve example.com:8443:127.0.0.1 https://example.com:8443/
```

## ACME issuance fails

Check the challenge method against the network path:

- HTTP-01 needs the ACME server to reach TCP 80 and the correct DNS address.
- TLS-ALPN-01 needs TCP 443, the requested site on port 443, and a compatible
  ACME client handshake.
- DNS-01 needs a valid Cloudflare token with permission to edit the zone and
  enough DNS propagation time.

Inspect the configured `--cert-dir`, confirm it is writable, and ensure the
ACME account and certificate state persist between process restarts. Use an
ACME staging directory while testing a new domain setup.

## The upstream returns 502

Check the upstream scheme and address:

- bare `host:port` uses HTTP/1.1;
- `https://` enables TLS HTTP/1.1;
- `h2://` requires TLS HTTP/2;
- `h2c://` requires cleartext prior-knowledge HTTP/2.

For TLS upstreams, verify `tls_servername`, the CA file, client certificate,
and hostname verification. For h2c, confirm that the backend accepts the HTTP/2
connection preface directly rather than only HTTP/1.1 upgrade syntax.

## Reload or upgrade does not complete

Run the same check with the exact configuration and flags used by the service:

```bash
raddy check -c /etc/raddy/Raddyfile
```

A listener topology change is intentionally rejected by reload and upgrade.
Use a normal restart when adding, removing, or rebinding a listener. For an
upgrade, make sure the pidfile and upgrade socket match the running process.

Transparent TCP must be restarted normally. On Linux UDP upgrade, inspect the
handoff status and verify that the new process received every configured UDP
listener. A failed handoff is reported as a failed upgrade rather than a
successful but lossy transition.

## Transparent proxying fails

Transparent mode is a Linux networking integration, not a normal TCP reverse
proxy. Verify all of the following on the host:

- `CAP_NET_ADMIN` or equivalent privilege;
- `IP_TRANSPARENT` / `IPV6_TRANSPARENT` support;
- netfilter TPROXY rules that preserve the original destination;
- policy routing that sends marked traffic to the local socket;
- a route to the selected upstream.

Test the ordinary `tcp` mode first. If ordinary mode works but transparent mode
does not, the remaining fault is usually in kernel policy or service capability.

## QUIC and HTTP/3 expectations

Successful UDP forwarding proves only datagram passthrough. Raddy does not
terminate QUIC, parse HTTP/3, route HTTP/3 requests, or manage QUIC connection
migration. Put a dedicated QUIC/HTTP/3 implementation beside Raddy when those
functions are required.
