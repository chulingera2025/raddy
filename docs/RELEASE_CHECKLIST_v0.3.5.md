# Raddex v0.3.5 Release Checklist

## Scope

- [x] Application-level Pingora work completed: upstream H2/h2c, multi-domain
  sites, IPv6 sites/listeners, wildcard SNI, L4 TLS termination, and
  TLS-ALPN-01.
- [x] Linux integrations completed: transparent TCP routing and UDP listener /
  flow-state handoff during zero-downtime upgrade.
- [x] QUIC/HTTP3 boundary documented: UDP passthrough is available, while
  terminating QUIC/HTTP3 remains a separate sidecar.
- [x] DNS-01 provider expansion issues remain outside this release scope.

## Configuration and compatibility

- [x] `Cargo.toml`, `Cargo.lock`, CLI metadata, and badges report `0.3.5`.
- [x] Raddexfile specification and Chinese specification describe only tested
  syntax and behavior.
- [x] Existing HTTP/1.1, TLS, mTLS, raw TCP, SNI passthrough, UDP, reload, and
  TCP upgrade behavior remains covered by regression tests.
- [x] IPv4-mapped IPv6 peer addresses normalize before trusted-proxy,
  rate-limit, access-log, and source-IP-hash decisions.

## Verification

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --all-targets --locked -- -D warnings`
- [x] `cargo test --all-targets --locked`
- [x] `cargo build --release --locked`
- [x] TLS-ALPN challenge certificate and ALPN-selection handshake tests pass.
- [x] `tests/pebble/tls-alpn-e2e.sh` passes against local Pebble.
- [x] UDP upgrade transfers a persistent client flow without rebinding.
- [x] UDP IPv6 upstream round-trip passes.
- [x] Documentation links and generated site build pass.

## Release

- [x] Commit the release changes.
- [x] Push the release branch and open a pull request.
- [x] Merge the pull request into `main`.
- [x] Create and push the `v0.3.5` tag from the merge commit.
- [x] Publish the GitHub release and verify installer artifacts/checksums.

Release evidence:

- Merge commit: `ee27445a50a14b64d5cb78d342c3fceee373cd6a`.
- Pull request: [#24](https://github.com/chulingera2025/raddex/pull/24).
- Release: [v0.3.5](https://github.com/chulingera2025/raddex/releases/tag/v0.3.5).
