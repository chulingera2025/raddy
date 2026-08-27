---
title: CLI reference
description: Commands and options for validating, running, importing, and upgrading Raddy.
---

The CLI has four operational commands:

```text
raddy run       Run the proxy in the foreground
raddy check     Validate a Raddyfile without binding listeners
raddy upgrade   Replace a running binary without dropping listeners
raddy import    Convert a supported Caddyfile or nginx.conf subset
```

Use `raddy --version` to print the installed release and `raddy <command>
--help` for the exact options shipped by that binary.

## `raddy check`

Validate the configuration and exit:

```bash
raddy check -c Raddyfile
```

This runs the same configuration checks used by a reload. Exit status `0`
means the file is valid; exit status `1` reports the first error and prevents
the process from starting or reloading.

| Option | Default | Description |
| --- | --- | --- |
| `-c, --config <file>` | `Raddyfile` | Configuration path |

## `raddy run`

Run the proxy in the foreground:

```bash
raddy run -c Raddyfile
```

| Option | Default | Description |
| --- | --- | --- |
| `-c, --config <file>` | `Raddyfile` | Configuration path |
| `--cert-dir <dir>` | `raddy_certs` | ACME certificates and account credentials |
| `--acme-directory <url>` | Let's Encrypt production | ACME directory endpoint |
| `--acme-root-pem <file>` | unset | Root CA for a private ACME test server |
| `--access-log <file>` | unset | Append JSON access logs to a file |
| `--metrics-addr <addr>` | unset | Expose Prometheus `/metrics` |
| `--pidfile <file>` | unset | PID file used by `raddy upgrade` |
| `--upgrade-sock <sock>` | `/tmp/raddy_upgrade.sock` | Unix socket used for listener handoff |
| `-t, --test` | off | Validate construction without binding listeners |
| `-u, --upgrade` | off | Start as the replacement side of an upgrade |

The `--access-log` and `--metrics-addr` options are deployment settings, not
Raddyfile directives. The `access_log` directive configures global logging or
disables it per site; the CLI flag takes precedence when both are present.

## `raddy upgrade`

Upgrade a running process using the new binary in the current environment:

```bash
raddy upgrade \
  -c /etc/raddy/Raddyfile \
  --cert-dir /var/lib/raddy/certs \
  --pidfile /run/raddy.pid \
  --upgrade-sock /run/raddy_upgrade.sock
```

The command validates the new configuration, starts a replacement with
`--upgrade`, transfers the listener topology, and asks the old process to
finish. The configuration's listener topology must match the running instance.
Routing changes on existing listeners are reloadable. Adding, removing, or
rebinding a listener requires a normal restart; an upgrade is valid only when
the listener topology is unchanged.

Pass the same deployment flags used by the running service, including
`--cert-dir`, `--access-log`, and `--metrics-addr` when they are configured.
Transparent TCP uses a custom Linux listener and must use a normal restart.
UDP listener and flow handoff is supported on Linux when the upgrade preflight
and handoff checks pass.

## `raddy import`

Convert a supported subset of a Caddyfile or nginx configuration:

```bash
raddy import caddyfile source.Caddyfile -o Raddyfile
raddy import nginx nginx.conf -o Raddyfile
```

Omit `-o` to print the result to stdout. The converter is independent of the
Raddyfile grammar and validates the emitted configuration before printing it.
Unsupported directives are reported rather than silently approximated.

## Exit status

- `0`: command completed successfully.
- `1`: invalid configuration, failed startup, failed conversion, or failed
  upgrade preflight.

Always run `raddy check` before changing a running service. See [Deployment and
operations](../operations/deployment/) for service-manager examples.
