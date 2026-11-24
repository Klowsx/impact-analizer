use crate::graph::DependencyGraph;
use crate::model::{
    AffectedConsumer, AnalysisReport, AnalysisStats, Confidence, ContractChange, Evidence,
    ImpactFinding, Severity, UnresolvedCall,
};
use crate::openapi::find_field_line;

pub struct ImpactInput<'a> {
    pub target_repo: &'a str,
    pub base_rev: &'a str,
    pub head_rev: &'a str,
    pub openapi_file: &'a str,
    pub base_openapi_raw: &'a str,
    pub changes: Vec<ContractChange>,
    pub unresolved_calls: Vec<UnresolvedCall>,
    pub stats: AnalysisStats,
}

/// Crosses breaking contract changes from the producer against the populated dependency graph
/// to produce a deterministic `AnalysisReport` with verifiable evidence.
pub fn analyze_impact(graph: &DependencyGraph, input: ImpactInput<'_>) -> AnalysisReport {
    let mut findings = Vec::new();

    for (idx, change) in input.changes.into_iter().enumerate() {
        let (producer_line, producer_snippet, consumers) = match &change {
            ContractChange::EndpointRemoved { endpoint } => {
                let line = find_field_line(input.base_openapi_raw, &endpoint.normalized_path);
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
                let line = find_field_line(input.base_openapi_raw, field_path);
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

        findings.push(ImpactFinding {
            id: format!("LAINE-{:03}", idx + 1),
            change,
            severity,
            confidence: Confidence::Confirmed,
            producer_evidence: Evidence {
                repo: input.target_repo.to_string(),
                revision: input.head_rev.to_string(),
                file: input.openapi_file.to_string(),
                line: producer_line,
                snippet: producer_snippet,
            },
            consumers,
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
    }
}
