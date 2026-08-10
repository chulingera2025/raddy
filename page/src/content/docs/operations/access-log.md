---
title: Access log
description: The JSON and Common Log Format access logs written by raddy via --access-log or the access_log directive.
---

raddy writes one access-log line per completed request. You can configure it from
the CLI or from the Raddyfile.

## Via the CLI: `--access-log`

Pass `--access-log <file>` to append one structured JSON object per completed
request to the given file:

```bash
raddy run -c Raddyfile --access-log /var/log/raddy/access.jsonl
```

Each line is a single JSON object (JSON Lines). Appending (never truncating),
so you can rotate the file out from under raddy and it keeps writing to the new
path.

## Via the Raddyfile: `access_log`

The [`access_log` directive](../../config/directives/#access_log) configures
logging in the config, in either format:

```caddyfile
{
    access_log /var/log/raddy/access.log format=json   # or format=common
}

api.example.com {
    access_log off        # disable logging for this site only
    reverse_proxy 127.0.0.1:8080
}
```

- Global block: `access_log <path> [format=<json|common>]` sets the instance log
  file and format; `access_log off` disables logging for the whole instance.
- Site block: `access_log off` disables logging for that site only.
- When both the Raddyfile and `--access-log` are set, the **flag wins**.

`format=common` writes the classic combined log line
(`%h %l %u %t "%r" %>s %b "%{Referer}i" "%{User-Agent}i"`); `json` (the default)
writes the structured line below.

## JSON fields

| Field | Type | Meaning |
|---|---|---|
| `ts` | integer | Request start time as **epoch milliseconds** |
| `client` | string | The **real client IP** — the TCP peer, or the untrusted `X-Forwarded-For` entry when `trusted_proxies` is configured (see [Trusted proxies](../../config/trusted-proxies/)) |
| `method` | string | HTTP method (`GET`, `POST`, …) |
| `path` | string | The request path, **including the query string** |
| `status` | integer | HTTP response status code |
| `duration_ms` | integer | Request duration in milliseconds |

```json
{"ts":1760850000123,"client":"203.0.113.7","method":"GET","path":"/","status":200,"duration_ms":4}
{"ts":1760850000456,"client":"203.0.113.7","method":"GET","path":"/search?q=raddy&page=2","status":200,"duration_ms":7}
```

The `client` field is the real client IP per the [trust model](../../config/trusted-proxies/):
without `trusted_proxies`, it is the TCP peer; with it configured, it is the
rightmost untrusted `X-Forwarded-For` entry. This differs from the
`{remote_host}` placeholder used in header rewrites, which always expands to the
TCP peer address — even when that peer is a trusted proxy.
