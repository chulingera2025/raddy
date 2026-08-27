# Installation and deployment

This document is the operator-facing installation record for Raddy. The
task-oriented version is available in the [documentation site](https://chulingera2025.github.io/raddy/).

## Release binaries

Prebuilt release archives currently target Linux GNU on `x86_64` and `aarch64`.
Download the installer and checksum file from the release:

```bash
curl -fsSLO https://github.com/chulingera2025/raddy/releases/latest/download/install.sh
curl -fsSLO https://github.com/chulingera2025/raddy/releases/latest/download/SHA256SUMS
grep '  install.sh$' SHA256SUMS | shasum -a 256 -c -
less install.sh
./install.sh
raddy --version
```

The installer downloads the matching archive, verifies its checksum, and only
then installs `/usr/local/bin/raddy`. Pass a release tag and prefix when needed:

```bash
./install.sh v0.3.5 /usr/local
./install.sh v0.3.5 "$HOME/.local"
```

For a manual installation, download the matching archive and `SHA256SUMS`, run
`shasum -a 256 -c SHA256SUMS`, extract the archive, and install the `raddy`
binary with mode `0755`.

## Build from source

Source builds require stable Rust, OpenSSL development libraries, CMake, and a
C compiler:

```bash
cargo build --release --locked
./target/release/raddy --version
```

## Files and permissions

A typical host layout is:

```text
/etc/raddy/Raddyfile       configuration
/var/lib/raddy/certs/      ACME account and certificate state
/run/raddy.pid             process identity for upgrade
/run/raddy_upgrade.sock    listener handoff socket
/var/log/raddy/            optional access logs
```

The service account must read the configuration and write the certificate
directory. Protect the certificate directory because it contains ACME account
credentials. Keep access logs separate from certificate state and configure
rotation for the process's long-lived file handle.

## systemd

The repository includes [`examples/raddy.service`](../examples/raddy.service):

```bash
sudo install -Dm644 examples/raddy.service /etc/systemd/system/raddy.service
sudo install -d -m0750 /etc/raddy /var/lib/raddy/certs
sudo install -m0640 Raddyfile /etc/raddy/Raddyfile
sudo systemctl daemon-reload
sudo systemctl enable --now raddy
```

Automatic HTTPS normally needs ports 80 and 443. Run with the required bind
privilege or grant `CAP_NET_BIND_SERVICE`. Transparent TCP additionally needs
the Linux networking capabilities described in the [layer 4 guide](https://chulingera2025.github.io/raddy/guides/layer4/).

Validate and reload:

```bash
sudo raddy check -c /etc/raddy/Raddyfile
sudo systemctl reload raddy
```

## Docker

Build the included image and mount configuration and ACME state explicitly:

```bash
docker build -t raddy .
docker run --rm -p 80:80 -p 443:443 \
  -v "$PWD/Raddyfile:/etc/raddy/Raddyfile:ro" \
  -v raddy_certs:/etc/raddy/certs \
  raddy run -c /etc/raddy/Raddyfile --cert-dir /etc/raddy/certs
```

## Reload and upgrade policy

Use SIGHUP for routing changes that keep the listener topology unchanged. Use
`raddy upgrade` when replacing the binary or transferring compatible listeners:

```bash
raddy upgrade \
  -c /etc/raddy/Raddyfile \
  --cert-dir /var/lib/raddy/certs \
  --pidfile /run/raddy.pid \
  --upgrade-sock /run/raddy_upgrade.sock
```

Topology changes are intentionally rejected by reload and upgrade preflight.
Transparent TCP uses a custom listener and requires a normal restart. Linux UDP
listener and flow handoff is verified and fails closed when state cannot be
transferred.

## Preflight checklist

- Verify the release checksum and record the version.
- Run `raddy check` with the same path used by the service.
- Confirm DNS, ports, firewall rules, and upstream reachability.
- Persist and protect `--cert-dir`.
- Bind metrics to a private address and configure log rotation.
- Test reload, upgrade, and rollback with the production listener topology.
