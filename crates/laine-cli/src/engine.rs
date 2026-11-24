use std::path::{Path, PathBuf};

use laine_core::config::WorkspaceConfig;
use laine_core::error::{LaineError, Result};
use laine_core::git::GitRepo;
use laine_core::graph::DependencyGraph;
use laine_core::impact::{ImpactInput, analyze_impact};
use laine_core::model::{AnalysisReport, AnalysisStats, Evidence, UnresolvedCall};
use laine_core::openapi::{diff_contracts, extract_contract};
use laine_ts::extract_file;

#[derive(Debug, Clone)]
pub struct AnalyzeRequest {
    pub config_path: PathBuf,
    pub target_repo: String,
    pub base_rev: String,
    pub head_rev: String,
}

/// Runs a full cross-repository impact analysis without mutating any repository's working tree.
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

    let openapi_rel = target_service.openapi.as_deref().ok_or_else(|| {
        LaineError::ConfigInvalid(format!(
            "service '{}' does not configure an `openapi` spec path in {}",
            req.target_repo,
            req.config_path.display()
        ))
    })?;

    let producer_path = resolve_repo_path(config_dir, &target_service.path);
    let producer_git = GitRepo::new(&producer_path);
    let base_sha = producer_git.resolve_revision(&req.base_rev)?;
    let head_sha = producer_git.resolve_revision(&req.head_rev)?;

    let base_openapi_raw = producer_git.read_file(&base_sha, openapi_rel)?;
    let head_openapi_raw = producer_git.read_file(&head_sha, openapi_rel)?;

    let base_contract = extract_contract(&target_service.name, &base_openapi_raw)?;
    let head_contract = extract_contract(&target_service.name, &head_openapi_raw)?;
    let contract_changes = diff_contracts(&base_contract, &head_contract);

    let mut graph = DependencyGraph::new();
    graph.add_repository(&target_service.name);
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

    let mut files_analyzed = 1usize; // producer OpenAPI spec
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

            let Ok(extracted) = extract_file(&file_path, &source, &config.services) else {
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

    let openapi_display = openapi_rel.to_string_lossy().replace('\\', "/");

    Ok(analyze_impact(
        &graph,
        ImpactInput {
            target_repo: &target_service.name,
            base_rev: &base_sha,
            head_rev: &head_sha,
            openapi_file: &openapi_display,
            base_openapi_raw: &base_openapi_raw,
            changes: contract_changes,
            unresolved_calls,
            stats,
        },
    ))
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
