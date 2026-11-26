use std::collections::{BTreeMap, HashSet};

use crate::graph::DependencyGraph;
use crate::model::{
    AffectedConsumer, AnalysisReport, AnalysisStats, Confidence, ContractChange,
    ContractDiscrepancy, EndpointId, Evidence, HttpMethod, ImpactFinding, IndirectConsumer,
    Severity, UnresolvedCall,
};
use crate::openapi::{canonical_route_key, find_field_line, find_parameter_line};

pub struct ImpactInput<'a> {
    pub target_repo: &'a str,
    pub base_rev: &'a str,
    pub head_rev: &'a str,
    pub source_file: &'a str,
    pub base_source_raw: &'a str,
    pub head_source_raw: &'a str,
    /// Optional exact `(file, line)` source locations for code-extracted routes keyed by
    /// `(service, method, canonical_route_key)`.
    pub route_locations: BTreeMap<(String, HttpMethod, String), (String, usize)>,
    pub changes: Vec<ContractChange>,
    pub unresolved_calls: Vec<UnresolvedCall>,
    pub discrepancies: Vec<ContractDiscrepancy>,
    pub stats: AnalysisStats,
}

// Traslada al hallazgo las llamadas de la operación con su confianza de resolución.
fn consumers_for_endpoint(graph: &DependencyGraph, endpoint: &EndpointId) -> Vec<AffectedConsumer> {
    graph
        .consumers_of_endpoint(
            &endpoint.service,
            endpoint.method,
            &endpoint.normalized_path,
        )
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
            confidence: hit.confidence,
            dependency_path: vec![
                format!("{} {}", endpoint.method.as_str(), endpoint.normalized_path),
                format!("HTTP call @ {}/{}:{}", hit.repo, hit.file, hit.line),
            ],
        })
        .collect()
}

