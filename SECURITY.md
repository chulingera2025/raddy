# Security Policy

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.** Report it privately
so it can be fixed and released before it is disclosed.

1. **Preferred:** use GitHub's [private vulnerability reporting][gh-pvr] on this
   repository (Security → "Report a vulnerability").
2. If that is not available, email the maintainer directly (see the repository
   owner's profile) with the subject prefix `[raddy-security]`.

Please include, when possible:

- The affected version / commit
- A minimal reproduction (Raddyfile + request)
- Impact assessment and any suggested fix

## Handling

The maintainer will acknowledge within a reasonable window, coordinate a fix on
the `dev` branch, and publish a patch release on `main` (tagged `v*`) before the
vulnerability is disclosed publicly.

## Scope

The security-relevant surface of this project includes, but is not limited to:

- The config parser (hand-written; fuzz-verified) — untrusted Raddyfiles must
  never panic the process
- TLS / ACME (on-demand certificate issuance, the `ask` authorization callback)
- `trusted_proxies` / client-IP trust model (when shipped)
- Path handling in `file_server` (directory traversal)
- The `raddy upgrade` zero-downtime mechanism (listener fd handoff)

[gh-pvr]: https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/creating-a-repository-security-advisory
