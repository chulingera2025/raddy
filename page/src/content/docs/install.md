---
title: Installation
description: Install a verified Linux release, build Raddex from source, or run it in Docker.
---

The release page provides checksum-verified Linux binaries for `x86_64` and
`aarch64`. Other platforms can build from source. The installer downloads the
matching archive and verifies its checksum before it writes the binary.

## Install a release

Download the installer and its checksum file, inspect the script, then run it:

```bash
curl -fsSLO https://github.com/chulingera2025/raddex/releases/latest/download/install.sh
curl -fsSLO https://github.com/chulingera2025/raddex/releases/latest/download/SHA256SUMS
grep '  install.sh$' SHA256SUMS | shasum -a 256 -c -
less install.sh
./install.sh
raddex --version
```

The default prefix is `/usr/local`. A specific release and prefix can be
selected explicitly:

```bash
./install.sh v0.3.6 "$HOME/.local"
```

If the prefix is not on `PATH`, invoke the binary by its full path or update
your shell configuration.

## Install manually

1. Download `raddex-<arch>-unknown-linux-gnu.tar.gz` and `SHA256SUMS` from the
   [release page](https://github.com/chulingera2025/raddex/releases).
2. Verify the archive:

   ```bash
   shasum -a 256 -c SHA256SUMS
   ```

3. Extract the matching archive and install the binary:

   ```bash
   tar -xzf raddex-<arch>-unknown-linux-gnu.tar.gz
   sudo install -Dm755 raddex /usr/local/bin/raddex
   raddex --version
   ```

## Build from source

Requirements:

- stable Rust and Cargo;
- OpenSSL development libraries;
- CMake and a C compiler.

Build and verify the release binary:

```bash
cargo build --release --locked
./target/release/raddex --version
```

## Run in Docker

Build the included image, mount the configuration read-only, and expose only
the listener ports you use:

```bash
docker build -t raddex .
docker run --rm -p 8080:8080 \
  -v "$PWD/Raddexfile:/etc/raddex/Raddexfile:ro" \
  raddex run -c /etc/raddex/Raddexfile
```

For ACME, persist the certificate directory and publish ports 80 and 443:

```bash
docker run --rm -p 80:80 -p 443:443 \
  -v "$PWD/Raddexfile:/etc/raddex/Raddexfile:ro" \
  -v raddex_certs:/etc/raddex/certs \
  raddex run -c /etc/raddex/Raddexfile --cert-dir /etc/raddex/certs
```

## Verify before deployment

```bash
raddex check -c /etc/raddex/Raddexfile
```

For systemd, service hardening, certificate permissions, reload, and upgrade
procedures, continue with [Deployment and operations](../operations/deployment/).
