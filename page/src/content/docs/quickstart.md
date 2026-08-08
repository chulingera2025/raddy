---
title: Quick start
description: Write your first Raddyfile and serve traffic in about five minutes.
---

This guide takes you from a blank directory to a running reverse proxy: write a
Raddyfile, validate it, run it, and watch traffic flow. You'll need about five
minutes and a terminal.

## Before you start

[Install raddy](../install/) if you haven't yet. You'll also want a local HTTP
service to proxy to — start a trivial one if you don't have anything running on
`127.0.0.1:8080`:

```bash
python3 -m http.server 8080 --bind 127.0.0.1
```

Leave it running; raddy will forward requests to it.

## 1. Write your first Raddyfile

Create a file named `Raddyfile` with one site. This site serves the host
`example.local` on port `8090` and proxies every request to your local service:

```caddyfile
example.local:8090 {
    reverse_proxy 127.0.0.1:8080
}
```

> **Why the explicit port?** A named site without a port binds **443 (TLS)** and
> enables automatic HTTPS. Writing `:8090` keeps this first example plain HTTP so
> you can run it on your laptop without a real domain. Ports and HTTPS are
> covered in [Sites, ports & HTTPS](../config/sites/).

## 2. Validate the configuration

`raddy check` runs the **same validation a reload performs** — if it passes here,
raddy starts cleanly:

```bash
raddy check -c Raddyfile
```

Expected output:

```
Raddyfile: ok
```

## 3. Run raddy

```bash
raddy run -c Raddyfile
```

raddy starts in the foreground, binds port `8090`, and waits for requests.

## 4. Send a request

In a second terminal, request the site through raddy:

```bash
curl -H 'Host: example.local' http://127.0.0.1:8090/
```

The `Host` header matches the site, so raddy proxies the request to
`127.0.0.1:8080` and you see your local service's response.

Now see site selection in action. Unlike a plain port-forwarder, raddy routes by
host — try these:

```bash
curl http://127.0.0.1:8090/                             # missing Host → 400
curl -H 'Host: unknown.example' http://127.0.0.1:8090/  # no matching site → 404
```

## 5. Add a second site

Create a directory with a file to serve, then add a static site next to your
proxy:

```bash
mkdir public && echo 'hello from raddy' > public/hello.txt
```

```caddyfile
example.local:8090 {
    reverse_proxy 127.0.0.1:8080
}

static.local:8090 {
    root ./public
    file_server
}
```

Stop raddy (Ctrl-C) and start it again with the updated file:

```bash
raddy run -c Raddyfile
```

Then fetch the file through the static site:

```bash
curl -H 'Host: static.local' http://127.0.0.1:8090/hello.txt
# → hello from raddy
```

> raddy can also **reload** configuration without downtime: send the running
> process `SIGHUP` (`kill -HUP <raddy pid>`). Reloads swap the routing snapshot
> atomically and keep existing connections alive.

## Next steps

- [Serve static files](../guides/static-files/) — `file_server` in detail
- [Redirect HTTP → HTTPS](../guides/http-to-https/) — the `:80` catch-all pattern
- [Proxy an API](../guides/api-proxy/) — load balancing, health checks, rate limiting
- [Sites, ports & HTTPS](../config/sites/) — how matching and certificates work
- [Directive reference](../config/directives/) — every directive, with examples
