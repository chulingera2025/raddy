# Documentation map and maintenance

Raddy has two documentation surfaces with different audiences.

## Public documentation site

The Astro Starlight site under `page/src/content/docs/` is task-oriented. It
should answer a user's next question with a runnable example and a clear
boundary:

```text
quickstart.md                 first successful local proxy
install.md                    release installation
guides/                       task and protocol recipes
config/                       Raddyfile concepts and reference
operations/                   deployment, observation, and troubleshooting
architecture/                 capability and protocol boundaries
```

Keep links relative so the site continues to work under the GitHub Pages base
path. Raddyfile examples use the `caddyfile` syntax tag.

## Repository documentation

The root `docs/` directory is for durable project records rather than a second
copy of every user guide. The English filenames remain at the root deliberately
so existing links do not break; the table below is the logical grouping:

| File | Role |
| --- | --- |
| Specification | `RADDYFILE_SPEC.md` — configuration compatibility source of truth |
| Architecture | `PINGORA_CAPABILITY_RESEARCH.md` and `L4_PROXY_PLAN.md` — runtime boundaries and invariants |
| Operations | `INSTALL.md` and `PERFORMANCE.md` — deployment and benchmark records |
| Releases | `RELEASE_CHECKLIST_*.md` — historical release evidence |
| `DOCUMENTATION.md` | This structure and maintenance policy |

Implementation notes may live in pull requests or release checklists, but a
released behavior belongs in the specification, a user guide, or the
architecture record before it is advertised as supported.

## Source-of-truth order

When pages disagree, resolve the conflict in this order:

1. Current parser and validator behavior.
2. `docs/RADDYFILE_SPEC.md` for configuration semantics.
3. Focused integration tests for runtime behavior.
4. Public guides and README summaries.

The README is a landing page, not a complete reference. It should link to the
more precise document instead of repeating every default and edge case.

## Feature status labels

Use these terms consistently:

- **Supported** — implemented and verified in the stated release.
- **Linux-only** — supported with host kernel or privilege requirements.
- **Passthrough** — forwarded without terminating the higher-level protocol.
- **Sidecar required** — outside the current Raddy/Pingora protocol stack.

Avoid broad claims such as "supports HTTP/3" when only UDP datagram passthrough
exists.

## Verification

Run the following from the repository root after documentation changes:

```bash
node page/scripts/check-links.mjs
(cd page && npm run build)
```

The content must remain English-only for new or rewritten documentation, use
relative internal links in the site, and keep examples consistent with the
Raddyfile specification and CLI help output.
