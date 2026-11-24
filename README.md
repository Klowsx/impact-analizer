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
# Run tests
cargo test

# Materialize the reproducible 4-repository demo workspace in ./fixtures/demo-workspace
cargo run -p laine-cli -- demo-setup ./fixtures/demo-workspace
```
