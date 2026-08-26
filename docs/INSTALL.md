# Installation

Releases ship a **checksum-verified installer** and a **manual path** — neither relies on `curl | sudo bash`. The installer downloads, verifies the sha256, then installs.

## Option 1: Installer script (recommended)

The release assets include `install.sh` (download and review it first):

```bash
# Download and review the script, then run it
curl -fsSL -O https://github.com/chulingera2025/raddy/releases/latest/download/install.sh
# Optional: verify the script's own checksum against the release SHA256SUMS
shasum -a 256 -c SHA256SUMS
./install.sh                  # installs to /usr/local/bin/raddy
./install.sh v0.1.2 ~/.local  # specific version and prefix
```

The script picks `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu` from `uname -m`, downloads the matching tarball and `SHA256SUMS`, and only runs `install` after `shasum -a 256 -c` passes. A failed check aborts without installing.

## Option 2: Manual install

1. From [Releases](https://github.com/chulingera2025/raddy/releases), download `raddy-<arch>.tar.gz` for your architecture and `SHA256SUMS`.
2. Verify:
   ```bash
   shasum -a 256 -c SHA256SUMS
   ```
   The output must include `<filename>: OK`.
3. Extract and install:
   ```bash
   tar -xzf raddy-<arch>.tar.gz -C /usr/local
   raddy --version
   ```

## Option 3: Build from source

```bash
cargo build --release
./target/release/raddy --version
```

System dependencies: stable Rust, OpenSSL dev libraries (`libssl-dev` / `openssl`), and `cmake` (required by pingora's `libz-ng-sys`).

## Docker

The image does **not** bake in a Raddyfile, so mount your own read-only. The
image's `ENTRYPOINT` is `raddy`, so the container command is the `run`
subcommand itself. Run these from the directory that contains your `Raddyfile`:

```bash
docker build -t raddy .
docker run --rm -p 8080:8080 \
  -v "$PWD/Raddyfile:/etc/raddy/Raddyfile:ro" \
  raddy run -c /etc/raddy/Raddyfile
```

To keep ACME certificates across container restarts, mount a certificate
directory and point `--cert-dir` at it:

```bash
docker run --rm -p 80:80 -p 443:443 \
  -v "$PWD/Raddyfile:/etc/raddy/Raddyfile:ro" \
  -v raddy_certs:/etc/raddy/certs \
  raddy run -c /etc/raddy/Raddyfile --cert-dir /etc/raddy/certs
```

> Note: integrity is guaranteed with **sha256 checksums** (verified by the installer). Code signing with a published release key (e.g. minisign/cosign) is a future enhancement.

## Run as a systemd service

`examples/raddy.service` is a ready-made unit that starts raddy on boot, hot-reloads
it on `systemctl reload` (SIGHUP), and restarts it on failure:

```bash
sudo install -Dm644 examples/raddy.service /etc/systemd/system/raddy.service
sudo systemctl daemon-reload
sudo systemctl enable --now raddy
```

Because automatic HTTPS binds ports 80/443, the service runs as root (or grant
`CAP_NET_BIND_SERVICE`). It expects the config at `/etc/raddy/Raddyfile` and
stores certificates under `/var/lib/raddy/certs` — edit the unit to match your
layout. The unit's `ExecStart` flags are also what `raddy upgrade` must be
invoked with for zero-downtime binary upgrades.

## Verify the install

```bash
raddy check -c <your Raddyfile>   # validate the config
raddy run -c <your Raddyfile>     # run
```
