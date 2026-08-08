---
title: Proxy an API
description: Load balance an API across upstreams with health checks, rate limiting, and the real client IP.
---

## Goal

Expose an API over HTTPS with two backend instances, automatic failover when one
drops, rate limiting per client, and the client socket address forwarded to the
backend.

## Configuration

```caddyfile
{
    acme_email ops@example.com
    trusted_proxies 10.0.0.0/8
}

api.example.com {
    rate_limit remote_ip 100r/s burst=200

    reverse_proxy {
        to 10.0.0.1:8000 10.0.0.2:8000
        lb_policy round_robin
        health_check {
            interval 5s
            timeout 2s
            consecutive_failures 3
            consecutive_successes 2
        }
    }

    header_up X-Real-IP {remote_host}
}
```

What each piece does:

- **`rate_limit remote_ip 100r/s burst=200`** — allows 100 requests per second
  per client, with a burst of 200; excess gets `429 Too Many Requests`.
- **`to`** — distributes requests across both upstreams.
- **`lb_policy round_robin`** — the default selection order (options are
  `round_robin`, `random`, `ip_hash`).
- **`health_check`** — probes each upstream with a TCP connect every `5s`
  (timeout `2s`). An upstream is removed only after `3` consecutive failures and
  restored only after `2` consecutive successes — this flapping suppression
  avoids thrashing on a flaky network.
- **`header_up X-Real-IP {remote_host}`** — forwards the client socket address
  (the direct TCP peer) to the backend. Behind a trusted proxy that is the
  proxy's address, not the effective client IP used by `rate_limit`.

## Run it

```bash
raddy check -c Raddyfile
raddy run -c Raddyfile
```

## What you get

- **Distribution** — consecutive requests alternate between `10.0.0.1:8000`
  and `10.0.0.2:8000`.
- **Failover** — stop one backend; after `consecutive_failures` probes it is
  removed and traffic goes to the healthy one. Restart it and it flows back
  after `consecutive_successes`.
- **Total outage** — if *every* upstream is unhealthy, raddy returns
  **`502 Bad Gateway`** instead of silently black-holing the request.
- **Rate limiting** — a client over the rate gets `429`; health state and rate
  buckets survive SIGHUP reloads.

## Variations

**Per-IP stickiness** — swap `round_robin` for `ip_hash` so a client keeps
hitting the same upstream (useful with stateful backends).

**Distinct paths** — combine with `handle` to rate-limit and balance an API
while serving static assets from the same host:

```caddyfile
api.example.com {
    handle /static/* {
        root /var/www/html
        file_server
    }

    rate_limit remote_ip 100r/s
    reverse_proxy {
        to 10.0.0.1:8000 10.0.0.2:8000
        health_check {
            interval 5s
            timeout 2s
        }
    }
}
```
