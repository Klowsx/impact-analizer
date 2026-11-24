# Laine

> Cross-repository change impact analyzer written in Rust.

Laine detects breaking contract changes across independent Git repositories before code reaches production—without AI, without running untrusted code, and with verifiable file/line evidence.

See [PLAN.md](./PLAN.md) for the full product plan, architecture, and roadmap.

## Workspace layout

- `crates/laine-core` — Domain model, intermediate representation (IR), configuration, dependency graph, and impact engine.
- `crates/laine-ts` — TypeScript/JavaScript analyzer built on `oxc`.
- `crates/laine-cli` — Command-line interface (`laine`).

## Quickstart

```bash
# Run all tests (including end-to-end Phase 1 verification)
cargo test --workspace

# Materialize the reproducible 4-repository demo workspace in ./fixtures/demo-workspace
cargo run -p laine-cli -- demo-setup ./fixtures/demo-workspace

# Run cross-repository impact analysis (terminal report, exit code 2 on breaking changes)
cargo run -p laine-cli -- analyze \
  --config ./fixtures/demo-workspace/laine.toml \
  --repo customer-service \
  --base main \
  --head feature/split-customer-name

# Export deterministic versioned JSON report
cargo run -p laine-cli -- analyze \
  --config ./fixtures/demo-workspace/laine.toml \
  --repo customer-service \
  --base main \
  --head feature/split-customer-name \
  --format json
```
