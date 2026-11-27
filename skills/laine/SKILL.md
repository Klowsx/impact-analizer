---
name: laine
description: Use Laine for cross-repository API impact checks, finding endpoint consumers before editing backend routes or OpenAPI specs, validating Laine workspaces, and interpreting Laine JSON reports.
---

# Laine agent playbook

Use Laine before changing or deleting a backend route or OpenAPI field, before committing or pushing producer changes, and when verifying work across repositories.

## Step 1 — Validate workspace readiness

List registered workspaces and validate the current or selected workspace without prompting:

```sh
laine workspace list
laine workspace validate --format json
laine workspace validate <name> --format json
laine workspace validate --config <path> --format json
laine workspace validate --all --format json
laine workspace validate --all --strict --format json
```

`workspace validate` also accepts `--strict`, `--all`, and `--config <path>`. Review `workspaces[].issues[]` on validation failures. Never invoke interactive pickers without a TTY. For `laine init --scan` and `laine workspace edit`, always pass `--select <names>` or `--all` to make the selection non-interactive.

## Step 2 — Look up blast radius before editing

This endpoint lookup does not require a producer Git diff:

```sh
laine consumers --service <svc> --method <METHOD> --path <PATH> --format json
```

Optional flags are `--field <field_path>`, `--producer-revision <rev>`, `--consumer-revision head|staged|worktree`, `--include-repo-roots`, and `--no-snippets`. Also available are `--config <path>`, `--workspace <name>`, `--no-cache`, and `--format terminal|json|markdown`. Check `in_catalog` to see whether the endpoint is declared in the producer catalog. Review `consumers`, `indirect_consumers`, `fields_read`, and `unresolved_calls` where present.

## Step 3 — Analyze edits before committing

`check` compares `HEAD..WORKTREE` by default. Add `--staged` to check the staged producer diff; set `--consumer-revision staged` or `worktree` to include local consumer edits:

```sh
laine check --format json
laine check --staged --consumer-revision staged --format json
```

For explicit revisions and a named producer, use:

```sh
laine analyze --config <path> --repo <service> --base <rev> --head <rev> --format json
```

## Exit codes

- `0`: the lookup succeeded or the analysis passed, including a successful lookup with zero consumers.
- `2`: `analyze` or `check` found a policy-blocked breaking change. Inspect `findings`, `policy.reason`, and `unresolved_calls`.
- `1`: configuration, Git, OpenAPI, validation, or runtime error. Inspect `diagnostics[].code` and `diagnostics[].remediation`; for `workspace validate`, inspect `workspaces[].issues[]`.
- `64`: invalid CLI arguments. With `--format json`, Laine emits a `CliErrorReport` on stdout.

## Certainty, coverage, and limits

Confidence describes how strongly Laine matched evidence: `CONFIRMED` is a direct, verified match; `LIKELY` is strong but not conclusive; `POSSIBLE` can include ambiguous routes or unverified request arguments; and `UNKNOWN` means the call could not be resolved. Severity (`CRITICAL`, `HIGH`, `MEDIUM`, `LOW`) describes the impact of a change and is separate from confidence.

Never claim zero impact when `coverage.complete == false` or `unresolved_calls` is non-empty. State that coverage is incomplete and cite the relevant `diagnostics[]` and/or `unresolved_calls[]`. Verify each cited consumer file and line before editing. Treat `POSSIBLE` results, including ambiguous routes and unverified request arguments, as requiring code inspection.

## Privacy and portability

Use `--no-snippets` in shared pull request comments and CI logs to avoid exposing code context. Use `--include-repo-roots` only locally when you need absolute checkout paths.

## MCP clients

Run `laine mcp --no-cache --no-snippets` as a stdio server to expose workspace validation, endpoint consumers, impact analysis, and policy explanations as read-only tools. Inspect `structuredContent.exit_code` and `policy` for analysis results, including a policy block with `isError: false`. See the [MCP setup and tool reference](../../docs/mcp.md).

## References

- [IR v1 JSON Schema](../../schemas/ir-v1.schema.json)
- [IR and report specification](../../docs/ir-spec.md)
