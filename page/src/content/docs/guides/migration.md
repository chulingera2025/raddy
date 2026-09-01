---
title: Migrate from Caddy or nginx
description: Convert a supported Caddyfile or nginx.conf subset and verify the result before deployment.
---

Raddex includes an independent migration tool for common configurations. It
produces a Raddexfile, then validates the generated result. It does not extend
or silently reinterpret Raddexfile syntax.

## Convert a source file

```bash
raddex import caddyfile source.Caddyfile -o Raddexfile
raddex import nginx nginx.conf -o Raddexfile
raddex check -c Raddexfile
```

Omit `-o` to print the generated configuration to stdout. Treat the generated
file as a starting point: review upstream TLS, trust boundaries, file roots,
and listener ownership before using it in production.

## Configuration concepts

| Existing concept | Raddex equivalent |
| --- | --- |
| Caddy site block | Raddex site block |
| Caddy `reverse_proxy` | Raddex `reverse_proxy` |
| Caddy `handle` / `handle_path` | Raddex `handle` / `handle_path` |
| Caddy `file_server` | Raddex `file_server` |
| nginx `proxy_pass` | Raddex `reverse_proxy` |
| nginx `server_name` | Raddex site keys |
| nginx `upstream` | Raddex `reverse_proxy` block with multiple `to` targets |
| nginx `proxy_set_header` | Raddex `header_up` |
| nginx `return` | Raddex `redir` or `respond` |

The mapping is intentionally conservative. Unsupported source directives are
reported instead of being emitted as a similar-looking configuration with
different behavior.

## Review after conversion

1. Run `raddex check` and keep the generated file under version control.
2. Confirm that a named site, an explicit port, and a catch-all have the
   intended TLS and listener behavior.
3. Review `trusted_proxies`; Raddex does not trust forwarded client IP headers
   unless the networks are declared.
4. Review upstream schemes. `https://`, `h2://`, and `h2c://` select different
   transport behavior.
5. Replace implicit ordering assumptions with explicit Raddex terminal,
   modifier, and guard placement.

See [Concepts](../../config/), [Routing and matchers](../routing/), and the
[Directive reference](../../config/directives/) for the Raddexfile model.
