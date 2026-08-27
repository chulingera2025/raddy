---
title: Deployment and operations
description: Deploy Raddy with a verified binary, systemd, or Docker and operate reloads, upgrades, certificates, and permissions safely.
---

This guide covers the operational decisions that matter after the first local
request works. Start with [Installation](../../install/) if Raddy is not on the
machine yet.

## Choose a process model

| Model | Best for | Important detail |
| --- | --- | --- |
| Foreground process | Development, containers, supervisors | The parent process owns shutdown and logging |
| systemd | Linux hosts with local listeners | Use the provided unit as a starting point |
| Docker | Containerized HTTP deployments | Mount configuration and certificate storage explicitly |

Raddy does not daemonize itself. Let the service manager own restart policy,
resource limits, and standard output handling.

## Validate before every change

```bash
raddy check -c /etc/raddy/Raddyfile
```

Keep the command in deployment automation. A successful check proves the file
passes parser and validation checks; it does not prove that DNS, certificates,
upstreams, firewall rules, or external credentials are reachable.

## systemd

The repository includes [`examples/raddy.service`](https://github.com/chulingera2025/raddy/blob/main/examples/raddy.service).
Install it, then adjust the paths and user model to your host:

```bash
sudo install -Dm644 examples/raddy.service /etc/systemd/system/raddy.service
sudo install -d -m0750 /etc/raddy /var/lib/raddy/certs
sudo install -m0640 Raddyfile /etc/raddy/Raddyfile
sudo systemctl daemon-reload
sudo systemctl enable --now raddy
```

Automatic HTTPS normally needs ports 80 and 443. Use root, `CAP_NET_BIND_SERVICE`,
or a socket/port arrangement that gives the service access to those ports. The
certificate directory must be writable by the account running Raddy and should
not be world-readable because it contains the ACME account credentials.

Check the service and recent logs:

```bash
sudo systemctl status raddy
sudo journalctl -u raddy -n 100 --no-pager
```

## Reload versus upgrade

Use reload for configuration changes that keep the listener topology stable:

```bash
sudo raddy check -c /etc/raddy/Raddyfile
sudo systemctl reload raddy
```

SIGHUP swaps the routing snapshot atomically. Existing HTTP, TCP, and UDP work
keeps its selected upstream; new work uses the new configuration.

Use `raddy upgrade` when replacing the binary or when a listener handoff is
needed:

```bash
sudo raddy upgrade \
  -c /etc/raddy/Raddyfile \
  --cert-dir /var/lib/raddy/certs \
  --pidfile /run/raddy.pid \
  --upgrade-sock /run/raddy_upgrade.sock
```

Pass the same deployment flags used by the running instance. The replacement
must agree with it on listener topology.
Changing a bind address, adding a listener, or removing one is not a reloadable
routing change. Transparent TCP uses a custom listener and requires a normal
restart. UDP handoff is available on Linux only and fails closed if the
listener or flow state cannot be transferred.

## Certificates and ACME

- Use `tls internal` only for development or a private trust domain.
- Use static `tls <cert> <key>` when another system owns certificate renewal.
- Use the default HTTP-01 challenge when the ACME server can reach port 80.
- Use `dns_challenge cloudflare <token>` when DNS-01 is the appropriate method.
- Use `tls_alpn_challenge` only for eligible ACME sites on TCP 443.

Persist `--cert-dir` across restarts and container replacement. A certificate
renewal failure leaves the existing certificate serving; monitor logs and the
certificate expiry rather than assuming issuance succeeded because the process
started.

## Docker

Mount the Raddyfile read-only and persist the certificate directory:

```bash
docker run --rm \
  -p 80:80 -p 443:443 \
  -v "$PWD/Raddyfile:/etc/raddy/Raddyfile:ro" \
  -v raddy_certs:/etc/raddy/certs \
  raddy run -c /etc/raddy/Raddyfile --cert-dir /etc/raddy/certs
```

For transparent TCP, a container also needs the host networking and Linux
capabilities required by TPROXY. Treat that as a host-network deployment and
verify the kernel routing rules independently.

## Observe the process

Enable metrics on a private address:

```bash
raddy run -c /etc/raddy/Raddyfile --metrics-addr 127.0.0.1:9100
curl http://127.0.0.1:9100/metrics
```

Use JSON access logs for ingestion and `format=common` for traditional tools.
The file handle is opened at startup; plan rotation accordingly. See [Metrics](../metrics/)
and [Access log](../access-log/) for field and rotation details.

## Production checklist

- Pin the release version and verify `SHA256SUMS`.
- Run `raddy check` before start and reload.
- Persist and protect the ACME certificate directory.
- Confirm DNS, ports 80/443, upstream reachability, and IPv6 behavior.
- Bind metrics to a private address and configure log rotation.
- Test a rollback using the same listener topology.
- For Linux transparent or UDP handoff deployments, test privileges and kernel
  rules on the target host rather than only in a development container.
