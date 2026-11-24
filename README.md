# Laine

> Cross-repository change impact analyzer written in Rust.

Laine detects breaking contract changes across independent Git repositories before code reaches production—without AI, without running untrusted code, and with verifiable file/line evidence.

See [PLAN.md](./PLAN.md) for the full product plan, architecture, and roadmap.

## Workspace layout

- `crates/laine-core` — Domain model, intermediate representation (IR), configuration, dependency graph, and impact engine.
- `crates/laine-ts` — TypeScript/JavaScript analyzer built on `oxc`.
- `crates/laine-cli` — Command-line interface (`laine`).

## Capabilities

- **Producer contract sources:** OpenAPI 3.0/3.1 (YAML & JSON), Express routes (`oxc`), and Laravel PHP routes (`routes/api.php`, `routes/web.php`), plus OpenAPI-vs-code discrepancy detection.
- **Consumer static analysis (`oxc`):** `fetch`, `axios`, `axios.create({ baseURL })`, configurable custom wrappers (`[[clients]]` like `$fetch`, `getClient().fetch`, `apiClient`), mapped env vars (`[env]`), destructuring, local aliases, optional chaining, generic response types, and 1-level local helper propagation.
- **Dependency graph (`petgraph`):** Direct confirmed consumers (`CONFIRMED`), cycle-safe transitive indirect consumers (`POSSIBLE`), and explicit unresolvable dynamic URLs (`UNKNOWN`).
- **CI-ready outputs:** Human-readable terminal reports, byte-for-byte deterministic JSON (`--format json`), and explicit exit codes (`0` compatible, `2` breaking change, `1` error).

## Quickstart

```bash
# Run all tests (unit + end-to-end)
cargo test --workspace

# Initialize a new laine.toml and register services
cargo run -p laine-cli -- init
cargo run -p laine-cli -- repo add ./customer-service --openapi openapi.yaml --base-url http://customer-service

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

## Architecture Decisions

- [ADR-001: Rust + Oxc](./docs/adr/001-rust-and-oxc.md)
- [ADR-002: Deterministic IR and Certainty vs. Severity Model](./docs/adr/002-deterministic-ir-and-certainty.md)
