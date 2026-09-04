---
title: CLI reference
description: Commands and options for validating, running, importing, and upgrading Raddex.
---

The CLI has four operational commands:

```text
raddex run       Run the proxy in the foreground
raddex check     Validate a Raddexfile without binding listeners
raddex upgrade   Replace a running binary without dropping listeners
raddex import    Convert a supported Caddyfile or nginx.conf subset
```

Use `raddex --version` to print the installed release and `raddex <command>
--help` for the exact options shipped by that binary.

## `raddex check`

Validate the configuration and exit:

```bash
raddex check -c Raddexfile
```

This runs the same configuration checks used by a reload. Exit status `0`
means the file is valid; exit status `1` reports the first error and prevents
the process from starting or reloading.

| Option | Default | Description |
| --- | --- | --- |
| `-c, --config <file>` | `Raddexfile` | Configuration path |

## `raddex run`

Run the proxy in the foreground:

```bash
raddex run -c Raddexfile
```

| Option | Default | Description |
| --- | --- | --- |
| `-c, --config <file>` | `Raddexfile` | Configuration path |
| `--cert-dir <dir>` | `raddex_certs` | ACME certificates and account credentials |
| `--acme-directory <url>` | Let's Encrypt production | ACME directory endpoint |
| `--acme-root-pem <file>` | unset | Root CA for a private ACME test server |
| `--access-log <file>` | unset | Append JSON access logs to a file |
| `--metrics-addr <addr>` | unset | Expose Prometheus `/metrics` |
| `--pidfile <file>` | unset | PID file used by `raddex upgrade` |
| `--upgrade-sock <sock>` | `/tmp/raddex_upgrade.sock` | Unix socket used for listener handoff |
| `--threads <n>` | `1` | Worker threads per listener runtime: the HTTP service and each layer-4 TCP/UDP listener. Each layer-4 listener also binds one `SO_REUSEPORT` socket per thread, so this sets its accept and receive parallelism |
| `-t, --test` | off | Validate construction without binding listeners |
| `-u, --upgrade` | off | Start as the replacement side of an upgrade |

The `--access-log` and `--metrics-addr` options are deployment settings, not
Raddexfile directives. The `access_log` directive configures global logging or
disables it per site; the CLI flag takes precedence when both are present.

## `raddex upgrade`

Upgrade a running process using the new binary in the current environment:

```bash
raddex upgrade \
  -c /etc/raddex/Raddexfile \
  --cert-dir /var/lib/raddex/certs \
  --pidfile /run/raddex.pid \
  --upgrade-sock /run/raddex_upgrade.sock
```

The command validates the new configuration, starts a replacement with
`--upgrade`, transfers the listener topology, and asks the old process to
finish. The configuration's listener topology must match the running instance.
Routing changes on existing listeners are reloadable. Adding, removing, or
rebinding a listener requires a normal restart; an upgrade is valid only when
the listener topology is unchanged.

Pass the same deployment flags used by the running service, including
`--cert-dir`, `--access-log`, and `--metrics-addr` when they are configured.
Pass the same `--threads` value as the running process when comparing or
replacing a worker configuration.
Transparent TCP uses a custom Linux listener and must use a normal restart.
UDP listener and flow handoff is supported on Linux when the upgrade preflight
and handoff checks pass.

## `raddex import`

Convert a supported subset of a Caddyfile or nginx configuration:

```bash
raddex import caddyfile source.Caddyfile -o Raddexfile
raddex import nginx nginx.conf -o Raddexfile
```

Omit `-o` to print the result to stdout. The converter is independent of the
Raddexfile grammar and validates the emitted configuration before printing it.
Unsupported directives are reported rather than silently approximated.

## Exit status

- `0`: command completed successfully.
- `1`: invalid configuration, failed startup, failed conversion, or failed
  upgrade preflight.

Always run `raddex check` before changing a running service. See [Deployment and
operations](../operations/deployment/) for service-manager examples.
