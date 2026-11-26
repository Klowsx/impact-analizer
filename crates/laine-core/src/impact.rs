use std::collections::{BTreeMap, HashSet};

use crate::graph::DependencyGraph;
use crate::model::{
    AffectedConsumer, AnalysisReport, AnalysisStats, Confidence, ContractChange,
    ContractDiscrepancy, EndpointId, Evidence, HttpMethod, ImpactFinding, IndirectConsumer,
    ProducerEvidence, Severity, UnresolvedCall,
};
use crate::openapi::{ApiContract, canonical_route_key};

pub struct ImpactInput<'a> {
    pub target_repo: &'a str,
    pub base_rev: &'a str,
    pub head_rev: &'a str,
    pub base_source_file: &'a str,
    pub head_source_file: &'a str,
    pub base_contract: Option<&'a ApiContract>,
    pub head_contract: Option<&'a ApiContract>,
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
pub fn analyze_impact(graph: &DependencyGraph, mut input: ImpactInput<'_>) -> AnalysisReport {
    let mut findings = Vec::new();
    for (idx, change) in std::mem::take(&mut input.changes).into_iter().enumerate() {
        let (producer_evidence, producer_evidence_comparison) = producer_sources(&input, &change);
        let consumers = match &change {
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
                let hits = graph.consumers_of_field(
                    &endpoint.service,
                    endpoint.method,
                    &endpoint.normalized_path,
                    field_path,
                );
                hits.into_iter()
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
                    .collect::<Vec<_>>()
            }
            _ => consumers_for_endpoint(graph, change.endpoint()),
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
            producer_evidence,
            producer_evidence_comparison,
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
        policy: None,
        coverage: None,
        diagnostics: Vec::new(),
        exit_code: None,
        workspace: None,
        consumer_revisions: BTreeMap::new(),
        stats: input.stats,
        findings,
        unresolved_calls,
        discrepancies: input.discrepancies,
    }
}

// Selecciona el lado del diff que define el cambio y conserva ambos lados para cambios de tipo.
fn producer_sources(
    input: &ImpactInput<'_>,
    change: &ContractChange,
) -> (ProducerEvidence, Option<ProducerEvidence>) {
    let removed = matches!(
        change,
        ContractChange::EndpointRemoved { .. } | ContractChange::ResponseFieldRemoved { .. }
    );
    let response_type = matches!(change, ContractChange::ResponseFieldTypeChanged { .. });
    let type_changed =
        response_type || matches!(change, ContractChange::RequestFieldTypeChanged { .. });
    let base_first = removed || response_type;
    let evidence = |base| {
        let (contract, revision, file) = if base {
            (input.base_contract, input.base_rev, input.base_source_file)
        } else {
            (input.head_contract, input.head_rev, input.head_source_file)
        };
        let mut result = ProducerEvidence {
            repo: input.target_repo.to_string(),
            revision: revision.to_owned(),
            file: file.to_owned(),
            description: producer_description(change, base),
            ..ProducerEvidence::default()
        };
        if let Some(source) = contract.and_then(|contract| contract.source_for_change(change)) {
            result.json_pointer = Some(source.pointer.clone());
            result.span = source.span.clone();
            result.line = source.span.as_ref().map(|span| span.line);
        } else if matches!(change, ContractChange::EndpointRemoved { .. }) {
            let endpoint = change.endpoint();
            let key = (
                endpoint.service.clone(),
                endpoint.method,
                canonical_route_key(&endpoint.normalized_path),
            );
            if let Some((file, line)) = input.route_locations.get(&key) {
                result.file.clone_from(file);
                result.line = Some(*line);
            }
        }
        result
    };
    (
        evidence(base_first),
        type_changed.then(|| evidence(!base_first)),
    )
}

// Describe la declaración de cada lado sin presentarla como un fragmento de código literal.
fn producer_description(change: &ContractChange, base: bool) -> String {
    match change {
        ContractChange::EndpointRemoved { endpoint } => {
            format!("{} {}", endpoint.method.as_str(), endpoint.normalized_path)
        }
        ContractChange::ResponseFieldRemoved {
            field_path,
            previous_type,
            ..
        } => format!("Response field {field_path} ({previous_type})"),
        ContractChange::ResponseFieldTypeChanged {
            field_path,
            previous_type,
            new_type,
            ..
        }
        | ContractChange::RequestFieldTypeChanged {
            field_path,
            previous_type,
            new_type,
            ..
        } => {
            let ty = if base { previous_type } else { new_type };
            format!("Field {field_path} ({ty})")
        }
        ContractChange::RequestFieldAddedRequired {
            field_path,
            field_type,
            ..
        } => format!("Required request field {field_path} ({field_type})"),
        ContractChange::RequiredParameterAdded {
            location,
            name,
            param_type,
            ..
        } => format!("Required {location} parameter {name} ({param_type})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::HttpMethod;

    // Comprueba que los cambios de request apunten al contrato de head.
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

        let head_contract =
            crate::openapi::extract_contract("customer-service", head_spec).unwrap();
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
                base_source_file: "openapi.yaml",
                head_source_file: "openapi.yaml",
                base_contract: None,
                head_contract: Some(&head_contract),
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
            assert!(finding.producer_evidence.line.unwrap() > 1);
            assert_eq!(finding.producer_evidence.revision, "head111");
        }
        assert_eq!(report.findings[0].producer_evidence.line, Some(14));
        assert_eq!(report.findings[1].producer_evidence.line, Some(16));
        assert_eq!(report.findings[2].producer_evidence.line, Some(6));
        assert_eq!(
            report.findings[1]
                .producer_evidence_comparison
                .as_ref()
                .unwrap()
                .revision,
            "base000"
        );
        let missing = serde_json::to_value(&report.findings[3].producer_evidence).unwrap();
        assert!(missing.get("line").is_none());

        // Finding 3 targets GET /customers which has 0 consumers -> Severity::Medium
        assert_eq!(report.findings[3].severity, Severity::Medium);
        assert!(report.findings[3].consumers.is_empty());
    }
}
