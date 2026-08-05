#!/usr/bin/env bash
# Prepare the local Pebble ACME test server: download the binary, generate its
# CA + server certificate, and write its config. Run this before e2e.sh.
#
# The generated artifacts (binary, certs, config) are git-ignored.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"                    # tests/pebble (absolute)
ROOT="$(cd .. && pwd)"

PEBBLE_VERSION="v2.10.1"
BIN_DIR="pebble-linux-amd64/linux/amd64"
BIN="$BIN_DIR/pebble"

if [ ! -x "$BIN" ]; then
  echo "downloading pebble $PEBBLE_VERSION..."
  curl -sL -o pebble.tar.gz \
    "https://github.com/letsencrypt/pebble/releases/download/$PEBBLE_VERSION/pebble-linux-amd64.tar.gz"
  tar xzf pebble.tar.gz
  chmod +x "$BIN"
fi

mkdir -p certs config

# Pebble test CA (signs the directory server cert).
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout certs/pebble-ca.key -out certs/pebble-ca.pem -days 3650 -nodes \
  -subj "/CN=Pebble Test CA" 2>/dev/null

# Directory server certificate for localhost/pebble, signed by the CA.
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout certs/localhost.key -out /tmp/raddy_localhost.csr -nodes \
  -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,DNS:pebble" 2>/dev/null
openssl x509 -req -in /tmp/raddy_localhost.csr \
  -CA certs/pebble-ca.pem -CAkey certs/pebble-ca.key -CAcreateserial \
  -out certs/localhost.pem -days 3650 \
  -extfile <(echo "subjectAltName=DNS:localhost,DNS:pebble") 2>/dev/null
rm -f /tmp/raddy_localhost.csr

# Pebble config (absolute paths; httpPort is where Pebble validates HTTP-01,
# unused with PEBBLE_VA_ALWAYS_VALID). The "default" profile overrides Pebble's
# built-in 90-day validity with 90s so the M8 renewal e2e can observe an
# automatic re-issue within the test window.
ABS="$(pwd)"
cat > config/pebble-config.json <<EOF
{
  "pebble": {
    "listenAddress": "0.0.0.0:14000",
    "managementListenAddress": "0.0.0.0:15000",
    "certificate": "$ABS/certs/localhost.pem",
    "privateKey": "$ABS/certs/localhost.key",
    "httpPort": 5002,
    "tlsPort": 5001,
    "ocspResponderURL": "",
    "externalAccountBindingRequired": false,
    "profiles": {
      "default": {
        "description": "Short-lived certificates for the renewal e2e",
        "validityPeriod": 90,
        "maxValidityPeriod": 90
      }
    }
  }
}
EOF

echo "pebble ready at $ROOT/$BIN"
