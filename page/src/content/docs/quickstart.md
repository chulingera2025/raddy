---
title: Quick start
description: Write your first Raddyfile and serve traffic in about five minutes.
---

This guide takes you from a blank directory to a running reverse proxy: install
raddy, write a Raddyfile, validate it, run it, and watch traffic flow. Then a few
short follow-on snippets show rate limiting, basic auth, and HTTPS. You'll need
about five minutes and a terminal.

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

## Try more: rate limiting

Add a rate limit to the proxy site. Stop raddy, add a `rate_limit` line, and
start it again:

```caddyfile
example.local:8090 {
    rate_limit remote_ip 10r/s
    reverse_proxy 127.0.0.1:8080
}
```

Burst a few requests — the 11th request in a burst of 10 returns `429 Too Many
Requests`:

```bash
for i in $(seq 1 12); do
    curl -s -o /dev/null -w '%{http_code}\n' -H 'Host: example.local' http://127.0.0.1:8090/
done
# → 200 ten times, then 429 429
```

## Try more: basic auth

Protect a site with a username and a bcrypt password hash. Generate the hash
with `htpasswd -B` (from the `apache2-utils` package):

```bash
htpasswd -Bbn admin 's3cret'   # → admin:$2b$12$...
```

Then paste the hash into a new site:

```caddyfile
admin.local:8090 {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:8080
}
```

```bash
curl -H 'Host: admin.local' http://127.0.0.1:8090/            # → 401 Unauthorized
curl -u admin:'s3cret' -H 'Host: admin.local' http://127.0.0.1:8090/   # → 200
```

## Try more: HTTPS

On a machine with a real domain that resolves to it, drop the `:8090` port and
let raddy issue a certificate automatically:

```caddyfile
example.com {
    reverse_proxy 127.0.0.1:8080
}
```

A named site without a port binds **443** and gets an ACME certificate
automatically (HTTP-01 by default — make sure port 80 is reachable, or configure
`dns_challenge`). For local development without a domain, use the `tls` directive
with a self-signed certificate:

```caddyfile
dev.local:8443 {
    tls internal
    reverse_proxy 127.0.0.1:8080
}
```

```bash
curl -k -H 'Host: dev.local' https://127.0.0.1:8443/
```

The full story — the `tls` directive, upstream TLS, mTLS, and HTTP/2 — is in the
[HTTPS & TLS](../guides/https-tls/) guide.

## Next steps

- [HTTPS & TLS](../guides/https-tls/) — the `tls` directive, upstream TLS, mTLS, HTTP/2
- [Routing & matchers](../guides/routing/) — route by path, host, method, header, query, IP
- [Serve static files](../guides/static-files/) — `file_server` in detail
- [Redirect HTTP → HTTPS](../guides/http-to-https/) — the `:80` catch-all pattern
- [Proxy an API](../guides/api-proxy/) — load balancing, health checks, rate limiting, WebSockets
- [Authentication](../guides/auth/) — `basic_auth` and `forward_auth`
- [Directive reference](../config/directives/) — every directive, with examples
