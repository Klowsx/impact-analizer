# Evaluation corpus and benchmarks

## Multi-framework evaluation corpus

Run the corpus with Git available on `PATH`.

```sh
cargo test -p laine-cli --test corpus_eval -- --nocapture
```

The test generates Git repositories and a local SQLite cache in a temporary directory for each scenario. The fixtures use fixed commit identities and dates. Each evaluated snapshot must produce byte-identical JSON with caching disabled, while populating the cache, and with a warm cache.

| Scenario | Ground truth |
| --- | --- |
| OpenAPI 3.0 response evolution | An external `$ref` leads to a `oneOf` schema. TypeScript `fetch`, a barrel-exported Axios client, a Vue template, and Python `requests` and `httpx.Client` read affected fields. The test checks a field removal, a type change, and a required field becoming conditional. It also verifies unchanged and additive fields and two unresolved URLs. |
| OpenAPI 3.1 request arguments | TypeScript `fetch` and Axios plus Python `requests` and HTTPX exercise a nullable type change, a required body field, required query and header arguments, and a query type change. Missing and incompatible arguments are `CONFIRMED`, compatible calls pass, and a dynamic body field yields `POSSIBLE`. |
| TypeScript cross-file contracts | A service result reaches a JSX prop through aliased imports and re-exports. Zod validation and an Axios generic interface produce `LIKELY` attributions. A seven-hop chain emits `DataflowLimitReached`. |
| Express routes and drift | A custom TypeScript client and Python HTTPX call a removed route mounted under `/v1`. The surviving route is unaffected. The head snapshot reports both an undocumented code route and an OpenAPI-only operation. |
| FastAPI router mounts | Python `requests.Session` and TypeScript `fetch` call routes contributed by `APIRouter` and `include_router`. The removed route is attributed and the surviving route is unaffected. |
| Flask blueprints | A `register_blueprint` URL prefix overrides the blueprint default. Python `httpx.AsyncClient` and TypeScript Axios exercise both the removed and surviving routes. |
| Laravel routes | Vue `$fetch` and TypeScript `fetch` consume grouped PHP routes. A clean removal is attributed. A separate dynamic route snapshot reports `RouteDeclarationUnresolved` and emits zero removals. |
| Transitive impact, policy, and references | A producer change reaches `service-c` through `service-b` at `POSSIBLE` confidence. `ignore_endpoints` suppresses a known `/internal/debug` finding. A broken external reference produces `OpenApiReferenceUnresolved`. |

The version 1 JSON summary counts direct `CONFIRMED` and `LIKELY` attributions by change, repository, file, line, and confidence. It reports precision, recall, and F1, plus exact matches for `POSSIBLE`, unresolved (`UNKNOWN`) calls, diagnostics, and discrepancies. Each true negative is verified against a consumer lookup on the base catalog. Framework totals show both producer stacks and consumer languages, so a single attribution can appear in both breakdowns.

## Run the benchmark

```sh
cargo run --release -p laine-cli --example bench_harness -- --scale medium --iterations 5 --markdown
cargo run --release -p laine-cli --example bench_harness -- --scale medium --iterations 5 --json
```

Use `--scale small` for 4 repositories and 40 tracked files, `--scale medium` for 4 repositories and 200 files, or `--scale large` for 6 repositories and 500 files. The defaults are small, three iterations, and Markdown. Each iteration generates a fresh OpenAPI producer and mixed TypeScript, React, Vue, and Python consumers. The TypeScript consumers share an Axios client, a barrel, and a service with Zod and a generic DTO.

| Phase | Timed operation |
| --- | --- |
| `cold_no_cache` | Analyze with the SQLite cache disabled. |
| `cold_populate_cache` | Analyze with an empty SQLite cache and populate it. |
| `warm_full_cache_hit` | Repeat the analysis after the cache has been populated. |
| `incremental_1file_leaf_commit` | Analyze after committing a comment in one leaf React component. |
| `incremental_1file_shared_service` | Analyze after a second commit adds a comment to one shared service module. |
| `index_cold` | Index every repository after deleting the iteration's analysis cache. |
| `index_warm` | Repeat indexing with the populated cache. |

The timer covers the engine operation, including its Git and SQLite work. Fixture generation, edits, commits, and equivalence checks run outside the timer. The benchmark verifies exact JSON equality against an uncached run at each selected revision. It also checks that the comment edits preserve the original report after accounting for the new consumer commit IDs and confirms expected index hit and miss counts.

**Latency and memory.** The table gives the median and observed range of elapsed times for each phase. Speedup uses uncached analysis as the analysis baseline and cold indexing as the index baseline. Here “cold” describes Laine's cache state. The OS filesystem cache remains active. `files_analyzed` is the analysis report's logical file count, including files served by cache. The indexing API reports repository counts.

The benchmark wraps Rust's system allocator in `TrackingAllocator`. Atomic counters track live requested heap bytes and their high-water mark. Each phase resets the high-water mark to the current live count, so the result includes allocations retained across phases and excludes allocations made solely by native libraries, child Git processes, and the allocator's own bookkeeping. The reported value is the highest phase peak across the iterations. On Linux, `/proc/self/status` adds `VmHWM`, the benchmark process's cumulative peak resident memory in MB. It includes native and Rust memory and can reflect earlier phases. Other platforms leave that field empty. MB means 1,000,000 bytes.

## Local baseline

Windows 11 Pro x86-64 (build 22631), Intel Core i7-13700T, 16 GB RAM, Rust 1.100.0, Git 2.47.1.windows.1. Release build on the `0ba2818` engine baseline, medium scale, three iterations.

```sh
cargo run --release -p laine-cli --example bench_harness -- --scale medium --iterations 3 --markdown
```

| Phase | Median ms | Min ms | Max ms | Peak Rust heap MB | Speedup |
| --- | ---: | ---: | ---: | ---: | ---: |
| `cold_no_cache` | 909.34 | 674.30 | 932.38 | 0.758 | 1.00x |
| `cold_populate_cache` | 1027.48 | 733.44 | 1067.69 | 0.639 | 0.89x |
| `warm_full_cache_hit` | 550.30 | 358.13 | 581.97 | 0.410 | 1.65x |
| `incremental_1file_leaf_commit` | 584.89 | 484.13 | 646.54 | 0.567 | 1.55x |
| `incremental_1file_shared_service` | 611.07 | 539.42 | 652.33 | 0.580 | 1.49x |
| `index_cold` | 637.32 | 624.97 | 913.65 | 0.588 | 1.00x |
| `index_warm` | 143.62 | 129.38 | 185.58 | 0.209 | 4.44x |
