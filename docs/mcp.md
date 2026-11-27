# Laine MCP server

Run `laine mcp` to give an MCP client read-only access to Laine reports over stdio. The server reads one JSON-RPC message per line and writes responses to stdout. It sends transport diagnostics to stderr and exits when stdin closes.

## Client setup

Configure your client to launch the `laine` executable with the `mcp` subcommand. For clients that use an `mcpServers` configuration, an example entry is:

```json
{
  "mcpServers": {
    "laine": {
      "command": "laine",
      "args": ["mcp", "--config", "/path/to/laine.toml", "--no-cache", "--no-snippets"]
    }
  }
}
```

Use an absolute executable path if the client cannot find `laine` on its PATH. Set `--workspace <name>` to select a workspace from the Laine registry, or omit both flags to use workspace discovery. An explicit tool `config` or `workspace` overrides the server's selection for that call. When both are supplied, `config` takes precedence. For validation, the workspace selector is called `name`. Relative tool config paths are resolved from the supplied tool `cwd` or the server's current directory. Startup config paths are relative to the server's current directory.

## Tools

| Tool | Inputs | Structured result |
| --- | --- | --- |
| `laine_consumers` | Required `method` and `path`. Optional `service`, response `field`, and `producer_revision` (default `HEAD`). | The same versioned endpoint report as `laine consumers --format json`. |
| `laine_analyze` | Optional producer `repo`, `base` (default `HEAD`), `head` (default `WORKTREE`), and `staged` (sets head to `STAGED`). | The same versioned impact report as `laine analyze --format json` for equivalent revisions and options. |
| `laine_explain_policy` | Optional `endpoint` object with `method` and `path`. Pass `repo` to also evaluate a diff with optional `base` and `head`, using the analysis defaults. | Effective `fail_on`, `ignore_endpoints`, and `fail_on_unresolved`. Includes endpoint matches or the diff's policy decision, finding dispositions, unresolved calls, and coverage when requested. |
| `laine_workspace_validate` | Optional `config`, registered workspace `name`, or `all: true` for the entire registry. `all` is exclusive with `config` and `name`. Set `strict: true` to treat warnings as validation failures. | The same versioned validation report as `laine workspace validate --format json`. |

All tools accept `cwd` and `config`. The three analysis-related tools also accept `workspace`, `consumer_revision` (`head` by default, or `staged` or `worktree`), and `no_cache`. When `repo` or `service` is omitted for analysis or consumer lookup, Laine infers it from `cwd` as in `laine check`. If no tool directory is given, the server uses its process working directory. The supported HTTP methods are `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`, and `TRACE`.

For example, call `laine_consumers` with:

```json
{
  "service": "customer-service",
  "method": "GET",
  "path": "/customers/:id",
  "field": "fullName"
}
```

For policy explanations, `endpoint_check.matched_patterns` lists every matching ignore rule in configuration order. With a diff, `evaluation.finding_decisions` covers the findings retained by the engine after applying ignore rules.

Each tool returns a text summary in `content` and a machine-readable report in `structuredContent`. An analysis with `exit_code: 2` and `policy.should_fail: true` is a completed analysis and has `isError: false`. Validation findings also remain in a completed validation report, including when `valid` is false. Runtime failures that prevent a report have `isError: true` and use Laine's structured CLI diagnostics. Invalid tool names or input arguments produce JSON-RPC errors.

## Cache and source evidence

The server reads local Git snapshots and workspace configuration. By default, analysis and consumer lookup can read and populate the SQLite extraction cache. Use `laine mcp --no-cache` for strict zero-write operation across the session. It bypasses both cache reads and writes, even if a tool call passes `no_cache: false`. When starting without that flag, set `no_cache: true` to bypass the cache for one analysis, consumer lookup, or policy explanation.

Use `--no-snippets` to suppress source evidence snippets in every tool response. The analysis and consumer tools also support per-call `no_snippets: true` and `include_repo_roots: true`. By default, their reports omit the local checkout-root map.
