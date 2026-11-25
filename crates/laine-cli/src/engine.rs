use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use laine_core::cache::{IndexCache, compute_config_hash};
use laine_core::config::{RouteFramework, RouteSourceConfig, WorkspaceConfig};
use laine_core::error::{LaineError, Result};
use laine_core::git::GitRepo;
use laine_core::graph::DependencyGraph;
use laine_core::impact::{ImpactInput, analyze_impact};
use laine_core::laravel::extract_laravel_routes;
use laine_core::model::{
    AnalysisReport, AnalysisStats, Confidence, ContractChange, ContractDiscrepancy, Evidence,
    HttpMethod, UnresolvedCall,
};
use laine_core::openapi::{canonical_route_key, diff_contracts, extract_contract};
use laine_core::policy::filter_ignored_findings;
use laine_core::routes::{ExtractedRoute, detect_openapi_discrepancies, diff_routes};
use laine_py::{
    PyExtractContext, extract_fastapi_routes, extract_flask_routes, extract_python_file,
};
use laine_ts::{
    ExtractContext, ModuleResolver, TypeContractDef, analyze_cross_file_dataflow,
    extract_express_routes, extract_file_with_context, extract_type_definitions,
    extract_zod_schemas, resolve_type_fields,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct AnalyzeRequest {
    pub config_path: PathBuf,
    pub target_repo: String,
    pub base_rev: String,
    pub head_rev: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSummary {
    pub repos_indexed: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub cache_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CachedConsumeEdge {
    file: String,
    target_service: String,
    method: HttpMethod,
    normalized_path: String,
    line: usize,
    snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CachedFieldReadEdge {
    file: String,
    target_service: String,
    method: HttpMethod,
    normalized_path: String,
    field_path: String,
    line: usize,
    snippet: String,
    call_file: String,
    call_line: usize,
    confidence: Confidence,
    #[serde(default)]
    custom_trace: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CachedUnresolvedCall {
    file: String,
    line: usize,
    snippet: String,
    reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct CachedConsumerRepo {
    files_analyzed: usize,
    consumes: Vec<CachedConsumeEdge>,
    field_reads: Vec<CachedFieldReadEdge>,
    unresolved_calls: Vec<CachedUnresolvedCall>,
}

/// Runs a full cross-repository impact analysis without mutating any repository's working tree,
/// using the SQLite incremental extraction cache by default.
pub fn run_analysis(req: &AnalyzeRequest) -> Result<AnalysisReport> {
    run_analysis_with_options(req, false)
}

/// Runs a full cross-repository impact analysis without mutating any repository's working tree.
///
/// Supports producers configured with:
/// - `openapi` specification (OpenAPI 3.0/3.1 YAML or JSON)
/// - `routes` code extraction (Express, Laravel, FastAPI, or Flask) without requiring OpenAPI
/// - Both `openapi` and `routes` (also reporting discrepancies between spec and code)
///
/// When `no_cache` is `false`, per-repository consumer AST extractions are served from and
/// persisted to the SQLite incremental cache keyed by `(repo, commit_sha, config_hash, schema_version)`.
pub fn run_analysis_with_options(req: &AnalyzeRequest, no_cache: bool) -> Result<AnalysisReport> {
    let config = WorkspaceConfig::load_from_path(&req.config_path)?;
    let config_dir = config_directory(&req.config_path);

    let target_service = config.find_service(&req.target_repo).ok_or_else(|| {
        LaineError::ConfigInvalid(format!(
            "service '{}' not found in {}",
            req.target_repo,
            req.config_path.display()
        ))
    })?;

    if target_service.openapi.is_none() && target_service.routes.is_none() {
        return Err(LaineError::ConfigInvalid(format!(
            "service '{}' must configure `openapi` and/or `routes` in {}",
            req.target_repo,
            req.config_path.display()
        )));
    }

    let producer_path = resolve_repo_path(config_dir, &target_service.path);
    let producer_git = GitRepo::new(&producer_path);
    let base_sha = producer_git.resolve_revision(&req.base_rev)?;
    let head_sha = producer_git.resolve_revision(&req.head_rev)?;

    let mut graph = DependencyGraph::new();
    graph.add_repository(&target_service.name);

    let mut files_analyzed = 0usize;
    let mut contract_changes: Vec<ContractChange> = Vec::new();
    let mut discrepancies: Vec<ContractDiscrepancy> = Vec::new();
    let mut route_locations: BTreeMap<(String, HttpMethod, String), (String, usize)> =
        BTreeMap::new();
    let mut source_file_display = String::new();
    let mut base_source_raw = String::new();
    let mut head_source_raw = String::new();

    // 1. Optional OpenAPI producer contract analysis
    let mut head_openapi_contract = None;
    if let Some(openapi_rel) = target_service.openapi.as_deref() {
        let base_raw = producer_git.read_file(&base_sha, openapi_rel)?;
        let head_raw = producer_git.read_file(&head_sha, openapi_rel)?;
        files_analyzed += 1;

        let base_contract = extract_contract(&target_service.name, &base_raw)?;
        let head_contract = extract_contract(&target_service.name, &head_raw)?;
        contract_changes.extend(diff_contracts(&base_contract, &head_contract));

        for (endpoint_id, endpoint) in &base_contract.endpoints {
            graph.add_endpoint(
                &endpoint_id.service,
                endpoint_id.method,
                &endpoint_id.normalized_path,
            );
            for fields in endpoint.responses.values() {
                for field_path in fields.keys() {
                    graph.add_schema_field(
                        &endpoint_id.service,
                        endpoint_id.method,
                        &endpoint_id.normalized_path,
                        field_path,
                    );
                }
            }
        }

        source_file_display = openapi_rel.to_string_lossy().replace('\\', "/");
        base_source_raw = base_raw;
        head_source_raw = head_raw;
        head_openapi_contract = Some(head_contract);
    }

    // 2. Optional code-based producer route extraction (Express / Laravel / FastAPI / Flask)
    if let Some(route_cfg) = target_service.routes.as_ref() {
        let (base_routes, base_files_count, sample_file, sample_raw) =
            extract_producer_routes(&producer_git, &target_service.name, &base_sha, route_cfg)?;
        let (head_routes, _, _, _) =
            extract_producer_routes(&producer_git, &target_service.name, &head_sha, route_cfg)?;
        files_analyzed += base_files_count;

        for route in &base_routes {
            graph.add_endpoint(
                &route.endpoint.service,
                route.endpoint.method,
                &route.endpoint.normalized_path,
            );
            route_locations
                .entry((
                    route.endpoint.service.clone(),
                    route.endpoint.method,
                    canonical_route_key(&route.endpoint.normalized_path),
                ))
                .or_insert_with(|| (route.file.clone(), route.line));
        }

        let route_changes = diff_routes(&base_routes, &head_routes);
        let existing_removed: HashSet<_> = contract_changes
            .iter()
            .filter_map(|c| match c {
                ContractChange::EndpointRemoved { endpoint } => Some((
                    endpoint.service.clone(),
                    endpoint.method,
                    canonical_route_key(&endpoint.normalized_path),
                )),
                _ => None,
            })
            .collect();

        for change in route_changes {
            if let ContractChange::EndpointRemoved { ref endpoint } = change {
                let key = (
                    endpoint.service.clone(),
                    endpoint.method,
                    canonical_route_key(&endpoint.normalized_path),
                );
                if !existing_removed.contains(&key) {
                    contract_changes.push(change);
                }
            }
        }

        if let Some(head_openapi) = head_openapi_contract.as_ref() {
            discrepancies = detect_openapi_discrepancies(head_openapi, &head_routes);
        }

        if source_file_display.is_empty() {
            source_file_display = sample_file;
            base_source_raw = sample_raw;
        }
    }

    // 3. Consumer repository analysis (TypeScript / Vue / JavaScript / Python) with Rayon + SQLite cache
    let config_hash = compute_config_hash(&config);
    let cache = if no_cache {
        None
    } else {
        let cache_path = resolve_cache_path(config_dir, config.cache_path.as_deref());
        Some(IndexCache::open(&cache_path)?)
    };

    let mut unresolved_calls = Vec::new();
    for service in &config.services {
        if service.name == target_service.name {
            continue;
        }

        graph.add_repository(&service.name);
        let consumer_path = resolve_repo_path(config_dir, &service.path);
        let consumer_git = GitRepo::new(&consumer_path);
        let consumer_rev = consumer_git.resolve_revision("HEAD")?;

        let cached_snapshot = if let Some(cache_db) = cache.as_ref() {
            match cache_db.get_repo_snapshot(&service.name, &consumer_rev, &config_hash)? {
                Some(raw_json) => serde_json::from_str::<CachedConsumerRepo>(&raw_json).ok(),
                None => None,
            }
        } else {
            None
        };

        let snapshot = match cached_snapshot {
            Some(hit) => hit,
            None => {
                let extracted = extract_consumer_repo(&consumer_git, &consumer_rev, &config)?;
                if let Some(cache_db) = cache.as_ref() {
                    let payload = serde_json::to_string(&extracted)
                        .map_err(|err| LaineError::Cache(err.to_string()))?;
                    cache_db.put_repo_snapshot(
                        &service.name,
                        &consumer_rev,
                        &config_hash,
                        &payload,
                    )?;
                }
                extracted
            }
        };

        apply_cached_consumer_repo(
            &service.name,
            &consumer_rev,
            &snapshot,
            &mut graph,
            &mut unresolved_calls,
            &mut files_analyzed,
        );
    }

    let stats = AnalysisStats {
        repositories: config.services.len(),
        files_analyzed,
        graph_nodes: graph.node_count(),
        graph_edges: graph.edge_count(),
    };

    let mut report = analyze_impact(
        &graph,
        ImpactInput {
            target_repo: &target_service.name,
            base_rev: &base_sha,
            head_rev: &head_sha,
            source_file: &source_file_display,
            base_source_raw: &base_source_raw,
            head_source_raw: &head_source_raw,
            route_locations,
            changes: contract_changes,
            unresolved_calls,
            discrepancies,
            stats,
        },
    );

    filter_ignored_findings(&mut report, &config.policy);

    Ok(report)
}

/// Pre-computes and persists consumer AST extraction snapshots for every repository declared
/// in `config_path`, populating the SQLite incremental cache for subsequent `laine analyze` runs.
pub fn index_workspace(config_path: &Path) -> Result<IndexSummary> {
    let config = WorkspaceConfig::load_from_path(config_path)?;
    let config_dir = config_directory(config_path);
    let cache_path = resolve_cache_path(config_dir, config.cache_path.as_deref());
    let cache = IndexCache::open(&cache_path)?;
    let config_hash = compute_config_hash(&config);

    let mut repos_indexed = 0usize;
    let mut cache_hits = 0usize;
    let mut cache_misses = 0usize;

    for service in &config.services {
        let repo_path = resolve_repo_path(config_dir, &service.path);
        let git = GitRepo::new(&repo_path);
        let head_sha = git.resolve_revision("HEAD")?;

        let is_valid_hit = cache
            .get_repo_snapshot(&service.name, &head_sha, &config_hash)?
            .and_then(|raw| serde_json::from_str::<CachedConsumerRepo>(&raw).ok())
            .is_some();

        if is_valid_hit {
            cache_hits += 1;
        } else {
            let snapshot = extract_consumer_repo(&git, &head_sha, &config)?;
            let payload = serde_json::to_string(&snapshot)
                .map_err(|err| LaineError::Cache(err.to_string()))?;
            cache.put_repo_snapshot(&service.name, &head_sha, &config_hash, &payload)?;
            cache_misses += 1;
        }

        repos_indexed += 1;
    }

    Ok(IndexSummary {
        repos_indexed,
        cache_hits,
        cache_misses,
        cache_path,
    })
}

fn extract_consumer_repo(
    git: &GitRepo,
    rev: &str,
    config: &WorkspaceConfig,
) -> Result<CachedConsumerRepo> {
    let all_tracked_files = git.list_files(rev)?;
    let candidate_files: Vec<String> = all_tracked_files
        .iter()
        .filter(|f| is_analyzable_ts_or_js(f) || is_analyzable_python(f))
        .cloned()
        .collect();

    let tsconfig_files: Vec<String> = all_tracked_files
        .iter()
        .filter(|f| is_tsconfig_file(f))
        .cloned()
        .collect();
    let tsconfig_sources = git.read_files_batch(rev, &tsconfig_files)?;

    let file_contents: Vec<(String, String)> = git
        .read_files_batch(rev, &candidate_files)?
        .into_iter()
        .collect();

    // Pass 1 (parallel): build repo-wide TypeScript interface/type & Zod schema registry for
    // generic calls (`fetch<ReserveResponse>`), return-type annotations (`: Promise<UserDTO>`),
    // and Zod validators (`UserSchema.parse(data)` / `type User = z.infer<typeof UserSchema>`).
    let per_file_types: Vec<(Vec<TypeContractDef>, Vec<TypeContractDef>)> = file_contents
        .par_iter()
        .map(|(file_path, source)| {
            if is_analyzable_ts_or_js(file_path) {
                let ts_defs = extract_type_definitions(file_path, source).unwrap_or_default();
                let zod_defs = extract_zod_schemas(file_path, source).unwrap_or_default();
                (ts_defs, zod_defs)
            } else {
                (Vec::new(), Vec::new())
            }
        })
        .collect();

    let mut type_registry: BTreeMap<String, TypeContractDef> = BTreeMap::new();
    for (ts_defs, _) in &per_file_types {
        for def in ts_defs {
            type_registry
                .entry(def.name.clone())
                .or_insert_with(|| def.clone());
        }
    }
    for (_, zod_defs) in per_file_types {
        for def in zod_defs {
            // Zod definitions take precedence over placeholder `z.infer` aliases from `types.rs`.
            type_registry.insert(def.name.clone(), def);
        }
    }

    // Pass 2 (parallel): extract HTTP calls, direct field reads (CONFIRMED), and generic type contracts (LIKELY).
    let per_file_extracted: Vec<(
        Vec<CachedConsumeEdge>,
        Vec<CachedFieldReadEdge>,
        Vec<CachedUnresolvedCall>,
    )> = file_contents
        .par_iter()
        .map(|(file_path, source)| {
            let mut consumes = Vec::new();
            let mut field_reads = Vec::new();
            let mut unresolved = Vec::new();

            if is_analyzable_python(file_path) {
                let extracted = extract_python_file(
                    file_path,
                    source,
                    PyExtractContext {
                        services: &config.services,
                        clients: &config.clients,
                        env: &config.env,
                    },
                );

                for call in &extracted.calls {
                    consumes.push(CachedConsumeEdge {
                        file: file_path.clone(),
                        target_service: call.service.clone(),
                        method: call.method,
                        normalized_path: call.normalized_path.clone(),
                        line: call.line,
                        snippet: call.snippet.clone(),
                    });
                }

                for read in &extracted.field_reads {
                    if let Some(call) = extracted.calls.get(read.call_id) {
                        field_reads.push(CachedFieldReadEdge {
                            file: file_path.clone(),
                            target_service: call.service.clone(),
                            method: call.method,
                            normalized_path: call.normalized_path.clone(),
                            field_path: read.field_path.clone(),
                            line: read.line,
                            snippet: read.snippet.clone(),
                            call_file: file_path.clone(),
                            call_line: call.line,
                            confidence: Confidence::Confirmed,
                            custom_trace: Vec::new(),
                        });
                    }
                }

                for item in extracted.unresolved_calls {
                    unresolved.push(CachedUnresolvedCall {
                        file: file_path.clone(),
                        line: item.line,
                        snippet: item.snippet,
                        reason: item.reason,
                    });
                }

                return (consumes, field_reads, unresolved);
            }

            let Ok(extracted) = extract_file_with_context(
                file_path,
                source,
                ExtractContext {
                    services: &config.services,
                    clients: &config.clients,
                    env: &config.env,
                },
            ) else {
                return (consumes, field_reads, unresolved);
            };

            for call in &extracted.calls {
                consumes.push(CachedConsumeEdge {
                    file: file_path.clone(),
                    target_service: call.service.clone(),
                    method: call.method,
                    normalized_path: call.normalized_path.clone(),
                    line: call.line,
                    snippet: call.snippet.clone(),
                });

                if let Some(ref response_type) = call.response_type {
                    for (type_file, field_def) in resolve_type_fields(response_type, &type_registry)
                    {
                        field_reads.push(CachedFieldReadEdge {
                            file: type_file,
                            target_service: call.service.clone(),
                            method: call.method,
                            normalized_path: call.normalized_path.clone(),
                            field_path: field_def.field_path,
                            line: field_def.line,
                            snippet: field_def.snippet,
                            call_file: file_path.clone(),
                            call_line: call.line,
                            confidence: Confidence::Likely,
                            custom_trace: Vec::new(),
                        });
                    }
                }
            }

            for read in &extracted.field_reads {
                if let Some(call) = extracted.calls.get(read.call_id) {
                    field_reads.push(CachedFieldReadEdge {
                        file: file_path.clone(),
                        target_service: call.service.clone(),
                        method: call.method,
                        normalized_path: call.normalized_path.clone(),
                        field_path: read.field_path.clone(),
                        line: read.line,
                        snippet: read.snippet.clone(),
                        call_file: file_path.clone(),
                        call_line: call.line,
                        confidence: Confidence::Confirmed,
                        custom_trace: Vec::new(),
                    });
                }
            }

            for item in extracted.unresolved_calls {
                unresolved.push(CachedUnresolvedCall {
                    file: file_path.clone(),
                    line: item.line,
                    snippet: item.snippet,
                    reason: item.reason,
                });
            }

            (consumes, field_reads, unresolved)
        })
        .collect();

    let mut consumes = Vec::new();
    let mut field_reads = Vec::new();
    let mut unresolved_calls = Vec::new();

    for (file_consumes, file_reads, file_unresolved) in per_file_extracted {
        consumes.extend(file_consumes);
        field_reads.extend(file_reads);
        unresolved_calls.extend(file_unresolved);
    }

    // Pass 3: Summary-based cross-file interprocedural dataflow across TS/JS/Vue modules
    // (`services/` -> `hooks`/`composables`/`stores` -> `components/`, JSX props, `useQuery`, etc.).
    let ts_files: Vec<(String, String)> = file_contents
        .iter()
        .filter(|(p, _)| is_analyzable_ts_or_js(p))
        .cloned()
        .collect();
    if !ts_files.is_empty() {
        let resolver = ModuleResolver::from_git_snapshot(&all_tracked_files, &tsconfig_sources);
        let cross_reads = analyze_cross_file_dataflow(
            &ts_files,
            ExtractContext {
                services: &config.services,
                clients: &config.clients,
                env: &config.env,
            },
            &|importer, specifier| resolver.resolve_import(importer, specifier),
        );
        for item in cross_reads {
            field_reads.push(CachedFieldReadEdge {
                file: item.read_file,
                target_service: item.service,
                method: item.method,
                normalized_path: item.normalized_path,
                field_path: item.field_path,
                line: item.read_line,
                snippet: item.read_snippet,
                call_file: item.origin_file,
                call_line: item.origin_line,
                confidence: item.confidence,
                custom_trace: item.trace,
            });
        }
    }

    consumes.sort_by(|a, b| {
        (
            &a.file,
            a.line,
            &a.target_service,
            a.method,
            &a.normalized_path,
            &a.snippet,
        )
            .cmp(&(
                &b.file,
                b.line,
                &b.target_service,
                b.method,
                &b.normalized_path,
                &b.snippet,
            ))
    });

    field_reads.sort_by(|a, b| {
        (
            &a.file,
            a.line,
            &a.call_file,
            a.call_line,
            &a.target_service,
            a.method,
            &a.normalized_path,
            &a.field_path,
            a.confidence,
            &a.snippet,
        )
            .cmp(&(
                &b.file,
                b.line,
                &b.call_file,
                b.call_line,
                &b.target_service,
                b.method,
                &b.normalized_path,
                &b.field_path,
                b.confidence,
                &b.snippet,
            ))
    });

    unresolved_calls.sort_by(|a, b| {
        (&a.file, a.line, &a.reason, &a.snippet).cmp(&(&b.file, b.line, &b.reason, &b.snippet))
    });

    Ok(CachedConsumerRepo {
        files_analyzed: file_contents.len(),
        consumes,
        field_reads,
        unresolved_calls,
    })
}

fn apply_cached_consumer_repo(
    service_name: &str,
    consumer_rev: &str,
    snapshot: &CachedConsumerRepo,
    graph: &mut DependencyGraph,
    unresolved_calls: &mut Vec<UnresolvedCall>,
    files_analyzed: &mut usize,
) {
    *files_analyzed += snapshot.files_analyzed;

    for edge in &snapshot.consumes {
        graph.link_consumes(
            service_name,
            &edge.file,
            consumer_rev,
            &edge.target_service,
            edge.method,
            &edge.normalized_path,
            edge.line,
            &edge.snippet,
        );
    }

    for read in &snapshot.field_reads {
        graph.link_reads_field_with_trace(
            service_name,
            &read.file,
            consumer_rev,
            &read.target_service,
            read.method,
            &read.normalized_path,
            &read.field_path,
            read.line,
            &read.snippet,
            &read.call_file,
            read.call_line,
            read.confidence,
            &read.custom_trace,
        );
    }

    for unresolved in &snapshot.unresolved_calls {
        unresolved_calls.push(UnresolvedCall {
            repo: service_name.to_string(),
            evidence: Evidence {
                repo: service_name.to_string(),
                revision: consumer_rev.to_string(),
                file: unresolved.file.clone(),
                line: unresolved.line,
                snippet: unresolved.snippet.clone(),
            },
            reason: unresolved.reason.clone(),
        });
    }
}

fn extract_producer_routes(
    git: &GitRepo,
    service_name: &str,
    rev: &str,
    route_cfg: &RouteSourceConfig,
) -> Result<(Vec<ExtractedRoute>, usize, String, String)> {
    let candidate_files: Vec<String> = if !route_cfg.files.is_empty() {
        route_cfg
            .files
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect()
    } else {
        let all_files = git.list_files(rev)?;
        match route_cfg.framework {
            RouteFramework::Laravel => all_files
                .into_iter()
                .filter(|f| f == "routes/api.php" || f == "routes/web.php")
                .collect(),
            RouteFramework::Express => all_files
                .into_iter()
                .filter(|f| is_analyzable_ts_or_js(f))
                .collect(),
            RouteFramework::FastApi | RouteFramework::Flask => all_files
                .into_iter()
                .filter(|f| is_analyzable_python(f))
                .collect(),
        }
    };

    let mut routes = Vec::new();
    let mut files_count = 0usize;
    let mut first_file = String::new();
    let mut first_raw = String::new();

    let batch_contents = git.read_files_batch(rev, &candidate_files)?;

    for (file, raw) in batch_contents {
        files_count += 1;
        if first_file.is_empty() {
            first_file = file.clone();
            first_raw = raw.clone();
        }

        match route_cfg.framework {
            RouteFramework::Laravel => {
                routes.extend(extract_laravel_routes(
                    service_name,
                    &file,
                    &raw,
                    route_cfg.prefix.as_deref(),
                ));
            }
            RouteFramework::Express => {
                if let Ok(extracted) =
                    extract_express_routes(service_name, &file, &raw, route_cfg.prefix.as_deref())
                {
                    routes.extend(extracted);
                }
            }
            RouteFramework::FastApi => {
                routes.extend(extract_fastapi_routes(
                    service_name,
                    &file,
                    &raw,
                    route_cfg.prefix.as_deref(),
                ));
            }
            RouteFramework::Flask => {
                routes.extend(extract_flask_routes(
                    service_name,
                    &file,
                    &raw,
                    route_cfg.prefix.as_deref(),
                ));
            }
        }
    }

    Ok((routes, files_count, first_file, first_raw))
}

fn config_directory(config_path: &Path) -> &Path {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn resolve_cache_path(config_dir: &Path, configured: Option<&Path>) -> PathBuf {
    match configured {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => config_dir.join(path),
        None => config_dir.join(".laine-cache.db"),
    }
}

fn resolve_repo_path(config_dir: &Path, service_path: &Path) -> PathBuf {
    if service_path.is_absolute() {
        service_path.to_path_buf()
    } else {
        config_dir.join(service_path)
    }
}

fn is_analyzable_ts_or_js(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".d.ts") {
        return false;
    }

    // Skip hidden dot-directories (`.agents`, `.claude`, `.github`, `.nuxt`, `.output`, etc.)
    // as well as vendor/build output folders (`node_modules`, `dist`, `build`, `coverage`).
    for segment in lower.split('/') {
        if segment.starts_with('.')
            || matches!(
                segment,
                "node_modules" | "dist" | "build" | "coverage" | "vendor"
            )
        {
            return false;
        }
    }

    lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".vue")
        || lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".mjs")
        || lower.ends_with(".cjs")
}

fn is_analyzable_python(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if !lower.ends_with(".py") {
        return false;
    }

    for segment in lower.split('/') {
        if segment.starts_with('.')
            || matches!(
                segment,
                "venv"
                    | "env"
                    | "__pycache__"
                    | "site-packages"
                    | "node_modules"
                    | "dist"
                    | "build"
            )
        {
            return false;
        }
    }

    true
}

fn is_tsconfig_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.contains("node_modules/") || lower.contains("/dist/") || lower.starts_with("dist/") {
        return false;
    }
    let file_name = lower.rsplit('/').next().unwrap_or(&lower);
    (file_name.starts_with("tsconfig") || file_name.starts_with("jsconfig"))
        && file_name.ends_with(".json")
}
