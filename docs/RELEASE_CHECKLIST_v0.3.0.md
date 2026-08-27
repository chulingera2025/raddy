# Raddy v0.3.0 Release Checklist

This is a historical record of the `v0.3.0` release. Its follow-up items and
known limitations describe that release, not the current `v0.3.5` tree. See
[`RELEASE_CHECKLIST_v0.3.5.md`](RELEASE_CHECKLIST_v0.3.5.md) and the current
[capability boundaries](PINGORA_CAPABILITY_RESEARCH.md) for present behavior.

Status: released on 2026-08-26.

## Completed

- [x] Version metadata updated to `0.3.0`.
- [x] HTTP/L7 regression suite passes.
- [x] L4 TCP, SNI passthrough, and UDP integration coverage passes.
- [x] UDP reload coverage includes `max_datagram_size` changes.
- [x] ClientHello fuzz target added and compiled.
- [x] Short ClientHello fuzz run completed with no sanitizer findings.
- [x] `cargo fmt --all -- --check` passes.
- [x] `cargo clippy --all-targets --locked -- -D warnings` passes.
- [x] `cargo test --all-targets --locked` passes.
- [x] `cargo build --release --locked` passes.
- [x] Pebble ACME e2e passes, including non-443 TLS and renewal.
- [x] Documentation link check passes for 41 pages.
- [x] Astro documentation production build passes.
- [x] Release scope reviewed and merged through PR #20.
- [x] `v0.3.0` tag created and pushed.
- [x] GitHub release assets and installer checksums verified.

## Post-release follow-up

- [ ] Run direct-versus-proxy TCP and UDP benchmarks and record results.
- [ ] Perform an independent security and operations review.
- [ ] Update GitHub Actions dependencies after the Node.js 20 deprecation warning
      is actionable for the pinned action versions.

## Known limitations

- UDP listeners and active UDP flows are not transferred by zero-downtime
  upgrade; use a plain restart for UDP-enabled configurations.
- The current SNI routing implementation supports exact names only; wildcard
  SNI and TLS termination are deferred.
- Upstream HTTP/2 and cleartext h2c remain deferred.
