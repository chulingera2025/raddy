---
title: Migrate from Caddy or nginx
description: Convert a supported Caddyfile or nginx.conf subset and verify the result before deployment.
---

Raddy includes an independent migration tool for common configurations. It
produces a Raddyfile, then validates the generated result. It does not extend
or silently reinterpret Raddyfile syntax.

## Convert a source file

```bash
raddy import caddyfile source.Caddyfile -o Raddyfile
raddy import nginx nginx.conf -o Raddyfile
raddy check -c Raddyfile
```

Omit `-o` to print the generated configuration to stdout. Treat the generated
file as a starting point: review upstream TLS, trust boundaries, file roots,
and listener ownership before using it in production.

## Configuration concepts

| Existing concept | Raddy equivalent |
| --- | --- |
| Caddy site block | Raddy site block |
| Caddy `reverse_proxy` | Raddy `reverse_proxy` |
| Caddy `handle` / `handle_path` | Raddy `handle` / `handle_path` |
| Caddy `file_server` | Raddy `file_server` |
| nginx `proxy_pass` | Raddy `reverse_proxy` |
| nginx `server_name` | Raddy site keys |
| nginx `upstream` | Raddy `reverse_proxy` block with multiple `to` targets |
| nginx `proxy_set_header` | Raddy `header_up` |
| nginx `return` | Raddy `redir` or `respond` |

The mapping is intentionally conservative. Unsupported source directives are
reported instead of being emitted as a similar-looking configuration with
different behavior.

## Review after conversion

1. Run `raddy check` and keep the generated file under version control.
2. Confirm that a named site, an explicit port, and a catch-all have the
   intended TLS and listener behavior.
3. Review `trusted_proxies`; Raddy does not trust forwarded client IP headers
   unless the networks are declared.
4. Review upstream schemes. `https://`, `h2://`, and `h2c://` select different
   transport behavior.
5. Replace implicit ordering assumptions with explicit Raddy terminal,
   modifier, and guard placement.

See [Concepts](../../config/), [Routing and matchers](../routing/), and the
[Directive reference](../../config/directives/) for the Raddyfile model.
