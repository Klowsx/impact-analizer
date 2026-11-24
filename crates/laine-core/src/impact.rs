use std::collections::{BTreeMap, HashSet};

use crate::graph::DependencyGraph;
use crate::model::{
    AffectedConsumer, AnalysisReport, AnalysisStats, Confidence, ContractChange,
    ContractDiscrepancy, Evidence, ImpactFinding, IndirectConsumer, Severity, UnresolvedCall,
};
use crate::openapi::find_field_line;

pub struct ImpactInput<'a> {
    pub target_repo: &'a str,
    pub base_rev: &'a str,
    pub head_rev: &'a str,
    pub source_file: &'a str,
    pub base_source_raw: &'a str,
    pub changes: Vec<ContractChange>,
    pub unresolved_calls: Vec<UnresolvedCall>,
    pub discrepancies: Vec<ContractDiscrepancy>,
    pub stats: AnalysisStats,
}

/// Crosses breaking contract changes from the producer against the populated dependency graph
/// to produce a deterministic `AnalysisReport` with verifiable evidence.
pub fn analyze_impact(graph: &DependencyGraph, input: ImpactInput<'_>) -> AnalysisReport {
    let mut findings = Vec::new();

    for (idx, change) in input.changes.into_iter().enumerate() {
        let (producer_line, producer_snippet, consumers) = match &change {
            ContractChange::EndpointRemoved { endpoint } => {
                let line = find_field_line(input.base_source_raw, &endpoint.normalized_path);
                let hits = graph.consumers_of_endpoint(
                    &endpoint.service,
                    endpoint.method,
                    &endpoint.normalized_path,
                );
                let consumers = hits
                    .into_iter()
                    .map(|hit| AffectedConsumer {
                        repo: hit.repo.clone(),
                        evidence: Evidence {
                            repo: hit.repo.clone(),
                            revision: hit.revision,
                            file: hit.file.clone(),
                            line: hit.line,
                            snippet: hit.snippet,
                        },
                        confidence: Confidence::Confirmed,
                        dependency_path: vec![
                            format!("{} {}", endpoint.method.as_str(), endpoint.normalized_path),
                            format!("HTTP call @ {}/{}:{}", hit.repo, hit.file, hit.line),
                        ],
                    })
                    .collect::<Vec<_>>();
                (
                    line,
                    format!(
                        "{} {} (removed)",
                        endpoint.method.as_str(),
                        endpoint.normalized_path
                    ),
                    consumers,
                )
            }
            ContractChange::ResponseFieldRemoved {
                endpoint,
                field_path,
                ..
            }
            | ContractChange::ResponseFieldTypeChanged {
                endpoint,
                field_path,
                ..
            } => {
                let line = find_field_line(input.base_source_raw, field_path);
                let hits = graph.consumers_of_field(
                    &endpoint.service,
                    endpoint.method,
                    &endpoint.normalized_path,
                    field_path,
                );
                let consumers = hits
                    .into_iter()
                    .map(|hit| AffectedConsumer {
                        repo: hit.repo.clone(),
                        evidence: Evidence {
                            repo: hit.repo.clone(),
                            revision: hit.revision,
                            file: hit.file.clone(),
                            line: hit.line,
                            snippet: hit.snippet,
                        },
                        confidence: Confidence::Confirmed,
                        dependency_path: vec![
                            format!("{} {}", endpoint.method.as_str(), endpoint.normalized_path),
                            format!("HTTP call @ {}/{}:{}", hit.repo, hit.file, hit.call_line),
                            format!(
                                "read `{}` @ {}/{}:{}",
                                hit.matched_field, hit.repo, hit.file, hit.line
                            ),
                        ],
                    })
                    .collect::<Vec<_>>();
                (line, format!("field `{field_path}`"), consumers)
            }
        };

        let severity = match (&change, consumers.is_empty()) {
            (ContractChange::EndpointRemoved { .. }, false) => Severity::Critical,
            (_, false) => Severity::High,
            (_, true) => Severity::Medium,
        };

        let direct_repo_set: HashSet<String> = consumers.iter().map(|c| c.repo.clone()).collect();
        let mut indirect_map: BTreeMap<String, IndirectConsumer> = BTreeMap::new();
        for direct_repo in &direct_repo_set {
            for (downstream_repo, chain) in graph.transitive_dependent_paths(direct_repo) {
                if downstream_repo == input.target_repo
                    || direct_repo_set.contains(&downstream_repo)
                {
                    continue;
                }
                indirect_map
                    .entry(downstream_repo.clone())
                    .or_insert_with(|| IndirectConsumer {
                        repo: downstream_repo,
                        confidence: Confidence::Possible,
                        via_repo: direct_repo.clone(),
                        dependency_chain: chain,
                    });
            }
        }
        let indirect_consumers: Vec<IndirectConsumer> = indirect_map.into_values().collect();

        findings.push(ImpactFinding {
            id: format!("LAINE-{:03}", idx + 1),
            change,
            severity,
            confidence: Confidence::Confirmed,
            producer_evidence: Evidence {
                repo: input.target_repo.to_string(),
                revision: input.head_rev.to_string(),
                file: input.source_file.to_string(),
                line: producer_line,
                snippet: producer_snippet,
            },
            consumers,
            indirect_consumers,
        });
    }

    let mut unresolved_calls = input.unresolved_calls;
    unresolved_calls.sort_by(|a, b| {
        (&a.repo, &a.evidence.file, a.evidence.line, &a.reason).cmp(&(
            &b.repo,
            &b.evidence.file,
            b.evidence.line,
            &b.reason,
        ))
    });

    AnalysisReport {
        schema_version: 1,
        target_repo: input.target_repo.to_string(),
        base_rev: input.base_rev.to_string(),
        head_rev: input.head_rev.to_string(),
        stats: input.stats,
        findings,
        unresolved_calls,
        discrepancies: input.discrepancies,
    }
}
