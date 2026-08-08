---
title: CLI reference
description: Every raddy subcommand and its options.
---

The `raddy` binary has three primary subcommands plus one migration helper.

## `raddy run`

Run the reverse proxy server in the foreground.

| Option | Default | Description |
|---|---|---|
| `-c, --config <file>` | `Raddyfile` | Path to the Raddyfile |
| `--cert-dir <dir>` | `raddy_certs` | Directory for ACME certificates and account credentials |
| `--acme-directory <url>` | Let's Encrypt production | ACME directory URL |
| `--acme-root-pem <file>` | — | PEM root CA that trusts the ACME server (required for a test server such as Pebble) |
| `--access-log <file>` | — | Append structured JSON access logs to this file |
| `--metrics-addr <addr>` | — | Expose Prometheus `/metrics` on this address (e.g. `127.0.0.1:9100`) |
| `--pidfile <file>` | — | Write this process's PID to this file so `raddy upgrade` can find it |
| `--upgrade-sock <sock>` | `/tmp/raddy_upgrade.sock` | Unix socket used to hand over listening fds during an upgrade |
| `-u, --upgrade` | — | Start as the *new* side of a zero-downtime upgrade (normally spawned by `raddy upgrade`) |
| `-t, --test` | — | Validate the config and construction, then exit 0/1 without binding any listener (the `raddy upgrade` pre-flight) |

## `raddy upgrade`

Zero-downtime binary upgrade (requires `--pidfile`): pre-flight the new binary,
spawn a replacement with `-u`, then SIGQUIT the running instance. Shares the
same options as `raddy run`.

## `raddy check`

Validate a Raddyfile and exit — the **same checks a reload performs**. A config
that passes `check` reloads cleanly, and vice versa.

```bash
raddy check -c Raddyfile   # prints "Raddyfile: ok", exits 0; or prints the error and exits 1
```

## `raddy import`

Convert a Caddyfile or nginx.conf subset into a Raddyfile. An **independent
converter**: it never changes the Raddyfile grammar, and it validates its own
output (through the same pipeline a reload uses) before printing.

```bash
raddy import caddyfile <source> [-o <output>]
raddy import nginx    <source> [-o <output>]
```

Omitting `-o` prints the Raddyfile to stdout.

## Exit behavior

`check` exits 0 for a valid config and 1 otherwise. `run` and `upgrade` exit 1
on startup errors (e.g. an invalid config, so an invalid config never starts the
process). `import` exits 1 when nothing is convertible or the emitted Raddyfile
fails validation.