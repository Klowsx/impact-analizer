use std::collections::HashSet;
use std::path::{Path, PathBuf};

use laine_core::config::{RouteFramework, RouteSourceConfig, WorkspaceConfig};
use laine_core::error::{LaineError, Result};
use laine_core::git::GitRepo;
use laine_core::graph::DependencyGraph;
use laine_core::impact::{ImpactInput, analyze_impact};
use laine_core::laravel::extract_laravel_routes;
use laine_core::model::{
    AnalysisReport, AnalysisStats, ContractChange, ContractDiscrepancy, Evidence, UnresolvedCall,
};
use laine_core::openapi::{canonical_route_key, diff_contracts, extract_contract};
use laine_core::routes::{ExtractedRoute, detect_openapi_discrepancies, diff_routes};
use laine_ts::{ExtractContext, extract_express_routes, extract_file_with_context};

#[derive(Debug, Clone)]
pub struct AnalyzeRequest {
    pub config_path: PathBuf,
    pub target_repo: String,
    pub base_rev: String,
    pub head_rev: String,
}

/// Runs a full cross-repository impact analysis without mutating any repository's working tree.
///
/// Supports producers configured with:
/// - `openapi` specification (OpenAPI 3.0/3.1 YAML or JSON)
/// - `routes` code extraction (Express or Laravel) without requiring OpenAPI
/// - Both `openapi` and `routes` (also reporting discrepancies between spec and code)
pub fn run_analysis(req: &AnalyzeRequest) -> Result<AnalysisReport> {
    let config = WorkspaceConfig::load_from_path(&req.config_path)?;
    let config_dir = req
        .config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

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
    let mut source_file_display = String::new();
    let mut base_source_raw = String::new();

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
        head_openapi_contract = Some(head_contract);
    }

    // 2. Optional code-based producer route extraction (Express / Laravel)
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

    // 3. Consumer repository analysis (TypeScript / JavaScript)
    let mut unresolved_calls = Vec::new();
    for service in &config.services {
        if service.name == target_service.name {
            continue;
        }

        graph.add_repository(&service.name);
        let consumer_path = resolve_repo_path(config_dir, &service.path);
        let consumer_git = GitRepo::new(&consumer_path);
        let consumer_rev = consumer_git.resolve_revision("HEAD")?;

        for file_path in consumer_git.list_files(&consumer_rev)? {
            if !is_analyzable_ts_or_js(&file_path) {
                continue;
            }

            let source = consumer_git.read_file(&consumer_rev, Path::new(&file_path))?;
            files_analyzed += 1;

            let Ok(extracted) = extract_file_with_context(
                &file_path,
                &source,
                ExtractContext {
                    services: &config.services,
                    clients: &config.clients,
                    env: &config.env,
                },
            ) else {
                continue;
            };

            for call in &extracted.calls {
                graph.link_consumes(
                    &service.name,
                    &file_path,
                    &consumer_rev,
                    &call.service,
                    call.method,
                    &call.normalized_path,
                    call.line,
                    &call.snippet,
                );
            }

            for read in &extracted.field_reads {
                if let Some(call) = extracted.calls.get(read.call_id) {
                    graph.link_reads_field(
                        &service.name,
                        &file_path,
                        &consumer_rev,
                        &call.service,
                        call.method,
                        &call.normalized_path,
                        &read.field_path,
                        read.line,
                        &read.snippet,
                        call.line,
                    );
                }
            }

            for unresolved in extracted.unresolved_calls {
                unresolved_calls.push(UnresolvedCall {
                    repo: service.name.clone(),
                    evidence: Evidence {
                        repo: service.name.clone(),
                        revision: consumer_rev.clone(),
                        file: file_path.clone(),
                        line: unresolved.line,
                        snippet: unresolved.snippet,
                    },
                    reason: unresolved.reason,
                });
            }
        }
    }

    let stats = AnalysisStats {
        repositories: config.services.len(),
        files_analyzed,
        graph_nodes: graph.node_count(),
        graph_edges: graph.edge_count(),
    };

    Ok(analyze_impact(
        &graph,
        ImpactInput {
            target_repo: &target_service.name,
            base_rev: &base_sha,
            head_rev: &head_sha,
            source_file: &source_file_display,
            base_source_raw: &base_source_raw,
            changes: contract_changes,
            unresolved_calls,
            discrepancies,
            stats,
        },
    ))
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
        }
    };

    let mut routes = Vec::new();
    let mut files_count = 0usize;
    let mut first_file = String::new();
    let mut first_raw = String::new();

    for file in candidate_files {
        let Ok(raw) = git.read_file(rev, Path::new(&file)) else {
            continue;
        };
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
        }
    }

    Ok((routes, files_count, first_file, first_raw))
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
    if lower.ends_with(".d.ts")
        || lower.starts_with("node_modules/")
        || lower.contains("/node_modules/")
        || lower.starts_with("dist/")
        || lower.contains("/dist/")
        || lower.starts_with(".nuxt/")
        || lower.contains("/.nuxt/")
        || lower.starts_with(".output/")
        || lower.contains("/.output/")
    {
        return false;
    }

    lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".mjs")
        || lower.ends_with(".cjs")
}
