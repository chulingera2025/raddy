<!-- Thanks for the PR! Fill in the sections below; delete the hints. -->

## Related issue

<!-- Link the issue this PR closes (dev workflow: issue → develop → commit → push dev).
     `Closes #N` auto-closes it on merge to main. -->

Closes #

## Summary of changes

<!-- What changed and why. Reference the ROADMAP milestone / acceptance criterion
     this satisfies where applicable. -->

## Raddexfile spec

<!-- If this PR touches config grammar: the Raddexfile is a public interface —
     the spec change in docs/RADDEXFILE_SPEC.md MUST land in the same PR
     (spec-first red line). Link the section. -->

- [ ] No grammar change
- [ ] Grammar change — spec updated: <!-- link/section -->

## Test coverage

<!-- Unit + integration tests. Parser changes MUST include new Raddexfile cases
     AND fuzz coverage (`cargo +nightly fuzz run parse_config`). -->

- [ ] `cargo test --all-targets`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo fmt --check`
- [ ] Fuzz coverage updated (parser changes only)
- [ ] Specified in acceptance criteria of the linked issue, all checked

## Notes for reviewers

<!-- Anything non-obvious: design trade-offs, behavior that is intentionally
     NOT covered, follow-up work. -->
