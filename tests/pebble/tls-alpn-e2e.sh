#!/usr/bin/env bash
# End-to-end ACME order test for TLS-ALPN-01 against a local Pebble server.
#
# Pebble is run in always-valid mode here, so the script exercises the
# instant-acme challenge selection and readiness path without changing the
# host's DNS or firewall. The certificate and ALPN handshake itself is covered
# by the Rust TLS unit test.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$(pwd)"
PEBBLE_DIR="tests/pebble"
PEBBLE_BIN="$ROOT/$PEBBLE_DIR/pebble-linux-amd64/linux/amd64/pebble"
PEBBLE_CA="$ROOT/$PEBBLE_DIR/certs/pebble-ca.pem"
BIN="$ROOT/target/debug/raddex"
CERT_DIR="$(mktemp -d)"
CONFIG="$(mktemp --suffix=.Raddexfile)"
UP_PID=""
PEBBLE_PID=""
RADDEX_PID=""

cleanup() {
  [ -n "$RADDEX_PID" ] && sudo kill "$RADDEX_PID" 2>/dev/null || true
  [ -n "$PEBBLE_PID" ] && kill "$PEBBLE_PID" 2>/dev/null || true
  [ -n "$UP_PID" ] && kill "$UP_PID" 2>/dev/null || true
  rm -rf "$CERT_DIR" "$CONFIG"
}
trap cleanup EXIT

bash "$ROOT/$PEBBLE_DIR/setup.sh"
cargo build -q
python3 -m http.server 19091 --bind 127.0.0.1 >/dev/null 2>&1 &
UP_PID=$!
(
  cd "$ROOT/$PEBBLE_DIR"
  PEBBLE_VA_ALWAYS_VALID=1 "$PEBBLE_BIN" -config config/pebble-config.json \
    >/tmp/raddex_tls_alpn_pebble.log 2>&1
) &
PEBBLE_PID=$!
sleep 2
timeout 5 curl -s --cacert "$PEBBLE_CA" https://localhost:14000/dir >/dev/null

cat >"$CONFIG" <<EOF
{
    tls_alpn_challenge
}
alpn.test {
    reverse_proxy 127.0.0.1:19091
}
EOF

sudo "$BIN" run -c "$CONFIG" \
  --cert-dir "$CERT_DIR" \
  --acme-directory "https://localhost:14000/dir" \
  --acme-root-pem "$PEBBLE_CA" \
  >/tmp/raddex_tls_alpn.log 2>&1 &
RADDEX_PID=$!

for _ in $(seq 1 60); do
  [ -f "$CERT_DIR/alpn.test.pem" ] && break
  sleep 1
done
[ -f "$CERT_DIR/alpn.test.pem" ]
echo "TLS-ALPN-01 ACME order path passed"
