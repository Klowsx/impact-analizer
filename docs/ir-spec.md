# Laine Intermediate Representation (IR) & JSON Report Specification

- **Schema Version:** `1`
- **Status:** Stable (`v0.1.0`)
- **Authoritative Rust Definitions:** [`crates/laine-core/src/model.rs`](../crates/laine-core/src/model.rs), [`crates/laine-core/src/graph.rs`](../crates/laine-core/src/graph.rs), [`crates/laine-core/src/impact.rs`](../crates/laine-core/src/impact.rs)

---

## 1. Overview

Laine decouples language- and framework-specific static extractors from impact analysis and reporting through two formal layers:

1. **Multi-Language Intermediate Representation (IR) & Dependency Graph:** Normalized endpoints, contract changes, HTTP consumer calls, field reads, and unresolved dynamic calls ingested into a directed `petgraph::DiGraph`.
2. **Versioned JSON Report (`AnalysisReport`, `schema_version: 1`):** Byte-for-byte deterministic structured output consumed by CI gates, Pull Request bots, golden tests, and downstream tooling.

---

## 2. Multi-Language IR & Graph Model

All producer extractors (OpenAPI 3.0/3.1, Express, Laravel, FastAPI, Flask) and consumer extractors (TypeScript/JavaScript/Vue SFC, Python `requests`/`httpx`) normalize their findings into the same language-agnostic graph primitives before impact analysis runs.

### 2.1 Path Normalization & Canonical Route Keys

Every framework expresses path parameters differently. Laine normalizes paths in two stages:

1. **Normalized Display Path (`EndpointId.normalized_path`):**
   - Leading slash enforced, trailing slash trimmed (except root `/`).
   - All parameter syntaxes normalize to `:param`:
     - OpenAPI: `/customers/{id}` -> `/customers/:id`
     - Express: `/customers/:id` -> `/customers/:id`
     - Laravel: `/customers/{id?}` -> `/customers/:id`
     - FastAPI: `/customers/{id:int}` -> `/customers/:id`
     - Flask: `/customers/<int:id>` -> `/customers/:id`
     - Consumer templates (JS/TS `${id}`, Python f-strings `{id}`): `/customers/:id`
2. **Canonical Route Key (`canonical_route_key`):**
   - Replaces every parameter segment (`:id`, `{id}`) with a positional wildcard `*` (e.g., `/customers/:customerId` -> `/customers/*`).
   - Graph matching (`routes_match`) compares segment-by-segment where `*` matches either another `*` or a concrete literal (e.g., `/customers/1024`), ensuring parameter renames do not cause false breaking changes and polyglot producers/consumers align accurately.

### 2.2 Graph Nodes (`GraphNode`)

| Node Variant | Identifying Fields | Role |
|---|---|---|
| `Repository` | `name` | Logical service / Git repository declared under `[[services]]`. |
| `File` | `repo`, `path` | Source file inside a repository (POSIX-normalized relative path). |
| `Endpoint` | `service`, `method`, `canonical_path`, `display_path` | HTTP endpoint exposed by a producer or targeted by a consumer. |
| `SchemaField` | `service`, `method`, `canonical_path`, `field_path` | Flattened JSON field path (e.g., `fullName`, `address.city`, `items[].price`). |

### 2.3 Graph Edges (`GraphEdge`)

| Edge Variant | Direction | Payload | Meaning |
|---|---|---|---|
| `Contains` | `Repository -> File` | _(none)_ | File belongs to repository. |
| `Exposes` | `Repository -> Endpoint` | _(none)_ | Producer service exposes an HTTP endpoint. |
| `ReturnsField` | `Endpoint -> SchemaField` | _(none)_ | Endpoint returns a response schema field. |
| `Consumes` | `File -> Endpoint` | `revision`, `line`, `snippet` | Consumer file invokes an HTTP endpoint. |
| `ReadsField` | `File -> SchemaField` | `revision`, `line`, `snippet`, `call_file`, `call_line`, `confidence` | Consumer file reads or declares a typed contract on a response field. |

Field matching is prefix-aware: if a parent object field (`address`) is removed or changes type, consumer reads of nested children (`address.city`, `address.zip`) or array elements (`items[].name` when `items` changes) are matched automatically.

---

## 3. JSON Report Schema (`schema_version: 1`)

### 3.1 Top-Level Object: `AnalysisReport`

