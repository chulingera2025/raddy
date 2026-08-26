# Raddy v0.3.0 Release Checklist

Status: release candidate in the working tree. No commit, tag, or GitHub
release has been created by this task.

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

## Before tagging

- [ ] Review the complete diff and split unrelated changes if needed.
- [ ] Run direct-versus-proxy TCP and UDP benchmarks and record results.
- [ ] Perform an independent security and operations review.
- [ ] Confirm the release notes and upgrade instructions.
- [ ] Commit the release candidate.
- [ ] Create and push the `v0.3.0` tag.
- [ ] Verify the GitHub release artifacts and installer checksums.

## Known limitations

- UDP listeners and active UDP flows are not transferred by zero-downtime
  upgrade; use a plain restart for UDP-enabled configurations.
- The current SNI routing implementation supports exact names only; wildcard
  SNI and TLS termination are deferred.
- Upstream HTTP/2 and cleartext h2c remain deferred.
