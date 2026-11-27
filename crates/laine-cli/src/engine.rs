use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use laine_core::cache::{IndexCache, compute_config_hash, stable_fingerprint};
use laine_core::config::{RouteFramework, RouteSourceConfig, ServiceConfig, WorkspaceConfig};
use laine_core::error::{LaineError, Result};
use laine_core::git::{GitRepo, STAGED_REV, WORKTREE_REV, to_git_posix_path};
use laine_core::graph::{DependencyGraph, routes_match};
use laine_core::impact::{
    ImpactInput, analyze_impact, compute_indirect_consumers, consumers_for_endpoint,
    consumers_for_field,
};
use laine_core::laravel::analyze_laravel_routes;
use laine_core::model::{
    AffectedConsumer, AnalysisCoverage, AnalysisReport, AnalysisStats, Confidence,
    ConsumersCoverage, ContractChange, ContractDiscrepancy, EndpointConsumersReport, EndpointId,
    Evidence, ExtractionDiagnostic, ExtractionFailure, ExtractionStage, FileCoverage, HttpMethod,
    ProducerEvidence, UnresolvedCall, WorkspaceIdentity,
};
use laine_core::openapi::{
    OpenApiExtraction, OpenApiSnapshot, canonical_route_key, diff_contracts,
};
use laine_core::policy::{evaluate_policy, filter_ignored_findings};
use laine_core::request::HttpRequestEvidence;
use laine_core::routes::{
    ExtractedRoute, detect_openapi_discrepancies, diff_routes, normalize_route_path,
};
use laine_core::workspace::{WorkspaceRegistry, normalize_absolute_path};
use laine_py::routes::flask_unresolved_mounts;
use laine_py::{
    PyExtractContext, extract_fastapi_snapshot, extract_flask_routes, extract_python_file,
};
use laine_ts::{
    ExtractContext, ModuleResolver, TypeContractModule, TypeContractRegistry,
    analyze_cross_file_dataflow_with_imports, collect_imported_axios_clients,
    extract_express_snapshot, extract_file_with_imported_axios,
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

pub struct ConsumersRequest {
    pub config_path: PathBuf,
    pub service: String,
    pub method: HttpMethod,
    pub path: String,
    pub field: Option<String>,
    pub producer_rev: String,
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
    #[serde(default)]
    request: HttpRequestEvidence,
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

// Keep source and parser memory bounded even when the Rayon pool has many threads.
const CONSUMER_REPO_BATCH_SIZE: usize = 2;

struct ConsumerRepoJob {
    revision: String,
    cache_key: String,
    work: ConsumerRepoWork,
}

enum ConsumerRepoWork {
    Cached(CachedConsumerRepo),
    Extract {
        git: GitRepo,
        inputs: Option<ConsumerInputs>,
    },
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

// Analyze the selected snapshots and rebuild extraction on cache misses.
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
        let before = base_document.extract(&producer_git, &base_sha, &target_service.name)?;
        let after = head_document.extract(&producer_git, &head_sha, &target_service.name)?;
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

    let ConsumerGraphExtraction {
        coverage: consumers,
        diagnostics: consumer_diagnostics,
        unresolved_calls,
        revisions: consumer_revisions,
    } = populate_consumers_into_graph(
        &config,
        &req.config_path,
        consumer_revision,
        no_cache,
        &mut graph,
    )?;
    files_analyzed += consumers.analyzed;
    diagnostics.extend(consumer_diagnostics);

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
    report.workspace = Some(workspace_identity(
        &config,
        &req.config_path,
        include_repo_roots,
    ));
    report.consumer_revisions = consumer_revisions;

    Ok(report)
}

#[derive(Default)]
struct ProducerCatalog {
    endpoints: BTreeMap<(HttpMethod, String), (EndpointId, ProducerEvidence)>,
    coverage: FileCoverage,
    diagnostics: Vec<ExtractionDiagnostic>,
}

// Load producer declarations from the requested snapshot and prefer OpenAPI for shared routes.
fn load_producer_catalog(
    git: &GitRepo,
    revision: &str,
    service: &ServiceConfig,
) -> Result<ProducerCatalog> {
    let mut catalog = ProducerCatalog::default();
    if let Some(openapi) = &service.openapi {
        let snapshot = OpenApiSnapshot::read(git, revision, &to_git_posix_path(openapi))?;
        let extraction = snapshot.extract(git, revision, &service.name)?;
        catalog.coverage = record_openapi_coverage(
            &extraction,
            &mut catalog.diagnostics,
            &service.name,
            revision,
            ExtractionStage::ProducerHead,
        );
        for (endpoint, contract) in extraction.contract.endpoints {
            let mut evidence = ProducerEvidence {
                file: snapshot.file.clone(),
                ..ProducerEvidence::default()
            };
            if let Some(source) = contract.source() {
                evidence.file.clone_from(&source.file);
                evidence.json_pointer = Some(source.pointer.clone());
                evidence.span = source.span.clone();
                evidence.line = source.span.as_ref().map(|span| span.line);
            }
            catalog.register(endpoint, evidence);
        }
    }

    if let Some(routes) = &service.routes {
        let (routes, extraction) = extract_producer_routes(git, &service.name, revision, routes)?;
        catalog.coverage.accumulate(extraction.coverage);
        extraction.append_diagnostics(
            &mut catalog.diagnostics,
            &service.name,
            revision,
            ExtractionStage::ProducerHead,
        );
        for route in routes {
            catalog.register(
                route.endpoint,
                ProducerEvidence {
                    file: route.file,
                    line: Some(route.line),
                    ..ProducerEvidence::default()
                },
            );
        }
    }
    Ok(catalog)
}

impl ProducerCatalog {
    // Store the first declaration of each canonical route identity.
    fn register(&mut self, endpoint: EndpointId, evidence: ProducerEvidence) {
        let key = (
            endpoint.method,
            canonical_route_key(&endpoint.normalized_path),
        );
        self.endpoints.entry(key).or_insert((endpoint, evidence));
    }
}

/// Query consumer dependencies without comparing producer revisions or evaluating impact policy.
pub fn query_endpoint_consumers(
    req: &ConsumersRequest,
    no_cache: bool,
    include_repo_roots: bool,
    consumer_revision: ConsumerRevision,
) -> Result<EndpointConsumersReport> {
    let config = WorkspaceConfig::load_from_path(&req.config_path)?;
    let service = config.find_service(&req.service).ok_or_else(|| {
        LaineError::ConfigInvalid(format!(
            "service '{}' not found in {}",
            req.service,
            req.config_path.display()
        ))
    })?;
    let config_dir = config_directory(&req.config_path);
    let git = GitRepo::new(resolve_repo_path(config_dir, &service.path));
    let producer_revision = git.resolve_revision(&req.producer_rev)?;
    let ProducerCatalog {
        endpoints,
        coverage: producer,
        mut diagnostics,
    } = load_producer_catalog(&git, &producer_revision, service)?;

    // Register the entire catalog so calls bind to their most specific route.
    let mut graph = DependencyGraph::new();
    for (endpoint, _) in endpoints.values() {
        graph.add_endpoint(
            &endpoint.service,
            endpoint.method,
            &endpoint.normalized_path,
        );
    }
    let normalized_path = normalize_route_path(&[], &req.path);
    let canonical_query = canonical_route_key(&normalized_path);
    let matches = if let Some(exact) = endpoints.get(&(req.method, canonical_query.clone())) {
        vec![exact]
    } else {
        endpoints
            .iter()
            .filter(|((method, path), _)| {
                *method == req.method && routes_match(path, &canonical_query)
            })
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>()
    };
    let matched_endpoints = matches
        .iter()
        .map(|(endpoint, _)| endpoint.clone())
        .collect::<Vec<_>>();
    let producer_evidence = matches
        .first()
        .map(|(endpoint, evidence)| ProducerEvidence {
            repo: endpoint.service.clone(),
            revision: producer_revision.clone(),
            description: format!("{} {}", endpoint.method.as_str(), endpoint.normalized_path),
            ..evidence.clone()
        });
    let in_catalog = !matched_endpoints.is_empty();
    let fallback = EndpointId {
        service: req.service.clone(),
        method: req.method,
        normalized_path: normalized_path.clone(),
    };
    let queried_endpoints = if in_catalog {
        matched_endpoints.as_slice()
    } else {
        graph.add_endpoint(&req.service, req.method, &normalized_path);
        std::slice::from_ref(&fallback)
    };

    let ConsumerGraphExtraction {
        coverage: consumers,
        diagnostics: consumer_diagnostics,
        mut unresolved_calls,
        revisions: consumer_revisions,
    } = populate_consumers_into_graph(
        &config,
        &req.config_path,
        consumer_revision,
        no_cache,
        &mut graph,
    )?;
    diagnostics.extend(consumer_diagnostics);
    diagnostics.sort();
    unresolved_calls.sort_by(|a, b| {
        (&a.repo, &a.evidence.file, a.evidence.line, &a.reason).cmp(&(
            &b.repo,
            &b.evidence.file,
            b.evidence.line,
            &b.reason,
        ))
    });

    // Merge evidence assigned to several routes while retaining distinct field reads on one line.
    let mut direct = BTreeMap::new();
    let mut fields_read = BTreeSet::new();
    for endpoint in queried_endpoints {
        let hits = if let Some(field) = &req.field {
            consumers_for_field(&graph, endpoint, field)
        } else {
            fields_read.extend(graph.fields_read_on_endpoint(
                &endpoint.service,
                endpoint.method,
                &endpoint.normalized_path,
            ));
            consumers_for_endpoint(&graph, endpoint)
        };
        for hit in hits {
            let key = (
                hit.repo.clone(),
                hit.evidence.file.clone(),
                hit.evidence.line,
                hit.evidence.snippet.clone(),
            );
            if direct
                .get(&key)
                .is_none_or(|prev: &AffectedConsumer| prev.confidence < hit.confidence)
            {
                direct.insert(key, hit);
            }
        }
    }
    let consumers_found: Vec<_> = direct.into_values().collect();
    let direct_repos = consumers_found.iter().map(|hit| hit.repo.clone()).collect();
    let indirect_consumers = compute_indirect_consumers(&graph, &req.service, &direct_repos);

    Ok(EndpointConsumersReport {
        schema_version: 1,
        service: req.service.clone(),
        method: req.method,
        queried_path: req.path.clone(),
        normalized_path,
        field: req.field.clone(),
        producer_revision,
        in_catalog,
        matched_endpoints,
        producer_evidence,
        coverage: ConsumersCoverage {
            complete: diagnostics.is_empty(),
            producer,
            consumers,
        },
        diagnostics,
        exit_code: 0,
        workspace: Some(workspace_identity(
            &config,
            &req.config_path,
            include_repo_roots,
        )),
        consumer_revisions,
        stats: AnalysisStats {
            repositories: config.services.len(),
            files_analyzed: producer.analyzed + consumers.analyzed,
            graph_nodes: graph.node_count(),
            graph_edges: graph.edge_count(),
        },
        consumers: consumers_found,
        indirect_consumers,
        fields_read: fields_read.into_iter().collect(),
        unresolved_calls,
    })
}

#[derive(Default)]
struct ConsumerGraphExtraction {
    coverage: FileCoverage,
    diagnostics: Vec<ExtractionDiagnostic>,
    unresolved_calls: Vec<UnresolvedCall>,
    revisions: BTreeMap<String, String>,
}

// Apply consumer snapshots in bounded batches using the shared extraction cache.
fn populate_consumers_into_graph(
    config: &WorkspaceConfig,
    config_path: &Path,
    consumer_revision: ConsumerRevision,
    no_cache: bool,
    graph: &mut DependencyGraph,
) -> Result<ConsumerGraphExtraction> {
    let config_dir = config_directory(config_path);
    let config_hash = compute_config_hash(config);
    let cache = if no_cache {
        None
    } else {
        resolve_cache_path(config_path, config.cache_path.as_deref())
            .and_then(|path| IndexCache::open(&path))
            .ok()
    };

    let mut extraction = ConsumerGraphExtraction::default();
    for services in config.services.chunks(CONSUMER_REPO_BATCH_SIZE) {
        // Read cache entries on this thread. Mutable snapshots reuse the inputs hashed for their key.
        let jobs: Vec<_> = services
            .iter()
            .map(|service| {
                let git = GitRepo::new(resolve_repo_path(config_dir, &service.path));
                let revision = git.resolve_revision(consumer_revision.git_revision())?;
                let inputs =
                    if cache.is_some() && !matches!(consumer_revision, ConsumerRevision::Head) {
                        Some(load_consumer_inputs(&git, &revision)?)
                    } else {
                        None
                    };
                let cache_key = match &inputs {
                    Some(inputs) => inputs.cache_key(&revision)?,
                    None => revision.clone(),
                };
                let cached = cache
                    .as_ref()
                    .and_then(|db| {
                        db.get_repo_snapshot(&service.name, &cache_key, &config_hash)
                            .ok()
                            .flatten()
                    })
                    .and_then(|raw| serde_json::from_str::<CachedConsumerRepo>(&raw).ok());
                let work = match cached {
                    Some(snapshot) => ConsumerRepoWork::Cached(snapshot),
                    None => ConsumerRepoWork::Extract { git, inputs },
                };
                Ok(ConsumerRepoJob {
                    revision,
                    cache_key,
                    work,
                })
            })
            .collect();

        // Indexed Rayon collection keeps results in configuration order.
        let extract = |(job, service): (Result<ConsumerRepoJob>, &ServiceConfig)| -> Result<_> {
            let ConsumerRepoJob {
                revision,
                cache_key,
                work,
            } = job?;
            let (snapshot, cache_miss) = match work {
                ConsumerRepoWork::Cached(snapshot) => (snapshot, false),
                ConsumerRepoWork::Extract { git, inputs } => {
                    let inputs = match inputs {
                        Some(inputs) => inputs,
                        None => load_consumer_inputs(&git, &revision)?,
                    };
                    (extract_consumer_repo(inputs, config, &service.name), true)
                }
            };
            Ok((revision, cache_key, snapshot, cache_miss))
        };
        let misses = jobs
            .iter()
            .filter(|job| {
                matches!(
                    job,
                    Ok(ConsumerRepoJob {
                        work: ConsumerRepoWork::Extract { .. },
                        ..
                    })
                )
            })
            .count();
        let snapshots: Vec<_> = if misses > 1 {
            jobs.into_par_iter().zip(services).map(extract).collect()
        } else {
            jobs.into_iter().zip(services).map(extract).collect()
        };

        // Persist and apply each snapshot in configuration order before loading the next batch.
        for (service, result) in services.iter().zip(snapshots) {
            let (revision, cache_key, snapshot, cache_miss) = result?;
            if cache_miss
                && let Some(db) = cache.as_ref()
                && let Ok(payload) = serde_json::to_string(&snapshot)
            {
                let _ = db.put_repo_snapshot(&service.name, &cache_key, &config_hash, &payload);
            }
            graph.add_repository(&service.name);
            apply_cached_consumer_repo(
                &service.name,
                &revision,
                &snapshot,
                graph,
                &mut extraction.unresolved_calls,
            );
            extraction.coverage.accumulate(snapshot.extraction.coverage);
            snapshot.extraction.append_diagnostics(
                &mut extraction.diagnostics,
                &service.name,
                &revision,
                ExtractionStage::Consumer,
            );
            extraction.revisions.insert(service.name.clone(), revision);
        }
    }
    Ok(extraction)
}

// Describe the workspace and optionally expose the absolute repository roots.
fn workspace_identity(
    config: &WorkspaceConfig,
    config_path: &Path,
    include_repo_roots: bool,
) -> WorkspaceIdentity {
    let config_dir = config_directory(config_path);
    let id = workspace_identifier(config_path);
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
    WorkspaceIdentity { id, repo_roots }
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
            let snapshot = extract_consumer_repo(
                load_consumer_inputs(&git, &head_sha)?,
                &config,
                &service.name,
            );
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

// Share the loaded sources across type, call, and cross-file passes for this repository.
fn extract_consumer_repo(
    inputs: ConsumerInputs,
    config: &WorkspaceConfig,
    consumer: &str,
) -> CachedConsumerRepo {
    let clients = config.clients_for_consumer(consumer);
    let mut extraction = FileExtraction::from_reads(&inputs.candidates, &inputs.sources);
    let file_contents: Vec<_> = inputs.sources.iter().collect();
    let mut ts_files: Vec<_> = file_contents
        .iter()
        .filter(|(file, _)| is_analyzable_ts_or_js(file))
        .map(|(file, source)| (file.as_str(), source.as_str()))
        .collect();
    let resolver = ModuleResolver::from_git_snapshot(&inputs.all_files, &inputs.configs);
    let resolve_import =
        |importer: &str, specifier: &str| resolver.resolve_import(importer, specifier);
    let ts_context = ExtractContext {
        services: &config.services,
        clients: &clients,
        env: &config.env,
    };
    let axios_imports = collect_imported_axios_clients(&ts_files, ts_context, &resolve_import);

    // Pass 1: index response contract declarations and module bindings in parallel.
    let per_file_types: Vec<_> = ts_files
        .par_iter()
        .map(|(file, source)| TypeContractModule::extract(file, source))
        .collect();

    let mut type_registry = TypeContractRegistry::default();
    for ((file, _), module) in ts_files.iter().zip(per_file_types) {
        match module {
            Ok(module) => type_registry.insert((*file).to_owned(), module),
            Err(_) => extraction.record_parse_failure(file),
        }
    }

    // Pass 2 (parallel): extract HTTP calls, direct field reads (CONFIRMED), and generic type contracts (LIKELY).
    let per_file_extracted: Vec<_> = file_contents
        .par_iter()
        .map(|&(file_path, source)| {
            let mut consumes = Vec::new();
            let mut field_reads = Vec::new();
            let mut unresolved = Vec::new();

            if is_analyzable_python(file_path) {
                let extracted = extract_python_file(
                    file_path,
                    source,
                    PyExtractContext {
                        services: &config.services,
                        clients: &clients,
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
                        request: call.request.clone(),
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

            let Ok(extracted) = extract_file_with_imported_axios(
                file_path,
                source,
                ts_context,
                axios_imports.get(file_path),
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
                    request: call.request.clone(),
                });

                if let Some(ref response_type) = call.response_type {
                    for (type_file, field_def) in type_registry.resolve_fields(
                        file_path,
                        response_type,
                        call.response_type_is_schema,
                        &resolver,
                    ) {
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
    ts_files.retain(|(file, _)| !extraction.failures.contains_key(*file));
    if !ts_files.is_empty() {
        let cross = analyze_cross_file_dataflow_with_imports(
            &ts_files,
            ts_context,
            &resolve_import,
            &axios_imports,
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
        graph.link_consumes_with_request(
            service_name,
            &edge.file,
            consumer_rev,
            &edge.target_service,
            edge.method,
            &edge.normalized_path,
            edge.line,
            "HTTP call",
            Some(&edge.request),
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

/// Supply discovery with the framework paths from the analysis engine's snapshot extractor.
pub fn catalog_workspace_routes(
    git: &GitRepo,
    service_name: &str,
    revision: &str,
    route_cfg: &RouteSourceConfig,
) -> Vec<String> {
    extract_producer_routes(git, service_name, revision, route_cfg)
        .map(|(routes, _)| {
            routes
                .into_iter()
                .map(|route| route.endpoint.normalized_path)
                .collect()
        })
        .unwrap_or_default()
}

/// Collect route snapshot failures, including Flask mounts, for workspace validation.
pub fn check_workspace_routes(
    git: &GitRepo,
    service_name: &str,
    revision: &str,
    route_cfg: &RouteSourceConfig,
) -> Result<Vec<(String, ExtractionFailure)>> {
    extract_route_snapshot(git, service_name, revision, route_cfg, true)
        .map(|(_, extraction)| extraction.failures.into_iter().collect())
}

// Extract routes while preserving the analysis report's established coverage.
fn extract_producer_routes(
    git: &GitRepo,
    service_name: &str,
    rev: &str,
    route_cfg: &RouteSourceConfig,
) -> Result<(Vec<ExtractedRoute>, FileExtraction)> {
    extract_route_snapshot(git, service_name, rev, route_cfg, false)
}

// Load one snapshot for the framework and keep failures associated with their source files.
fn extract_route_snapshot(
    git: &GitRepo,
    service_name: &str,
    rev: &str,
    route_cfg: &RouteSourceConfig,
    check_flask_mounts: bool,
) -> Result<(Vec<ExtractedRoute>, FileExtraction)> {
    let all_files = git.list_files(rev)?;
    let selected = route_cfg.snapshot_files();
    // Look for mounts and imports outside the configured files in the same revision.
    let candidate_files: Vec<_> = all_files
        .iter()
        .filter(|file| {
            if selected.contains(file.as_str()) {
                return true;
            }
            match route_cfg.framework {
                RouteFramework::Laravel => false,
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
        RouteFramework::Flask if check_flask_mounts => {
            for (file, raw) in batch_contents {
                if !flask_unresolved_mounts(&raw).is_empty() {
                    extraction
                        .record_incomplete(&file, ExtractionFailure::RouteDeclarationUnresolved);
                }
            }
            return Ok((Vec::new(), extraction));
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

// Preserve read failures and follow Git renames between the compared snapshots.
fn read_openapi_snapshots(
    git: &GitRepo,
    base: &str,
    head: &str,
    configured: &Path,
) -> Result<(OpenApiSnapshot, OpenApiSnapshot)> {
    let path = to_git_posix_path(configured);
    let path = path.as_str();
    let read = |revision| match OpenApiSnapshot::read(git, revision, path) {
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
                return Ok((before, OpenApiSnapshot::read(git, head, &destination)?));
            }
            Err(openapi_missing(path, head))
        }
        (None, Some(after)) => {
            if let Some((source, _)) = git
                .renamed_files(base, head)?
                .into_iter()
                .find(|(_, destination)| destination == path)
            {
                return Ok((OpenApiSnapshot::read(git, base, &source)?, after));
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
