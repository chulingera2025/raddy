---
title: Configuration reuse
description: Split your configuration across files, reuse snippets, and inject environment variables.
---

As a Raddexfile grows, three features keep it readable and deployable:
`import` for multi-file includes, `(name)` snippets for reuse within a file, and
`{$ENV}` for injecting environment variables at parse time.

## Including other files with `import`

`import <file>` splices the contents of another Raddexfile at that point. Paths
are relative to the importing file, and imports may nest (depth-limited):

```caddyfile
import common/headers.conf

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

A site block may import a file whose directives belong to that site:

```caddyfile
# common/proxy-settings.conf
rate_limit remote_ip 100r/s
header_up X-Raddex true
```

```caddyfile
api.example.com {
    import common/proxy-settings.conf
    reverse_proxy 127.0.0.1:8080
}
```

## Snippets: reusable blocks in one file

A top-level block named `(name) { ... }` defines a **snippet**; `import name`
splices it at that point. Snippets are local to the file that defines them:

```caddyfile
(base) {
    rate_limit remote_ip 100r/s
    header_up X-Raddex true
}

api.example.com {
    import base
    reverse_proxy 127.0.0.1:8080
}

admin.example.com {
    import base
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:9000
}
```

Both sites share the same guards, and `admin.example.com` adds its own auth.

## Injecting environment variables

A directive argument of the form `{$ENV_VAR}` is replaced by the value of
`ENV_VAR` at parse time. It works anywhere an argument appears — upstream
targets, `root` paths, `tls` certificate paths:

```caddyfile
api.example.com {
    reverse_proxy https://{$BACKEND_HOST}:8443
}
```

```bash
BACKEND_HOST=10.0.0.5 raddex run -c Raddexfile
```

A **missing** variable is a validation error, so a config that references a
typoed or unset variable fails `raddex check` instead of starting with a bogus
value — deploy-time mistakes are caught before traffic.

## Combining them

Snippets + env vars are a clean way to share a site template with
environment-specific values:

```caddyfile
(api_site) {
    rate_limit remote_ip 50r/s
    reverse_proxy https://{$API_BACKEND}:8443
}

api.example.com {
    import api_site
}
```

Validate the whole thing before you run it:

```bash
API_BACKEND=10.0.0.1 raddex check -c Raddexfile
API_BACKEND=10.0.0.1 raddex run -c Raddexfile
```
