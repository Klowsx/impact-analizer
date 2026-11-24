# ADR-001: Choosing Rust + Oxc Over Node.js + TypeScript Compiler API

- **Status:** Accepted
- **Date:** 2026-11-24

## Context

Laine analyzes changes across multiple independent Git repositories locally and in CI pipelines. An earlier draft proposed building the engine in TypeScript on top of Node.js and the TypeScript Compiler API.

## Decision

Implement the core engine, graph, OpenAPI/route analyzers, and CLI in **Rust**, using **`oxc`** (`oxc_parser`, `oxc_ast`, `oxc_semantic`, `oxc_ast_visit`) for TypeScript and JavaScript analysis, and **`petgraph`** for the dependency graph.

## Rationale

1. **Single-binary distribution:** Users and CI pipelines run one native binary (`laine`) without managing Node.js versions, `pnpm`, or `node_modules`.
2. **Zero dependency on `node_modules` for snapshots:** The TypeScript Compiler API degrades heavily to `any` when analyzing historical Git commits that do not have `node_modules` installed. Laine reads Git objects directly (`git show <rev>:<path>`) and relies on OpenAPI / explicit backend routes + `oxc` AST & data-flow analysis, never running `npm install` or untrusted code.
3. **Speed and memory safety:** `oxc`'s arena-allocated AST parses TypeScript files in sub-millisecond time; Rust's algebraic `enum` types model domain states (`ContractChange`, `Confidence`, `GraphNode`) cleanly at compile time.
4. **Natural multi-language extensibility:** Adding future language extractors (Go, Python, Java via `tree-sitter`, or custom scanners like our Laravel route extractor) stays native in one binary with no FFI overhead.

## Trade-offs & Consequences

- `oxc_semantic` does not perform full Microsoft `TypeChecker` inference over complex generics or external `.d.ts` files.
- We intentionally scope consumer field tracking to explicit, verifiable AST data-flow patterns (direct property reads, destructuring, local aliases, `axios.create`, configured custom wrappers, and 1-level local helpers), and classify anything outside scope honestly as `UNKNOWN` / `POSSIBLE`.
- `oxc_*` crates evolve quickly; versions are pinned in workspace `Cargo.toml` and isolated inside `crates/laine-ts`.
