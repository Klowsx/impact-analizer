use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use laine_core::cache::{IndexCache, compute_config_hash, stable_fingerprint};
use laine_core::config::{RouteFramework, RouteSourceConfig, WorkspaceConfig};
use laine_core::error::{LaineError, Result};
use laine_core::git::{GitRepo, STAGED_REV, WORKTREE_REV};
use laine_core::graph::DependencyGraph;
use laine_core::impact::{ImpactInput, analyze_impact};
use laine_core::laravel::analyze_laravel_routes;
use laine_core::model::{
    AnalysisCoverage, AnalysisReport, AnalysisStats, Confidence, ContractChange,
    ContractDiscrepancy, Evidence, ExtractionDiagnostic, ExtractionFailure, ExtractionStage,
    FileCoverage, HttpMethod, UnresolvedCall, WorkspaceIdentity,
};
use laine_core::openapi::{
    OpenApiExtraction, canonical_route_key, diff_contracts, extract_contract_with_references,
};
use laine_core::policy::{evaluate_policy, filter_ignored_findings};
use laine_core::routes::{ExtractedRoute, detect_openapi_discrepancies, diff_routes};
use laine_core::workspace::{WorkspaceRegistry, normalize_absolute_path};
use laine_py::{
    PyExtractContext, extract_fastapi_snapshot, extract_flask_routes, extract_python_file,
};
use laine_ts::{
    ExtractContext, ModuleResolver, TypeContractDef, analyze_cross_file_dataflow_with_limits,
    extract_express_snapshot, extract_file_with_context, extract_type_definitions,
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

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum ConsumerRevision {
    #[default]
    Head,
    Staged,
    Worktree,
}

impl ConsumerRevision {
    // Convierte la opción CLI a la revisión de Git o al nombre del snapshot local.
    fn git_revision(self) -> &'static str {
        match self {
            Self::Head => "HEAD",
            Self::Staged => STAGED_REV,
            Self::Worktree => WORKTREE_REV,
        }
    }
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CachedFieldReadEdge {
    file: String,
    target_service: String,
    method: HttpMethod,
    normalized_path: String,
    field_path: String,
    line: usize,
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
    reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct CachedConsumerRepo {
    extraction: FileExtraction,
    consumes: Vec<CachedConsumeEdge>,
    field_reads: Vec<CachedFieldReadEdge>,
    unresolved_calls: Vec<CachedUnresolvedCall>,
}

struct ConsumerInputs {
    all_files: Vec<String>,
    candidates: Vec<String>,
    configs: BTreeMap<String, String>,
    sources: BTreeMap<String, String>,
}

impl ConsumerInputs {
    // Identifica un snapshot mutable por los archivos y contenidos usados para extraerlo.
    fn cache_key(&self, revision: &str) -> Result<String> {
        let sources = self
            .all_files
            .iter()
            .map(|path| {
                let contents = self.sources.get(path).or_else(|| self.configs.get(path));
                (
                    path,
                    contents.map(|text| stable_fingerprint(text.as_bytes())),
                )
            })
            .collect::<Vec<_>>();
        let bytes =
            serde_json::to_vec(&sources).map_err(|err| LaineError::Cache(err.to_string()))?;
        Ok(format!("{revision}:{}", stable_fingerprint(&bytes)))
    }
}

struct OpenApiSnapshot {
    file: String,
    contents: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FileExtraction {
    coverage: FileCoverage,
    failures: BTreeMap<String, ExtractionFailure>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    failure_lines: BTreeMap<String, Vec<usize>>,
}

impl FileExtraction {
    // Cuenta los candidatos y detecta cuáles no llegaron de la lectura por lotes.
    fn from_reads(files: &[String], contents: &BTreeMap<String, String>) -> Self {
        let failures = files
            .iter()
            .filter(|file| !contents.contains_key(*file))
            .map(|file| (file.clone(), ExtractionFailure::FileReadFailed))
            .collect::<BTreeMap<_, _>>();
        let read = contents.len();
        Self {
            coverage: FileCoverage {
                discovered: files.len(),
                read,
                analyzed: read,
                failed: failures.len(),
            },
            failures,
            failure_lines: BTreeMap::new(),
        }
    }

    // Descuenta un archivo que se pudo leer, pero cuya extracción falló.
    fn record_parse_failure(&mut self, file: &str) {
        self.record_incomplete(file, ExtractionFailure::FileParseFailed);
    }

    // Descuenta la cobertura de un archivo cuando se perdió parte de su evidencia.
    fn record_incomplete(&mut self, file: &str, code: ExtractionFailure) {
        if self.failures.insert(file.to_owned(), code).is_none() {
            self.coverage.analyzed -= 1;
            self.coverage.failed += 1;
        }
    }

    // Añade el contexto de repositorio y revisión a fallos portables que también se cachean.
    fn append_diagnostics(
        &self,
        diagnostics: &mut Vec<ExtractionDiagnostic>,
        repo: &str,
        revision: &str,
        stage: ExtractionStage,
    ) {
        for (file, code) in &self.failures {
            let lines = self.failure_lines.get(file);
            let positions = lines
                .into_iter()
                .flatten()
                .copied()
                .map(Some)
                .chain(lines.is_none().then_some(None));
            diagnostics.extend(positions.map(|line| ExtractionDiagnostic {
                repo: repo.to_owned(),
                revision: revision.to_owned(),
                file: file.clone(),
                stage,
                code: *code,
                line,
                json_pointer: None,
                remediation: code.remediation().to_owned(),
            }));
        }
    }
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
// Ejecuta el análisis portable sin incluir rutas absolutas de los checkouts.
pub fn run_analysis_with_options(req: &AnalyzeRequest, no_cache: bool) -> Result<AnalysisReport> {
    run_analysis_with_local_paths(req, no_cache, false)
}

// Conserva la revisión HEAD de los consumidores al pedir solo el mapa local.
pub fn run_analysis_with_local_paths(
    req: &AnalyzeRequest,
    no_cache: bool,
    include_repo_roots: bool,
) -> Result<AnalysisReport> {
    run_analysis_with_consumer_revision(req, no_cache, include_repo_roots, ConsumerRevision::Head)
}

// Analiza los snapshots elegidos y reconstruye la extracción si el caché falla.
pub fn run_analysis_with_consumer_revision(
    req: &AnalyzeRequest,
    no_cache: bool,
    include_repo_roots: bool,
    consumer_revision: ConsumerRevision,
) -> Result<AnalysisReport> {
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
        let mut message = format!(
            "service '{}' must configure `openapi` and/or `routes` in {}",
            req.target_repo,
            req.config_path.display()
        );
        let candidates: std::collections::BTreeSet<_> = config
            .services
            .iter()
            .filter(|service| service.openapi.is_some() || service.routes.is_some())
            .map(|service| service.name.as_str())
            .collect();
        if !candidates.is_empty() {
            message.push_str(". Choose a configured producer:\n");
            message.push_str(
                &candidates
                    .iter()
                    .map(|name| {
                        format!(
                            "  laine check --config \"{}\" --repo \"{name}\"",
                            req.config_path.display()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        return Err(LaineError::ConfigInvalid(message));
    }

    let producer_path = resolve_repo_path(config_dir, &target_service.path);
    let producer_git = GitRepo::new(&producer_path);
    let base_sha = producer_git.resolve_revision(&req.base_rev)?;
    let head_sha = producer_git.resolve_revision(&req.head_rev)?;

    let mut graph = DependencyGraph::new();
    graph.add_repository(&target_service.name);

    let mut files_analyzed = 0usize;
    let mut producer_base = FileCoverage::default();
    let mut producer_head = FileCoverage::default();
    let mut consumers = FileCoverage::default();
    let mut diagnostics = Vec::new();
    let mut contract_changes: Vec<ContractChange> = Vec::new();
    let mut discrepancies: Vec<ContractDiscrepancy> = Vec::new();
    let mut route_locations: BTreeMap<(String, HttpMethod, String), (String, usize)> =
        BTreeMap::new();
    let mut base_source_file = String::new();
    let mut head_source_file = String::new();

    // 1. Optional OpenAPI producer contract analysis
    let mut head_openapi_contract = None;
    let mut base_openapi_contract = None;
    let mut head_openapi_complete = true;
    if let Some(openapi_rel) = target_service.openapi.as_deref() {
        let (base_document, head_document) =
            read_openapi_snapshots(&producer_git, &base_sha, &head_sha, openapi_rel)?;
        let before = extract_contract_with_references(
            &target_service.name,
            &base_document.file,
            &base_document.contents,
            |path| producer_git.read_file(&base_sha, Path::new(path)).ok(),
        )?;
        let after = extract_contract_with_references(
            &target_service.name,
            &head_document.file,
            &head_document.contents,
            |path| producer_git.read_file(&head_sha, Path::new(path)).ok(),
        )?;
        producer_base = record_openapi_coverage(
            &before,
            &mut diagnostics,
            &target_service.name,
            &base_sha,
            ExtractionStage::ProducerBase,
        );
        producer_head = record_openapi_coverage(
            &after,
            &mut diagnostics,
            &target_service.name,
            &head_sha,
            ExtractionStage::ProducerHead,
        );
        files_analyzed += producer_base.analyzed;
        head_openapi_complete = after.diagnostics.is_empty();
        let base_contract = before.contract;
        let head_contract = after.contract;
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

        base_source_file = base_document.file;
        head_source_file = head_document.file;
        base_openapi_contract = Some(base_contract);
        head_openapi_contract = Some(head_contract);
    }

    // 2. Optional code-based producer route extraction (Express / Laravel / FastAPI / Flask)
    if let Some(route_cfg) = target_service.routes.as_ref() {
        let (base_routes, base_extraction) =
            extract_producer_routes(&producer_git, &target_service.name, &base_sha, route_cfg)?;
        let (head_routes, head_extraction) =
            extract_producer_routes(&producer_git, &target_service.name, &head_sha, route_cfg)?;
        files_analyzed += base_extraction.coverage.analyzed;
        producer_base.accumulate(base_extraction.coverage);
        producer_head.accumulate(head_extraction.coverage);
        base_extraction.append_diagnostics(
            &mut diagnostics,
            &target_service.name,
            &base_sha,
            ExtractionStage::ProducerBase,
        );
        head_extraction.append_diagnostics(
            &mut diagnostics,
            &target_service.name,
            &head_sha,
            ExtractionStage::ProducerHead,
        );

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

        // Con rutas incompletas no hay evidencia suficiente para afirmar que se eliminó un montaje.
        let complete = base_extraction.coverage.failed == 0 && head_extraction.coverage.failed == 0;
        let route_changes = if complete {
            diff_routes(&base_routes, &head_routes)
        } else {
            Vec::new()
        };
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

        if head_extraction.coverage.failed == 0
            && head_openapi_complete
            && let Some(head_openapi) = head_openapi_contract.as_ref()
        {
            discrepancies = detect_openapi_discrepancies(head_openapi, &head_routes);
        }
    }

    // 3. Consumer repository analysis (TypeScript / Vue / JavaScript / Python) with Rayon + SQLite cache
    let config_hash = compute_config_hash(&config);
    let cache = if no_cache {
        None
    } else {
        resolve_cache_path(&req.config_path, config.cache_path.as_deref())
            .and_then(|path| IndexCache::open(&path))
            .ok()
    };

    let mut unresolved_calls = Vec::new();
    let mut consumer_revisions = BTreeMap::new();
    for service in &config.services {
        graph.add_repository(&service.name);
        let consumer_path = resolve_repo_path(config_dir, &service.path);
        let consumer_git = GitRepo::new(&consumer_path);
        let consumer_rev = consumer_git.resolve_revision(consumer_revision.git_revision())?;
        consumer_revisions.insert(service.name.clone(), consumer_rev.clone());
        let mutable_inputs =
            if cache.is_some() && !matches!(consumer_revision, ConsumerRevision::Head) {
                Some(load_consumer_inputs(&consumer_git, &consumer_rev)?)
            } else {
                None
            };
        let cache_key = if let Some(inputs) = &mutable_inputs {
            inputs.cache_key(&consumer_rev)?
        } else {
            consumer_rev.clone()
        };

        let cached_snapshot = cache
            .as_ref()
            .and_then(|cache_db| {
                cache_db
                    .get_repo_snapshot(&service.name, &cache_key, &config_hash)
                    .ok()
                    .flatten()
            })
            .and_then(|raw| serde_json::from_str::<CachedConsumerRepo>(&raw).ok());

        let snapshot = match cached_snapshot {
            Some(hit) => hit,
            None => {
                let inputs = match mutable_inputs {
                    Some(inputs) => inputs,
                    None => load_consumer_inputs(&consumer_git, &consumer_rev)?,
                };
                let extracted = extract_consumer_repo(inputs, &config);
                if let Some(cache_db) = cache.as_ref()
                    && let Ok(payload) = serde_json::to_string(&extracted)
                {
                    // Los resultados se pueden usar aunque solo falle su persistencia.
                    let _ = cache_db.put_repo_snapshot(
                        &service.name,
                        &cache_key,
                        &config_hash,
                        &payload,
                    );
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
        );
        files_analyzed += snapshot.extraction.coverage.analyzed;
        consumers.accumulate(snapshot.extraction.coverage);
        snapshot.extraction.append_diagnostics(
            &mut diagnostics,
            &service.name,
            &consumer_rev,
            ExtractionStage::Consumer,
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
            base_source_file: &base_source_file,
            head_source_file: &head_source_file,
            base_contract: base_openapi_contract.as_ref(),
            head_contract: head_openapi_contract.as_ref(),
            route_locations,
            changes: contract_changes,
            unresolved_calls,
            discrepancies,
            stats,
        },
    );

    filter_ignored_findings(&mut report, &config.policy);
    diagnostics.sort();
    report.coverage = Some(AnalysisCoverage {
        complete: diagnostics.is_empty(),
        producer_base,
        producer_head,
        consumers,
    });
    report.diagnostics = diagnostics;
    let policy = evaluate_policy(&report, &config.policy);
    report.exit_code = Some(policy.exit_code);
    report.policy = Some(policy);
    let id = workspace_identifier(&req.config_path);
    let repo_roots = if include_repo_roots {
        config
            .services
            .iter()
            .map(|service| {
                let path = normalize_absolute_path(&resolve_repo_path(config_dir, &service.path));
                (service.name.clone(), path.to_string_lossy().into_owned())
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    report.workspace = Some(WorkspaceIdentity { id, repo_roots });
    report.consumer_revisions = consumer_revisions;

    Ok(report)
}

/// Pre-computes and persists consumer AST extraction snapshots for every repository declared
/// in `config_path`, populating the SQLite incremental cache for subsequent `laine analyze` runs.
// Exige acceso al caché cuando se solicita explícitamente precalcularlo.
pub fn index_workspace(config_path: &Path) -> Result<IndexSummary> {
    let config = WorkspaceConfig::load_from_path(config_path)?;
    let config_dir = config_directory(config_path);
    let cache_path = resolve_cache_path(config_path, config.cache_path.as_deref())?;
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
            let snapshot = extract_consumer_repo(load_consumer_inputs(&git, &head_sha)?, &config);
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

// Enumera y lee una sola vez los recursos que alimentan al analizador de consumidores.
fn load_consumer_inputs(git: &GitRepo, rev: &str) -> Result<ConsumerInputs> {
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

    let sources = git.read_files_batch(rev, &candidate_files)?;
    Ok(ConsumerInputs {
        all_files: all_tracked_files,
        candidates: candidate_files,
        configs: tsconfig_sources,
        sources,
    })
}

// Extrae hechos estructurados de los archivos del snapshot que ya se leyeron.
fn extract_consumer_repo(inputs: ConsumerInputs, config: &WorkspaceConfig) -> CachedConsumerRepo {
    let mut extraction = FileExtraction::from_reads(&inputs.candidates, &inputs.sources);
    let file_contents: Vec<(String, String)> = inputs.sources.into_iter().collect();

    // Pass 1 (parallel): build repo-wide TypeScript interface/type & Zod schema registry for
    // generic calls (`fetch<ReserveResponse>`), return-type annotations (`: Promise<UserDTO>`),
    // and Zod validators (`UserSchema.parse(data)` / `type User = z.infer<typeof UserSchema>`).
    let per_file_types: Vec<_> = file_contents
        .par_iter()
        .map(|(file_path, source)| {
            if is_analyzable_ts_or_js(file_path) {
                let ts_defs = extract_type_definitions(file_path, source);
                let zod_defs = extract_zod_schemas(file_path, source);
                let failed = ts_defs.is_err() || zod_defs.is_err();
                (
                    ts_defs.unwrap_or_default(),
                    zod_defs.unwrap_or_default(),
                    failed,
                )
            } else {
                (Vec::new(), Vec::new(), false)
            }
        })
        .collect();

    let mut type_registry: BTreeMap<String, TypeContractDef> = BTreeMap::new();
    for ((file, _), (ts_defs, _, failed)) in file_contents.iter().zip(&per_file_types) {
        if *failed {
            extraction.record_parse_failure(file);
        }
        for def in ts_defs {
            type_registry
                .entry(def.name.clone())
                .or_insert_with(|| def.clone());
        }
    }
    for (_, zod_defs, _) in per_file_types {
        for def in zod_defs {
            // Zod definitions take precedence over placeholder `z.infer` aliases from `types.rs`.
            type_registry.insert(def.name.clone(), def);
        }
    }

    // Pass 2 (parallel): extract HTTP calls, direct field reads (CONFIRMED), and generic type contracts (LIKELY).
    let per_file_extracted: Vec<_> = file_contents
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
                        reason: item.reason,
                    });
                }

                return Some((consumes, field_reads, unresolved));
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
                return None;
            };

            for call in &extracted.calls {
                consumes.push(CachedConsumeEdge {
                    file: file_path.clone(),
                    target_service: call.service.clone(),
                    method: call.method,
                    normalized_path: call.normalized_path.clone(),
                    line: call.line,
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
                    reason: item.reason,
                });
            }

            Some((consumes, field_reads, unresolved))
        })
        .collect();

    let mut consumes = Vec::new();
    let mut field_reads = Vec::new();
    let mut unresolved_calls = Vec::new();

    for ((file, _), result) in file_contents.iter().zip(per_file_extracted) {
        if let Some((file_consumes, file_reads, file_unresolved)) = result {
            consumes.extend(file_consumes);
            field_reads.extend(file_reads);
            unresolved_calls.extend(file_unresolved);
        } else {
            extraction.record_parse_failure(file);
        }
    }

    // Pass 3: Summary-based cross-file interprocedural dataflow across TS/JS/Vue modules
    // (`services/` -> `hooks`/`composables`/`stores` -> `components/`, JSX props, `useQuery`, etc.).
    let ts_files: Vec<(String, String)> = file_contents
        .iter()
        .filter(|(p, _)| is_analyzable_ts_or_js(p) && !extraction.failures.contains_key(p))
        .cloned()
        .collect();
    if !ts_files.is_empty() {
        let resolver = ModuleResolver::from_git_snapshot(&inputs.all_files, &inputs.configs);
        let cross = analyze_cross_file_dataflow_with_limits(
            &ts_files,
            ExtractContext {
                services: &config.services,
                clients: &config.clients,
                env: &config.env,
            },
            &|importer, specifier| resolver.resolve_import(importer, specifier),
        );
        for file in cross.limited_files {
            extraction.record_incomplete(&file, ExtractionFailure::DataflowLimitReached);
        }
        for item in cross.field_reads {
            field_reads.push(CachedFieldReadEdge {
                file: item.read_file,
                target_service: item.service,
                method: item.method,
                normalized_path: item.normalized_path,
                field_path: item.field_path,
                line: item.read_line,
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
        )
            .cmp(&(
                &b.file,
                b.line,
                &b.target_service,
                b.method,
                &b.normalized_path,
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
            ))
    });

    unresolved_calls
        .sort_by(|a, b| (&a.file, a.line, &a.reason).cmp(&(&b.file, b.line, &b.reason)));

    CachedConsumerRepo {
        extraction,
        consumes,
        field_reads,
        unresolved_calls,
    }
}

// Genera descripciones a partir de los hechos y conserva la ubicación de cada evidencia.
fn apply_cached_consumer_repo(
    service_name: &str,
    consumer_rev: &str,
    snapshot: &CachedConsumerRepo,
    graph: &mut DependencyGraph,
    unresolved_calls: &mut Vec<UnresolvedCall>,
) {
    for edge in &snapshot.consumes {
        graph.link_consumes(
            service_name,
            &edge.file,
            consumer_rev,
            &edge.target_service,
            edge.method,
            &edge.normalized_path,
            edge.line,
            "HTTP call",
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
            &format!("Response field: {}", read.field_path),
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
                snippet: "Unresolved HTTP call".into(),
            },
            reason: unresolved.reason.clone(),
        });
    }
}

// Extracts routes and keeps unresolved mounts tied to the analyzed revision.
fn extract_producer_routes(
    git: &GitRepo,
    service_name: &str,
    rev: &str,
    route_cfg: &RouteSourceConfig,
) -> Result<(Vec<ExtractedRoute>, FileExtraction)> {
    let all_files = git.list_files(rev)?;
    let selected: BTreeSet<_> = route_cfg
        .files
        .iter()
        .map(|path| {
            path.to_string_lossy()
                .replace('\\', "/")
                .trim_start_matches("./")
                .to_owned()
        })
        .collect();
    // Look for mounts and imports outside the configured files in the same revision.
    let candidate_files: Vec<_> = all_files
        .iter()
        .filter(|file| {
            if selected.contains(file.as_str()) {
                return true;
            }
            match route_cfg.framework {
                RouteFramework::Laravel => {
                    selected.is_empty()
                        && matches!(file.as_str(), "routes/api.php" | "routes/web.php")
                }
                RouteFramework::Express => {
                    is_analyzable_ts_or_js(file)
                        && (selected.is_empty() || !file.to_ascii_lowercase().ends_with(".vue"))
                }
                RouteFramework::FastApi => is_analyzable_python(file),
                RouteFramework::Flask => selected.is_empty() && is_analyzable_python(file),
            }
        })
        .cloned()
        .collect();

    let batch_contents = git.read_files_batch(rev, &candidate_files)?;
    let mut extraction = FileExtraction::from_reads(&candidate_files, &batch_contents);
    let prefix = route_cfg.prefix.as_deref();
    let snapshot = match route_cfg.framework {
        RouteFramework::Express => {
            let config_files = all_files
                .iter()
                .filter(|file| is_tsconfig_file(file))
                .cloned()
                .collect::<Vec<_>>();
            let configs = git.read_files_batch(rev, &config_files)?;
            let resolver = ModuleResolver::from_git_snapshot(&all_files, &configs);
            let (snapshot, errors) = extract_express_snapshot(
                service_name,
                &batch_contents,
                &resolver,
                prefix,
                &selected,
            );
            for file in errors.keys() {
                extraction.record_parse_failure(file);
            }
            snapshot
        }
        RouteFramework::FastApi => {
            extract_fastapi_snapshot(service_name, &batch_contents, prefix, &selected)
        }
        RouteFramework::Laravel => {
            let mut routes = Vec::new();
            for (file, raw) in batch_contents {
                let (found, unresolved) = analyze_laravel_routes(service_name, &file, &raw, prefix);
                routes.extend(found);
                if !unresolved.is_empty() {
                    extraction
                        .record_incomplete(&file, ExtractionFailure::RouteDeclarationUnresolved);
                    extraction.failure_lines.insert(file, unresolved);
                }
            }
            return Ok((routes, extraction));
        }
        RouteFramework::Flask => {
            let routes = batch_contents
                .into_iter()
                .flat_map(|(file, raw)| extract_flask_routes(service_name, &file, &raw, prefix))
                .collect();
            return Ok((routes, extraction));
        }
    };
    for (file, lines) in snapshot.unresolved_mounts {
        extraction.record_incomplete(&file, ExtractionFailure::RouteDeclarationUnresolved);
        extraction
            .failure_lines
            .insert(file, lines.into_iter().collect());
    }

    Ok((snapshot.routes, extraction))
}

// Conserva archivos ilegibles como errores de lectura y sigue renombres detectados por Git.
fn read_openapi_snapshots(
    git: &GitRepo,
    base: &str,
    head: &str,
    configured: &Path,
) -> Result<(OpenApiSnapshot, OpenApiSnapshot)> {
    let path = configured.to_string_lossy();
    let path = if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.into_owned()
    };
    let path = path.trim_start_matches("./");
    let read = |revision| match read_openapi_snapshot(git, revision, path) {
        Ok(snapshot) => Ok(Some(snapshot)),
        Err(_)
            if git
                .list_files(revision)?
                .binary_search_by(|name| name.as_str().cmp(path))
                .is_err() =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    };

    match (read(base)?, read(head)?) {
        (Some(before), Some(after)) => Ok((before, after)),
        (Some(before), None) => {
            if let Some((_, destination)) = git
                .renamed_files(base, head)?
                .into_iter()
                .find(|(source, _)| source == path)
            {
                return Ok((before, read_openapi_snapshot(git, head, &destination)?));
            }
            Err(openapi_missing(path, head))
        }
        (None, Some(after)) => {
            if let Some((source, _)) = git
                .renamed_files(base, head)?
                .into_iter()
                .find(|(_, destination)| destination == path)
            {
                return Ok((read_openapi_snapshot(git, base, &source)?, after));
            }
            Err(openapi_missing(path, base))
        }
        (None, None) => Err(openapi_missing(path, &format!("{base} and {head}"))),
    }
}

// Conserva diagnósticos de referencias y descuenta solo los archivos que los contienen.
fn record_openapi_coverage(
    extraction: &OpenApiExtraction,
    diagnostics: &mut Vec<ExtractionDiagnostic>,
    repo: &str,
    revision: &str,
    stage: ExtractionStage,
) -> FileCoverage {
    let mut failed_files = HashSet::new();
    for (source, code) in &extraction.diagnostics {
        failed_files.insert(&source.file);
        diagnostics.push(ExtractionDiagnostic {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            file: source.file.clone(),
            stage,
            code: *code,
            line: source.span.as_ref().map(|span| span.line),
            json_pointer: Some(source.pointer.clone()),
            remediation: code.remediation().to_owned(),
        });
    }
    let failed = failed_files.len();
    FileCoverage {
        discovered: extraction.loaded_files,
        read: extraction.loaded_files,
        analyzed: extraction.loaded_files - failed,
        failed,
    }
}

// Lee el contrato y conserva la ruta exacta del archivo en su revisión.
fn read_openapi_snapshot(git: &GitRepo, revision: &str, path: &str) -> Result<OpenApiSnapshot> {
    Ok(OpenApiSnapshot {
        file: path.to_owned(),
        contents: git.read_file(revision, Path::new(path))?,
    })
}

// Propone verificar la ruta o añadir el renombre al índice cuando falta el documento.
fn openapi_missing(path: &str, revision: &str) -> LaineError {
    LaineError::ConfigInvalid(format!(
        "OpenAPI file '{path}' is absent in {revision}. Verify the configured path or stage the renamed file."
    ))
}

fn config_directory(config_path: &Path) -> &Path {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

// Usa el estado del usuario para el caché por defecto y respeta el destino configurado.
fn resolve_cache_path(config_path: &Path, configured: Option<&Path>) -> Result<PathBuf> {
    Ok(match configured {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => config_directory(config_path).join(path),
        None => WorkspaceRegistry::default_home_dir()?
            .join("cache")
            .join(format!("{}.sqlite3", workspace_identifier(config_path))),
    })
}

// Da al TOML un identificador local que sirve para aislar reportes y cachés.
fn workspace_identifier(config_path: &Path) -> String {
    stable_fingerprint(
        normalize_absolute_path(config_path)
            .as_os_str()
            .as_encoded_bytes(),
    )
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
