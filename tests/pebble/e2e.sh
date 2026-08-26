#!/usr/bin/env bash
# End-to-end test: raddy auto-provisions a certificate for a named site via
# ACME (against a local Pebble server) and serves HTTPS with it.
#
# Requires: sudo (to bind :443 and :80), a working openssl, network to fetch
# pebble on first run.
#
# Usage: tests/pebble/e2e.sh
set -euo pipefail

cd "$(dirname "$0")/../.."          # repo root
ROOT="$(pwd)"
PEBBLE_DIR="tests/pebble"
BIN="$ROOT/target/debug/raddy"

# --- ensure the pebble binary is present and its config is fresh ---
# setup.sh is idempotent: it downloads the binary only when missing, and always
# regenerates the CA/certs/config (which now carries a short cert validity for
# the M8 renewal phase).
echo "ensuring pebble is set up..."
bash "$ROOT/$PEBBLE_DIR/setup.sh"
PEBBLE_BIN="$ROOT/$PEBBLE_DIR/pebble-linux-amd64/linux/amd64/pebble"

UPSTREAM_PORT=19090
CERT_DIR=$(mktemp -d)
CONFIG=$(mktemp --suffix=.Raddyfile)
PEBBLE_CA="$PEBBLE_DIR/certs/pebble-ca.pem"
cleanup() {
  [ -n "${RADDY_PID:-}" ] && sudo kill "$RADDY_PID" 2>/dev/null || true
  [ -n "${PEBBLE_PID:-}" ] && kill "$PEBBLE_PID" 2>/dev/null || true
  pkill -x pebble 2>/dev/null || true
  [ -n "${UP_PID:-}" ] && kill "$UP_PID" 2>/dev/null || true
  rm -rf "$CERT_DIR" "$CONFIG"
}
trap cleanup EXIT

# --- build ---
cargo build -q

# --- start an upstream ---
python3 -m http.server "$UPSTREAM_PORT" --bind 127.0.0.1 >/dev/null 2>&1 &
UP_PID=$!
sleep 0.5

# --- pebble ---
(
  cd "$ROOT/$PEBBLE_DIR"
  PEBBLE_VA_ALWAYS_VALID=1 "$PEBBLE_BIN" -config config/pebble-config.json >pebble.log 2>&1
) &
PEBBLE_PID=$!
sleep 2
if ! timeout 5 curl -s --cacert "$PEBBLE_CA" https://localhost:14000/dir >/dev/null 2>&1; then
  echo "pebble did not become reachable" >&2
  exit 1
fi

# --- raddy config: one named site on 443, one on a non-443 TLS port (A1) ---
# `alt.test:8443` carries a bare `tls` directive (ACME source) so it is served
# over TLS on 8443; its certificate must be keyed and served as `host:port`.
cat > "$CONFIG" <<EOF
raddy.test {
    reverse_proxy 127.0.0.1:$UPSTREAM_PORT
}
alt.test:8443 {
    tls
    reverse_proxy 127.0.0.1:$UPSTREAM_PORT
}
EOF

# --- run raddy under sudo so :443 can bind ---
# RADDY_RENEW_INTERVAL_SECS=5 makes the M8 renewal scheduler scan every 5s so
# the (90s-valid) Pebble certificate is re-issued within the test window.
sudo RADDY_RENEW_INTERVAL_SECS=5 "$BIN" run -c "$CONFIG" \
  --cert-dir "$CERT_DIR" \
  --acme-directory "https://localhost:14000/dir" \
  --acme-root-pem "$PEBBLE_CA" \
  >/tmp/raddy_e2e.log 2>&1 &
RADDY_PID=$!

# --- wait for the certificate to be issued ---
echo "waiting for ACME certificate..."
for _ in $(seq 1 60); do
  if [ -f "$CERT_DIR/raddy.test.pem" ]; then
    echo "certificate issued: $CERT_DIR/raddy.test.pem"
    break
  fi
  sleep 1
