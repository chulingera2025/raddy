#!/usr/bin/env bash
# Raddy installer.
#
# Downloads the release binary for the current architecture, verifies its
# sha256 checksum against the release's SHA256SUMS, and installs it — never
# pipes an unverified script into a shell. Review this file before running.
#
# Usage:
#   ./install.sh [version] [prefix]
#     version: release tag (default: latest)
#     prefix:  install prefix (default: /usr/local)
#
# Manual install (no script):
#   Download raddy-<arch>.tar.gz + SHA256SUMS from the release, then:
#     shasum -a 256 -c SHA256SUMS
#     tar -xzf raddy-<arch>.tar.gz -C /usr/local
set -euo pipefail

VERSION="${1:-latest}"
PREFIX="${2:-/usr/local}"
# Overridable so forks can repoint the installer.
BASE_URL="${RADDY_RELEASE_BASE:-https://github.com/chulingera2025/raddy/releases/download}"

case "$(uname -m)" in
  x86_64 | amd64) ARCH="x86_64-unknown-linux-gnu" ;;
  aarch64 | arm64) ARCH="aarch64-unknown-linux-gnu" ;;
  *)
    echo "unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

release() {
  if [ "$VERSION" = "latest" ]; then
    echo "$BASE_URL/latest"
  else
    echo "$BASE_URL/$VERSION"
  fi
}
URL="$(release)/raddy-$ARCH.tar.gz"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "downloading $URL"
curl -fsSL -o "$TMP/raddy-$ARCH.tar.gz" "$URL"
curl -fsSL -o "$TMP/SHA256SUMS" "$(release)/SHA256SUMS"

echo "verifying checksum..."
(
  cd "$TMP"
  # The release SHA256SUMS lists every architecture's tarball, but only the
  # tarball for THIS architecture was downloaded — a full `shasum -c` would
  # fail on the missing files. Extract the matching line and verify it alone.
  line="$(grep "  raddy-$ARCH.tar.gz$" SHA256SUMS || true)"
  [ -n "$line" ] || exit 1
  printf '%s\n' "$line" | shasum -a 256 -c
) || {
  echo "checksum verification FAILED; refusing to install" >&2
  exit 1
}

tar -xzf "$TMP/raddy-$ARCH.tar.gz" -C "$TMP"
install -Dm755 "$TMP/raddy" "$PREFIX/bin/raddy"
echo "installed raddy to $PREFIX/bin/raddy"
