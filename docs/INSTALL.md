# Installation and deployment

This document is the operator-facing installation record for Raddex. The
task-oriented version is available in the [documentation site](https://chulingera2025.github.io/raddex/).

## Release binaries

Prebuilt release archives currently target Linux GNU on `x86_64` and `aarch64`.
Download the installer and checksum file from the release:

```bash
curl -fsSLO https://github.com/chulingera2025/raddex/releases/latest/download/install.sh
curl -fsSLO https://github.com/chulingera2025/raddex/releases/latest/download/SHA256SUMS
grep '  install.sh$' SHA256SUMS | shasum -a 256 -c -
less install.sh
./install.sh
raddex --version
```

The installer downloads the matching archive, verifies its checksum, and only
then installs `/usr/local/bin/raddex`. Pass a release tag and prefix when needed:

```bash
./install.sh v0.3.6 /usr/local
./install.sh v0.3.6 "$HOME/.local"
```

For a manual installation, download the matching archive and `SHA256SUMS`, run
`shasum -a 256 -c SHA256SUMS`, extract the archive, and install the `raddex`
binary with mode `0755`.

## Build from source

Source builds require stable Rust, OpenSSL development libraries, CMake, and a
C compiler:

```bash
cargo build --release --locked
./target/release/raddex --version
```

## Files and permissions

A typical host layout is:

```text
/etc/raddex/Raddexfile       configuration
/var/lib/raddex/certs/      ACME account and certificate state
/run/raddex.pid             process identity for upgrade
/run/raddex_upgrade.sock    listener handoff socket
/var/log/raddex/            optional access logs
```

The service account must read the configuration and write the certificate
directory. Protect the certificate directory because it contains ACME account
credentials. Keep access logs separate from certificate state and configure
rotation for the process's long-lived file handle.

## systemd

The repository includes [`examples/raddex.service`](../examples/raddex.service):

```bash
sudo install -Dm644 examples/raddex.service /etc/systemd/system/raddex.service
sudo install -d -m0750 /etc/raddex /var/lib/raddex/certs
sudo install -m0640 Raddexfile /etc/raddex/Raddexfile
sudo systemctl daemon-reload
sudo systemctl enable --now raddex
```

Automatic HTTPS normally needs ports 80 and 443. Run with the required bind
privilege or grant `CAP_NET_BIND_SERVICE`. Transparent TCP additionally needs
the Linux networking capabilities described in the [layer 4 guide](https://chulingera2025.github.io/raddex/guides/layer4/).

Validate and reload:

```bash
sudo raddex check -c /etc/raddex/Raddexfile
sudo systemctl reload raddex
```

## Docker

Build the included image and mount configuration and ACME state explicitly:

```bash
docker build -t raddex .
docker run --rm -p 80:80 -p 443:443 \
  -v "$PWD/Raddexfile:/etc/raddex/Raddexfile:ro" \
  -v raddex_certs:/etc/raddex/certs \
  raddex run -c /etc/raddex/Raddexfile --cert-dir /etc/raddex/certs
```

## Reload and upgrade policy

Use SIGHUP for routing changes that keep the listener topology unchanged. Use
`raddex upgrade` when replacing the binary or transferring compatible listeners:

```bash
raddex upgrade \
  -c /etc/raddex/Raddexfile \
  --cert-dir /var/lib/raddex/certs \
  --pidfile /run/raddex.pid \
  --upgrade-sock /run/raddex_upgrade.sock
```

Topology changes are intentionally rejected by reload and upgrade preflight.
Transparent TCP uses a custom listener and requires a normal restart. Linux UDP
listener and flow handoff is verified and fails closed when state cannot be
transferred.

## Preflight checklist

- Verify the release checksum and record the version.
- Run `raddex check` with the same path used by the service.
- Confirm DNS, ports, firewall rules, and upstream reachability.
- Persist and protect `--cert-dir`.
- Bind metrics to a private address and configure log rotation.
- Test reload, upgrade, and rollback with the production listener topology.
