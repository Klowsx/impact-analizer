## Summary

<!-- Explain WHAT changed and WHY. Link related issues (e.g. Closes #12). -->

## Type of Change

- [ ] `feat` — New feature
- [ ] `fix` — Bug fix
- [ ] `refactor` — Refactoring (no functional change)
- [ ] `test` — Tests or fixtures
- [ ] `docs` — Documentation / ADR
- [ ] `ci` / `chore` — Tooling, CI, or dependencies
- [ ] Breaking change (`!`)

## Verification

<!-- Describe how you verified the change (commands, fixtures, edge cases). -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`

## Checklist

- [ ] PR title follows Conventional Commits (`<type>(<scope>): <description>`)
- [ ] Added unit/integration tests covering positive, negative, and/or unresolvable cases
- [ ] No unjustified `unwrap()` / `expect()` in library code
- [ ] Updated docs or `PLAN.md` if applicable