/// Cruza los cambios de contrato con el grafo para producir un informe reproducible.
pub fn analyze_impact(graph: &DependencyGraph, input: ImpactInput<'_>) -> AnalysisReport {
    let mut findings = Vec::new();
    let head_or_base_raw = if input.head_source_raw.is_empty() {
        input.base_source_raw
    } else {
        input.head_source_raw
    };

    for (idx, change) in input.changes.into_iter().enumerate() {
        let mut producer_file = input.source_file.to_string();
        let (producer_line, producer_snippet, consumers) = match &change {
            ContractChange::EndpointRemoved { endpoint } => {
                let key = (
                    endpoint.service.clone(),
                    endpoint.method,
                    canonical_route_key(&endpoint.normalized_path),
                );
                let line = if let Some((file, exact_line)) = input.route_locations.get(&key) {
                    producer_file = file.clone();
                    *exact_line
                } else {
                    find_field_line(input.base_source_raw, &endpoint.normalized_path)
                };
                let consumers = consumers_for_endpoint(graph, endpoint);
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
                    .map(|hit| {
                        let step_label = if hit.source_confidence == Confidence::Likely {
                            "typed contract"
                        } else {
                            "read"
                        };
                        let dependency_path = if !hit.custom_trace.is_empty() {
                            hit.custom_trace
                        } else {
                            vec![
                                format!(
                                    "{} {}",
                                    endpoint.method.as_str(),
                                    endpoint.normalized_path
                                ),
                                format!(
                                    "HTTP call @ {}/{}:{}",
                                    hit.repo, hit.call_file, hit.call_line
                                ),
                                format!(
                                    "{step_label} `{}` @ {}/{}:{}",
                                    hit.matched_field, hit.repo, hit.file, hit.line
                                ),
                            ]
                        };
                        AffectedConsumer {
                            repo: hit.repo.clone(),
                            evidence: Evidence {
                                repo: hit.repo.clone(),
                                revision: hit.revision,
                                file: hit.file.clone(),
                                line: hit.line,
                                snippet: hit.snippet,
                            },
                            confidence: hit.confidence,
                            dependency_path,
                        }
                    })
                    .collect::<Vec<_>>();
                (line, format!("field `{field_path}`"), consumers)
            }
            ContractChange::RequestFieldAddedRequired {
                endpoint,
                field_path,
                field_type,
            } => {
                let line = find_field_line(head_or_base_raw, field_path);
                let consumers = consumers_for_endpoint(graph, endpoint);
                (
                    line,
                    format!("required request field `{field_path}` ({field_type})"),
                    consumers,
                )
            }
            ContractChange::RequestFieldTypeChanged {
                endpoint,
                field_path,
                previous_type,
                new_type,
            } => {
                let line = find_field_line(head_or_base_raw, field_path);
                let consumers = consumers_for_endpoint(graph, endpoint);
                (
                    line,
                    format!("request field `{field_path}` ({previous_type} -> {new_type})"),
                    consumers,
                )
            }
            ContractChange::RequiredParameterAdded {
                endpoint,
                location,
                name,
                param_type,
            } => {
                let line = find_parameter_line(head_or_base_raw, name);
                let consumers = consumers_for_endpoint(graph, endpoint);
                (
                    line,
                    format!("required {location} parameter `{name}` ({param_type})"),
                    consumers,
                )
            }
        };

        let severity = match (&change, consumers.is_empty()) {
            (ContractChange::EndpointRemoved { .. }, false) => Severity::Critical,
            (_, false) => Severity::High,
            (_, true) => Severity::Medium,
        };

        let direct_repo_set: HashSet<String> = consumers.iter().map(|c| c.repo.clone()).collect();
        let mut indirect_paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for direct_repo in &direct_repo_set {
            for (downstream_repo, chain) in graph.transitive_dependent_paths(direct_repo) {
                if downstream_repo == input.target_repo
                    || direct_repo_set.contains(&downstream_repo)
                {
                    continue;
                }
                // Prioriza la cadena más corta y desempata por orden lexicográfico.
                if indirect_paths
                    .get(&downstream_repo)
                    .is_none_or(|previous| (chain.len(), &chain) < (previous.len(), previous))
                {
                    indirect_paths.insert(downstream_repo, chain);
                }
            }
        }
        let indirect_consumers = indirect_paths
            .into_iter()
            .map(|(repo, dependency_chain)| IndirectConsumer {
                repo,
                confidence: Confidence::Possible,
                via_repo: dependency_chain[0].clone(),
                dependency_chain,
            })
            .collect();

        findings.push(ImpactFinding {
            id: format!("LAINE-{:03}", idx + 1),
            change,
            severity,
            confidence: Confidence::Confirmed,
            producer_evidence: Evidence {
                repo: input.target_repo.to_string(),
                revision: input.head_rev.to_string(),
                file: producer_file,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::HttpMethod;

    #[test]
    fn request_side_changes_map_to_endpoint_consumers_and_head_lines() {
        let mut graph = DependencyGraph::new();
        graph.add_repository("customer-service");
        graph.add_repository("checkout-web");
        graph.add_endpoint("customer-service", HttpMethod::Post, "/customers");
        graph.add_endpoint("customer-service", HttpMethod::Get, "/customers");
        graph.link_consumes(
            "checkout-web",
            "src/api.ts",
            "abc1234",
            "customer-service",
            HttpMethod::Post,
            "/customers",
            42,
            "fetch('/customers', { method: 'POST' })",
        );

        let head_spec = "\
openapi: 3.0.3
paths:
  /customers:
    post:
      parameters:
        - name: X-Tenant-Id
          in: header
          required: true
      requestBody:
        content:
          application/json:
            schema:
              properties:
                taxId:
                  type: string
                age:
                  type: string
";

        let post_ep = EndpointId {
            service: "customer-service".into(),
            method: HttpMethod::Post,
            normalized_path: "/customers".into(),
        };
        let get_ep = EndpointId {
            service: "customer-service".into(),
            method: HttpMethod::Get,
            normalized_path: "/customers".into(),
        };

        let report = analyze_impact(
            &graph,
            ImpactInput {
                target_repo: "customer-service",
                base_rev: "base000",
                head_rev: "head111",
                source_file: "openapi.yaml",
                base_source_raw: "",
                head_source_raw: head_spec,
                route_locations: BTreeMap::new(),
                changes: vec![
                    ContractChange::RequestFieldAddedRequired {
                        endpoint: post_ep.clone(),
                        field_path: "taxId".into(),
                        field_type: "string".into(),
                    },
                    ContractChange::RequestFieldTypeChanged {
                        endpoint: post_ep.clone(),
                        field_path: "age".into(),
                        previous_type: "integer".into(),
                        new_type: "string".into(),
                    },
                    ContractChange::RequiredParameterAdded {
                        endpoint: post_ep,
                        location: "header".into(),
                        name: "X-Tenant-Id".into(),
                        param_type: "string".into(),
                    },
                    ContractChange::RequiredParameterAdded {
                        endpoint: get_ep,
                        location: "query".into(),
                        name: "tenantId".into(),
                        param_type: "string".into(),
                    },
                ],
                unresolved_calls: Vec::new(),
                discrepancies: Vec::new(),
                stats: AnalysisStats::default(),
            },
        );

        assert_eq!(report.findings.len(), 4);

        // Findings 0, 1, 2 target POST /customers which has 1 consumer -> Severity::High
        for finding in &report.findings[0..3] {
            assert_eq!(finding.severity, Severity::High);
            assert_eq!(finding.consumers.len(), 1);
            assert_eq!(finding.consumers[0].repo, "checkout-web");
            assert!(finding.producer_evidence.line > 1);
        }
        assert_eq!(report.findings[0].producer_evidence.line, 14);
        assert_eq!(report.findings[1].producer_evidence.line, 16);
        assert_eq!(report.findings[2].producer_evidence.line, 6);

        // Finding 3 targets GET /customers which has 0 consumers -> Severity::Medium
        assert_eq!(report.findings[3].severity, Severity::Medium);
        assert!(report.findings[3].consumers.is_empty());
    }
}
