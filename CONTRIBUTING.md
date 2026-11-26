# Contributing to Laine

Thank you for your interest in contributing to **Laine**. This project follows a lightweight, disciplined Git workflow designed to keep `main` always green and the commit history clean.

## Language

- Write all **branch names**, **commit messages**, **pull request titles and descriptions**, **code comments**, and **documentation** in **English**.

## Branching Strategy (Trunk-Based)

- `main` is always buildable, tested, and ready to release.
- Never commit work-in-progress directly to `main`.
- Create short-lived branches from `main` using `<type>/<short-kebab-description>`:
  - `feat/openapi-contract-diff`
  - `fix/dynamic-url-classification`
  - `refactor/ir-node-ids`
  - `test/golden-json-reports`
  - `docs/adr-001-rust-oxc`
  - `chore/ci-cache`

## Commit Convention (Conventional Commits)

Use [Conventional Commits](https://www.conventionalcommits.org/) in imperative mood, lowercase, with no trailing period:

```text
<type>(<scope>): <short description>

[optional body explaining WHY]

[optional footer: Closes #12 / BREAKING CHANGE: ...]
```

### Types
- `feat` — new user-facing capability or engine feature
- `fix` — bug fix
- `refactor` — code restructuring without behavior change
- `test` — adding or improving tests and fixtures
- `docs` — documentation, ADRs, README, or plans
- `perf` — performance improvement backed by measurements
- `ci` — CI/CD workflow changes
- `chore` — maintenance, tooling, dependency updates

### Scopes
- `core` — `laine-core` (domain model, config, git snapshots, graph, impact engine)
- `openapi` — OpenAPI parsing and contract diffing
- `ts` — `laine-ts` (`oxc` parser, HTTP extraction, data-flow)
- `py` — `laine-py` (Python FastAPI/Flask and `requests`/`httpx` analyzers)
- `cli` — `laine-cli` (commands, flags, output formatting)
- `fixtures` — reproducible demo/test repositories
- `ci` / `docs` / `deps`

## Pull Requests

- **One PR = one focused responsibility.**
- **PR title** must be in English and follow Conventional Commits (because **Squash and Merge** uses the PR title as the commit message on `main`).
- **PR description** must be in English and follow `.github/pull_request_template.md` (`Summary`, `Type of Change`, `Verification`, and `Checklist`).
- Include tests for every new feature or bug fix (including negative and `UNKNOWN`/unresolvable cases where relevant).
- Keep `Cargo.lock` committed: Laine is a CLI application workspace and relies on reproducible builds.

## Local Quality Gates

Run these checks before opening or updating a PR (CI enforces all three):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
