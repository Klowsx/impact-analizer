# ADR-002: Deterministic Intermediate Representation and Certainty vs. Severity Model

- **Status:** Accepted
- **Date:** 2026-11-24

## Context

Static cross-repository analysis tools often fail in production because of two issues:
1. Non-deterministic outputs caused by unordered hash maps or timestamps inside structured reports (breaking golden tests and CI diffing).
2. Alert fatigue caused by treating heuristic guesses identically to verified incompatibilities.

## Decision

1. **Separate `Confidence` from `Severity`:**
   - `Confidence` (`CONFIRMED`, `LIKELY`, `POSSIBLE`, `UNKNOWN`) expresses how strong the static evidence is.
   - `Severity` (`LOW`, `MEDIUM`, `HIGH`, `CRITICAL`) expresses potential blast radius.
   - Dynamic/unresolvable URLs are never matched by substring heuristics; they are surfaced explicitly as `UnresolvedCall` (`UNKNOWN`).
2. **Enforce deterministic output:**
   - All contract maps and report collections use `BTreeMap` or explicit stable sorting before serialization.
   - Volatile execution metrics (such as `elapsed_ms`) are printed only in terminal summaries and excluded from `AnalysisReport` JSON so repeated runs over the same commits produce byte-for-byte identical JSON.
3. **Decouple analyzers from the impact engine via a normalized IR:**
   - `laine-core` owns `EndpointId`, `ContractChange`, `ExtractedRoute`, `DependencyGraph`, and `AnalysisReport`.
   - Language-specific extractors (`laine-ts`, `laine-core::laravel`, future `tree-sitter` crates) feed into the same graph without coupling the impact engine to any single language or framework.

## Consequences

- Downstream consumers can rely on stable JSON snapshots (`schema_version: 1`) for golden tests and CI gates (exit code `0` compatible, `2` incompatible change, `1` error).
- Transitive downstream repositories (consumers of consumers) are clearly labeled `POSSIBLE` rather than falsely claiming `CONFIRMED` breakage.