| Field | JSON Type | Required | Serialization Rules | Description |
|---|---|---|---|---|
| `schema_version` | `integer` (`u32`) | Yes | Always `1` for this specification | Major schema version for forward/backward compatibility. |
| `target_repo` | `string` | Yes | — | Name of the producer repository whose diff was analyzed. |
| `base_rev` | `string` | Yes | Resolved 40-char Git commit SHA | Base commit before the change. |
| `head_rev` | `string` | Yes | Resolved 40-char Git commit SHA | Head commit after the change. |
| `policy` | `object` | For CLI analysis | Omitted by reports produced without workspace policy | The policy decision for the reported findings and unresolved calls. |
| `stats` | [`AnalysisStats`](#32-analysisstats) | Yes | — | Deterministic structural counts for the run. |
| `findings` | `Array<`[`ImpactFinding`](#35-impactfinding)`>` | Yes | May be empty `[]` | Detected breaking contract changes and their direct/indirect consumers. |
| `unresolved_calls` | `Array<`[`UnresolvedCall`](#38-unresolvedcall)`>` | Yes | Sorted by `(repo, file, line, reason)` | Dynamic or unresolvable consumer HTTP calls (`UNKNOWN`). |
| `discrepancies` | `Array<`[`ContractDiscrepancy`](#39-contractdiscrepancy)`>` | No | Omitted when empty (`skip_serializing_if`) | Mismatches between OpenAPI spec and extracted backend code routes. |

> **Note on volatile metrics:** Wall-clock timings (such as `elapsed_ms`) are intentionally excluded from `AnalysisReport` JSON and rendered only in terminal output so repeated runs on identical commits remain byte-for-byte identical.

`policy` contains `should_fail`, `exit_code`, `blocking_findings`, `blocking_unresolved_calls`, and `reason`. `exit_code` is `0` when policy passes or `2` when it blocks, matching the CLI process. A report can include findings below the configured threshold or be blocked by unresolved calls with zero findings.

### 3.2 `AnalysisStats`

| Field | JSON Type | Description |
|---|---|---|
| `repositories` | `integer` (`usize`) | Total repositories configured and inspected. |
| `files_analyzed` | `integer` (`usize`) | Total source and contract files analyzed across repositories. |
| `graph_nodes` | `integer` (`usize`) | Total deduplicated nodes in the `DependencyGraph`. |
| `graph_edges` | `integer` (`usize`) | Total directed edges in the `DependencyGraph`. |

### 3.3 Core Enums

#### `HttpMethod` (`UPPERCASE`)
`"GET"` | `"POST"` | `"PUT"` | `"PATCH"` | `"DELETE"`

#### `Confidence` (`UPPERCASE`)
Ordered from weakest to strongest:
- `"UNKNOWN"` — Static target could not be resolved (e.g., runtime-computed URL or unmapped environment variable). Surfaced in `unresolved_calls`.
- `"POSSIBLE"` — Indirect transitive downstream repository depending on a directly affected consumer (`indirect_consumers`).
- `"LIKELY"` — Strong structural evidence inferred via a typed contract annotation (e.g., TypeScript generic `fetch<CustomerResponse>()` where the interface declares the field, even without an inline property access at the call site).
- `"CONFIRMED"` — Direct, verifiable static evidence in both the producer contract/route and consumer AST/scanner (e.g., direct endpoint call or concrete field read like `customer.fullName`, `customer["fullName"]`, destructuring, or 1-level helper propagation).

#### `Severity` (`UPPERCASE`)
Ordered from lowest to highest:
- `"LOW"` — Reserved for informational / low-blast-radius findings.
- `"MEDIUM"` — Breaking producer contract change with **no** statically detected direct consumers in the workspace.
- `"HIGH"` — Breaking response field, request field, or required parameter change with **one or more** direct consumers.
- `"CRITICAL"` — Entire endpoint removed (`endpoint_removed`) with **one or more** direct consumers.

### 3.4 `EndpointId` & `Evidence`

#### `EndpointId`
| Field | JSON Type | Description |
|---|---|---|
| `service` | `string` | Producer service name from `laine.toml`. |
| `method` | [`HttpMethod`](#httpmethod-uppercase) | Uppercase HTTP verb. |
| `normalized_path` | `string` | Normalized path using `:param` placeholders (e.g., `"/customers/:id"`). |

#### `Evidence`
| Field | JSON Type | Description |
|---|---|---|
| `repo` | `string` | Repository name containing the evidence. |
| `revision` | `string` | Resolved 40-char Git commit SHA from which the file blob was read. |
| `file` | `string` | Repository-relative POSIX path (e.g., `"src/invoices/customer.ts"`). |
| `line` | `integer` (`usize`) | 1-based line number in the source file. |
| `snippet` | `string` | Trimmed source line or concise contract description at `line`. |

### 3.5 `ImpactFinding`

| Field | JSON Type | Required | Serialization Rules | Description |
|---|---|---|---|---|
| `id` | `string` | Yes | Format `"LAINE-001"`, `"LAINE-002"`, ... | Deterministic sequential finding identifier within the report. |
| `change` | [`ContractChange`](#36-contractchange) | Yes | Internally tagged by `"kind"` | Structured description of the breaking producer change. |
| `severity` | [`Severity`](#severity-uppercase) | Yes | — | Blast-radius classification. |
| `confidence` | [`Confidence`](#confidence-uppercase) | Yes | — | Certainty level of the producer change (`"CONFIRMED"`). |
| `producer_evidence` | `ProducerEvidence` | Yes | — | Source in base for removals and response type changes. Source in head for request changes. |
| `producer_evidence_comparison` | `ProducerEvidence` | No | Present for type changes | Source for the other revision of the type change. |
| `consumers` | `Array<`[`AffectedConsumer`](#37-affectedconsumer--indirectconsumer)`>` | Yes | Sorted deterministically | Direct consumer sites affected by this change. |
| `indirect_consumers` | `Array<`[`IndirectConsumer`](#37-affectedconsumer--indirectconsumer)`>` | No | Omitted when empty (`skip_serializing_if`) | Transitive downstream repositories discovered via cycle-safe BFS. |

`ProducerEvidence` includes `repo`, `revision`, `file` and `description`. OpenAPI evidence includes a `json_pointer` to the declaration, which may be inside a referenced component. When the source has a verified location, `line` is one-based and `span` records its start and exclusive end with one-based lines and columns. Code routes provide a line when the extractor supplies it. `snippet` is reserved for literal source and is empty when omitted.

### 3.6 `ContractChange`

Internally tagged enum (`#[serde(tag = "kind", rename_all = "snake_case")]`). Backwards-compatible additive changes (new endpoints, new response fields, new optional request fields, new optional parameters) are intentionally excluded.

| `kind` | Additional Fields | Producer Evidence Line Source | Matched Consumers |
|---|---|---|---|
| `"endpoint_removed"` | `endpoint: EndpointId` | Base spec / route file | All consumers with `Consumes` edge to `endpoint`. |
| `"response_field_removed"` | `endpoint: EndpointId`, `status_code: string`, `field_path: string`, `previous_type: string` | Base spec | Consumers with `ReadsField` edge matching `field_path` (or nested child). |
| `"response_field_type_changed"` | `endpoint: EndpointId`, `status_code: string`, `field_path: string`, `previous_type: string`, `new_type: string` | Base spec | Consumers with `ReadsField` edge matching `field_path` (or nested child). |
| `"request_field_added_required"` | `endpoint: EndpointId`, `field_path: string`, `field_type: string` | Head spec (fallback: base) | All consumers with `Consumes` edge to `endpoint`. |
| `"request_field_type_changed"` | `endpoint: EndpointId`, `field_path: string`, `previous_type: string`, `new_type: string` | Head spec (fallback: base) | All consumers with `Consumes` edge to `endpoint`. |
| `"required_parameter_added"` | `endpoint: EndpointId`, `location: string`, `name: string`, `param_type: string` | Head spec (fallback: base) | All consumers with `Consumes` edge to `endpoint`. |

### 3.7 `AffectedConsumer` & `IndirectConsumer`

#### `AffectedConsumer`
| Field | JSON Type | Description |
|---|---|---|
| `repo` | `string` | Consuming repository name. |
| `evidence` | [`Evidence`](#34-endpointid--evidence) | Exact file, commit SHA, 1-based line, and snippet of the call or field read. |
| `confidence` | [`Confidence`](#confidence-uppercase) | `"CONFIRMED"` for direct calls/reads, `"LIKELY"` for typed generic contracts. |
| `dependency_path` | `Array<string>` | Ordered trace from endpoint through HTTP call site to field read / typed contract. |

#### `IndirectConsumer`
| Field | JSON Type | Description |
|---|---|---|
| `repo` | `string` | Transitive downstream repository name (excluding target producer and direct consumers). |
| `confidence` | [`Confidence`](#confidence-uppercase) | Always `"POSSIBLE"`. |
| `via_repo` | `string` | Direct consumer repository through which this downstream dependency flows. |
| `dependency_chain` | `Array<string>` | Cycle-safe repository hop chain starting from `via_repo` to `repo`. |

### 3.8 `UnresolvedCall`

| Field | JSON Type | Description |
|---|---|---|
| `repo` | `string` | Repository containing the unresolved call. |
| `evidence` | [`Evidence`](#34-endpointid--evidence) | Exact commit SHA, file, line, and snippet of the dynamic HTTP call. |
| `reason` | `string` | Human-readable explanation (e.g., dynamic variable URL, unmapped `[env]` variable). |

### 3.9 `ContractDiscrepancy`

Emitted when a producer configures both `openapi` and `routes` in `laine.toml`:

| Field | JSON Type | Description |
|---|---|---|
| `endpoint` | [`EndpointId`](#34-endpointid--evidence) | The mismatched endpoint. |
| `kind` | `"undocumented_in_openapi"` \| `"missing_in_code"` | `undocumented_in_openapi`: route exists in code but is absent from OpenAPI. `missing_in_code`: endpoint is declared in OpenAPI but not found in extracted route code. |
| `detail` | `string` | Human-readable explanation including code file and line when applicable. |

---

## 4. Confidence vs. Severity Matrix

Laine strictly separates **how sure we are** (`Confidence`) from **how severe the blast radius is** (`Severity`):

| Producer Change | Direct Consumers Found | Finding `severity` | Consumer `confidence` |
|---|---|---|---|
| `endpoint_removed` | `>= 1` | `CRITICAL` | `CONFIRMED` |
| `endpoint_removed` | `0` | `MEDIUM` | _(none)_ |
| `response_field_removed` / `response_field_type_changed` | `>= 1` (AST/scanner field read) | `HIGH` | `CONFIRMED` |
| `response_field_removed` / `response_field_type_changed` | `>= 1` (TS generic type contract only) | `HIGH` | `LIKELY` |
| `response_field_removed` / `response_field_type_changed` | `0` | `MEDIUM` | _(none)_ |
| `request_field_added_required` / `request_field_type_changed` / `required_parameter_added` | `>= 1` (endpoint callers) | `HIGH` | `CONFIRMED` |
| `request_field_added_required` / `request_field_type_changed` / `required_parameter_added` | `0` | `MEDIUM` | _(none)_ |
| Transitive downstream repo (consumer of a direct consumer) | `>= 1` | Inherited from finding | `POSSIBLE` (in `indirect_consumers`) |
| Unresolvable dynamic HTTP call | N/A | N/A (policy-configurable) | `UNKNOWN` (in `unresolved_calls`) |

---

## 5. Complete `schema_version: 1` JSON Example

```json
{
  "schema_version": 1,
  "target_repo": "customer-service",
  "base_rev": "1a2b3c4d5e6f78901234567890abcdef12345678",
  "head_rev": "9f8e7d6c5b4a3210fedcba0987654321fedcba09",
  "stats": {
    "repositories": 4,
    "files_analyzed": 12,
    "graph_nodes": 26,
    "graph_edges": 31
  },
  "findings": [
    {
      "id": "LAINE-001",
      "change": {
        "kind": "response_field_removed",
        "endpoint": {
          "service": "customer-service",
          "method": "GET",
          "normalized_path": "/customers/:id"
        },
        "status_code": "200",
        "field_path": "fullName",
        "previous_type": "string"
      },
      "severity": "HIGH",
      "confidence": "CONFIRMED",
      "producer_evidence": {
        "repo": "customer-service",
        "revision": "1a2b3c4d5e6f78901234567890abcdef12345678",
        "file": "openapi.yaml",
        "line": 24,
        "json_pointer": "/components/schemas/Customer/properties/fullName",
        "description": "Response field fullName (string)",
        "snippet": ""
      },
      "consumers": [
        {
          "repo": "billing-service",
          "evidence": {
            "repo": "billing-service",
            "revision": "abcdef1234567890abcdef1234567890abcdef12",
            "file": "src/invoices/customer.ts",
            "line": 8,
            "snippet": "return `Invoice for ${customer.fullName}`;"
          },
          "confidence": "CONFIRMED",
          "dependency_path": [
            "GET /customers/:id",
            "HTTP call @ billing-service/src/invoices/customer.ts:6",
            "read `fullName` @ billing-service/src/invoices/customer.ts:8"
          ]
        }
      ],
      "indirect_consumers": [
        {
          "repo": "finance-portal",
          "confidence": "POSSIBLE",
          "via_repo": "billing-service",
          "dependency_chain": [
            "billing-service",
            "finance-portal"
          ]
        }
      ]
    },
    {
      "id": "LAINE-002",
      "change": {
        "kind": "required_parameter_added",
        "endpoint": {
          "service": "customer-service",
          "method": "POST",
          "normalized_path": "/customers"
        },
        "location": "header",
        "name": "X-Tenant-Id",
        "param_type": "string"
      },
      "severity": "HIGH",
      "confidence": "CONFIRMED",
      "producer_evidence": {
        "repo": "customer-service",
        "revision": "9f8e7d6c5b4a3210fedcba0987654321fedcba09",
        "file": "openapi.yaml",
        "line": 42,
        "description": "Required header parameter X-Tenant-Id (string)",
        "snippet": ""
      },
      "consumers": [
        {
          "repo": "python-sync-worker",
          "evidence": {
            "repo": "python-sync-worker",
            "revision": "11223344556677889900aabbccddeeff11223344",
            "file": "workers/sync.py",
            "line": 19,
            "snippet": "res = httpx.post(f\"{BASE_URL}/customers\", json=payload)"
          },
          "confidence": "CONFIRMED",
          "dependency_path": [
            "POST /customers",
            "HTTP call @ python-sync-worker/workers/sync.py:19"
          ]
        }
      ]
    }
  ],
  "unresolved_calls": [
    {
      "repo": "billing-service",
      "evidence": {
        "repo": "billing-service",
        "revision": "abcdef1234567890abcdef1234567890abcdef12",
        "file": "src/legacy/sync.ts",
        "line": 5,
        "snippet": "return fetch(dynamicEndpoint);"
      },
      "reason": "dynamic URL expression cannot be resolved statically"
    }
  ],
  "discrepancies": [
    {
      "endpoint": {
        "service": "customer-service",
        "method": "POST",
        "normalized_path": "/internal/reindex"
      },
      "kind": "undocumented_in_openapi",
      "detail": "Route exposed in src/routes/internal.ts:14 is missing from the OpenAPI contract"
    }
  ]
}
```

---

## 6. Exit Codes & Policy Evaluation

`laine analyze` uses explicit process exit codes so CI pipelines can distinguish clean builds, breaking contract violations, and runtime/configuration failures:

| Exit Code | Meaning | When Returned |
|---|---|---|
| `0` | **Compatible / Pass** | No breaking changes were detected, or all findings fell below the configured `[policy]` blocking thresholds. |
| `2` | **Breaking Change / Policy Failure** | One or more findings triggered the active `[policy]` blocking rules (default: `fail_on = "low"`, meaning any breaking contract finding fails the check). |
| `1` | **Configuration or Runtime Error** | Invalid `laine.toml`, missing service or Git revision, malformed OpenAPI spec, unreadable repository, or I/O error. |

Policy behavior is controlled via the optional `[policy]` table in `laine.toml`:
- `fail_on` (`"low"` | `"medium"` | `"high"` | `"critical"`, default `"low"`): Minimum finding `Severity` that triggers exit code `2`.
- `ignore_endpoints` (`Array<string>`, default `[]`): Exact paths, single-segment wildcards (`"/internal/*"`), recursive prefix globs (`"/internal/**"`), or method-qualified patterns (`"GET /health"`) filtered out before policy evaluation.
- `fail_on_unresolved` (`boolean`, default `false`): When `true`, any `UnresolvedCall` (`UNKNOWN`) also triggers exit code `2`.

---

## 7. Determinism Guarantees

Laine enforces strict reproducibility for identical Git commits, `laine.toml`, and binary version:

1. **Ordered Collections:** OpenAPI endpoints (`BTreeMap<EndpointId, EndpointContract>`), response/request schemas, parameters, code route maps, indirect consumer maps, and deduplicated consumer hits (`BTreeMap`) eliminate `HashMap` iteration randomness.
2. **Explicit Stable Sorting:** `consumers_of_endpoint`, `transitive_dependent_paths`, and `unresolved_calls` are explicitly sorted by `(repo, file, line, ...)` prior to report assembly.
3. **Immutable Git Snapshots:** Files are read directly from Git objects (`git show` / `git cat-file --batch`) using resolved 40-char commit SHAs without touching working directories or reading uncommitted state.
4. **No Volatile Fields:** Timestamps, host-specific absolute paths, and wall-clock durations are excluded from JSON serialization.