done
if [ ! -f "$CERT_DIR/raddy.test.pem" ]; then
  echo "timed out waiting for certificate; raddy log:" >&2
  tail -30 /tmp/raddy_e2e.log >&2 || true
  exit 1
fi

# --- verify HTTPS through raddy with the issued certificate ---
# `curl --cacert` validates the served leaf against the issued chain, so a 200
# proves the TLS listener served the correct ACME certificate for raddy.test.
STATUS=$(curl -s -o /dev/null -w "%{http_code}" --cacert "$CERT_DIR/raddy.test.pem" \
  --resolve raddy.test:443:127.0.0.1 \
  https://raddy.test/ 2>/dev/null)
if [ "$STATUS" = "200" ]; then
  echo "OK: HTTPS forwarding works with the ACME-issued certificate (status $STATUS)"
else
  echo "FAILED: unexpected HTTPS status: $STATUS" >&2
  exit 1
fi

# --- non-443 TLS site (A1): the certificate is keyed by `host:port` and the
# SNI callback must find it there, or the handshake fails forever. ---
echo "waiting for non-443 ACME certificate..."
for _ in $(seq 1 60); do
  if [ -f "$CERT_DIR/alt.test:8443.pem" ]; then
    echo "certificate issued: $CERT_DIR/alt.test:8443.pem"
    break
  fi
  sleep 1
done
if [ ! -f "$CERT_DIR/alt.test:8443.pem" ]; then
  echo "FAILED: non-443 certificate was not issued; raddy log:" >&2
  tail -30 /tmp/raddy_e2e.log >&2 || true
  exit 1
fi
STATUS=$(curl -s -o /dev/null -w "%{http_code}" --cacert "$CERT_DIR/alt.test:8443.pem" \
  --resolve alt.test:8443:127.0.0.1 \
  https://alt.test:8443/ 2>/dev/null)
if [ "$STATUS" = "200" ]; then
  echo "OK: non-443 HTTPS forwarding works with the ACME-issued certificate (status $STATUS)"
else
  echo "FAILED: non-443 HTTPS unexpected status: $STATUS" >&2
  tail -30 /tmp/raddy_e2e.log >&2 || true
  exit 1
fi

# --- M8: automatic renewal before expiry ---
# The Pebble cert is valid for 90s and the renewal scheduler runs every 5s, so
# the on-disk certificate must be re-issued (different notAfter) within the wait.
OLD_ENDDATE=$(openssl x509 -in "$CERT_DIR/raddy.test.pem" -noout -enddate 2>/dev/null)
echo "renewal: initial notAfter: $OLD_ENDDATE"
RENEWED=""
for _ in $(seq 1 40); do
  NEW_ENDDATE=$(openssl x509 -in "$CERT_DIR/raddy.test.pem" -noout -enddate 2>/dev/null)
  if [ -n "$NEW_ENDDATE" ] && [ "$NEW_ENDDATE" != "$OLD_ENDDATE" ]; then
    RENEWED="$NEW_ENDDATE"
    break
  fi
  sleep 1
done
if [ -z "$RENEWED" ]; then
  echo "FAILED: certificate was not renewed within 40s; raddy log:" >&2
  tail -30 /tmp/raddy_e2e.log >&2 || true
  exit 1
fi
echo "renewal: re-issued, new notAfter: $RENEWED"

# The renewed certificate must still serve HTTPS (it replaced the old one).
STATUS=$(curl -s -o /dev/null -w "%{http_code}" --cacert "$CERT_DIR/raddy.test.pem" \
  --resolve raddy.test:443:127.0.0.1 \
  https://raddy.test/ 2>/dev/null)
if [ "$STATUS" != "200" ]; then
  echo "FAILED: HTTPS broken after renewal: $STATUS" >&2
  exit 1
fi
echo "renewal: HTTPS still serves after re-issue (status $STATUS)"

echo "e2e PASSED"
