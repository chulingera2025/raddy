---
title: Quick start
description: Put a local service behind Raddex, validate the configuration, and send the first request.
---

This guide gets a local HTTP service behind Raddex without requiring a public
domain or privileged ports. It uses one terminal for the upstream and one for
Raddex.

## 1. Start an upstream

If you do not already have a service on `127.0.0.1:8080`, run:

```bash
python3 -m http.server 8080 --bind 127.0.0.1
```

Keep that terminal open.

## 2. Write a Raddexfile

Create `Raddexfile`:

```caddyfile
example.local:8090 {
    reverse_proxy 127.0.0.1:8080
}
```

The explicit port keeps this example on plain HTTP. A named site without a port
uses port 443 and enters the automatic HTTPS path, which is covered in
[Sites, ports, and HTTPS](../config/sites/).

## 3. Validate before starting

```bash
raddex check -c Raddexfile
```

The command must exit with status 0 and print:

```text
Raddexfile: ok
```

The same validation is used by a reload, so this is the right check for CI and
deployment scripts.

## 4. Run and send a request

Start Raddex in a second terminal:

```bash
raddex run -c Raddexfile
```

Then send a request with the site Host header:

```bash
curl -H 'Host: example.local' http://127.0.0.1:8090/
```

The Host header matters because Raddex selects an HTTP site per listener. These
two requests deliberately exercise the fixed fallbacks:

```bash
curl http://127.0.0.1:8090/                             # 400: missing Host
curl -H 'Host: unknown.example' http://127.0.0.1:8090/  # 404: no matching site
```

## Add a second site

A listener can serve multiple named sites:

```caddyfile
example.local:8090 {
    reverse_proxy 127.0.0.1:8080
}

static.local:8090 {
    root ./public
    file_server
}
```

Create a file and restart Raddex with the updated configuration:

```bash
mkdir -p public
printf '%s\n' 'hello from raddex' > public/hello.txt
raddex check -c Raddexfile
```

Fetch it through the second site:

```bash
curl -H 'Host: static.local' http://127.0.0.1:8090/hello.txt
```

For a running service, send `SIGHUP` instead of restarting. The routing
snapshot is replaced atomically and existing connections remain open:

```bash
kill -HUP <raddex-pid>
```

## Try HTTPS locally

Use an internal certificate when you do not have a public ACME domain:

```caddyfile
dev.local:8443 {
    tls internal
    reverse_proxy 127.0.0.1:8080
}
```

```bash
raddex check -c Raddexfile
raddex run -c Raddexfile
curl -k -H 'Host: dev.local' https://127.0.0.1:8443/
```

`tls internal` is intended for development or private trust domains. For a
public site, see [HTTPS and TLS](../guides/https-tls/).

## Next steps

- [HTTPS and TLS](../guides/https-tls/) — ACME, mTLS, upstream TLS, and HTTP/2
- [Routing and matchers](../guides/routing/) — precise request selection
- [Proxy an API](../guides/api-proxy/) — balancing, health checks, and WebSockets
- [Layer 4](../guides/layer4/) — TCP, SNI, TLS termination, and UDP
- [Operations](../operations/deployment/) — deployment, reload, upgrade, and troubleshooting
- [Directive reference](../config/directives/) — complete syntax lookup
