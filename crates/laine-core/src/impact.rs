use std::collections::{BTreeMap, HashSet};

use crate::graph::DependencyGraph;
use crate::model::{
    AffectedConsumer, AnalysisReport, AnalysisStats, Confidence, ContractChange,
    ContractDiscrepancy, EndpointId, Evidence, HttpMethod, ImpactFinding, IndirectConsumer,
    ProducerEvidence, Severity, UnresolvedCall,
};
use crate::openapi::{ApiContract, canonical_route_key};
use crate::request::{ArgumentCompatibility, HttpRequestEvidence};

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

/// Collect call locations and traces for an endpoint, retaining their resolution confidence.
pub fn consumers_for_endpoint(
    graph: &DependencyGraph,
    endpoint: &EndpointId,
) -> Vec<AffectedConsumer> {
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

// Identify the changed argument and compare it against the corresponding request section.
fn request_compatibility<'a>(
    change: &'a ContractChange,
    request: &HttpRequestEvidence,
    line: usize,
) -> (&'a str, &'a str, ArgumentCompatibility) {
    let (section, field, required, expected) = match change {
        ContractChange::RequestFieldAddedRequired {
            field_path,
            field_type,
            ..
        } => ("body", Some(field_path.as_str()), true, field_type),
        ContractChange::RequestFieldTypeChanged {
            field_path,
            new_type,
            ..
        } => ("body", Some(field_path.as_str()), false, new_type),
        ContractChange::RequiredParameterAdded {
            location,
            name,
            param_type,
            ..
        } => (location.as_str(), Some(name.as_str()), true, param_type),
        ContractChange::RequestParameterTypeChanged {
            location,
            name,
            new_type,
            ..
        } => (location.as_str(), Some(name.as_str()), false, new_type),
        ContractChange::BodyTypeChanged {
            status_code: None,
            new_type,
            ..
        } => ("body", None, false, new_type),
        _ => unreachable!("only request changes have argument evidence"),
    };
    let result = match (section, field) {
        ("body", field) => request.body.compare(field, required, expected, line),
        ("query", Some(name)) => request.query.compare_query(name, required, expected, line),
        ("header", Some(name)) => {
            request
                .headers
                .compare(Some(&name.to_ascii_lowercase()), required, expected, line)
        }
        _ => ArgumentCompatibility::Unknown(None),
    };
    (section, field.unwrap_or("<root>"), result)
}

// Omit compatible calls and cap confidence when the argument is unverified.
fn consumers_for_request_change(
    graph: &DependencyGraph,
    change: &ContractChange,
) -> Vec<AffectedConsumer> {
    let endpoint = change.endpoint();
    let mut affected = BTreeMap::new();
    let empty = HttpRequestEvidence::default();
    for (hit, request) in graph.endpoint_call_hits(
        &endpoint.service,
        endpoint.method,
        &endpoint.normalized_path,
    ) {
        let request = request.unwrap_or(&empty);
        let (section, name, result) = request_compatibility(change, request, hit.line);
        let (line, confidence, status) = match result {
            ArgumentCompatibility::Compatible => continue,
            ArgumentCompatibility::Incompatible(line) => (line, hit.confidence, "Incompatible"),
            ArgumentCompatibility::Unknown(line) => (
                line.unwrap_or(hit.line),
                hit.confidence.min(Confidence::Possible),
                "Unverified",
            ),
        };
        let label = format!("{status} HTTP {section} argument {name:?}");
        let consumer = AffectedConsumer {
            repo: hit.repo.clone(),
            evidence: Evidence {
                repo: hit.repo.clone(),
                revision: hit.revision,
                file: hit.file.clone(),
                line,
                snippet: label.clone(),
            },
            confidence,
            dependency_path: vec![
                format!("{} {}", endpoint.method.as_str(), endpoint.normalized_path),
                format!("HTTP call @ {}/{}:{}", hit.repo, hit.file, hit.line),
                format!("{label} @ {}/{}:{line}", hit.repo, hit.file),
            ],
        };
        let key = (hit.repo, hit.file, line, hit.line);
        if affected
            .get(&key)
            .is_none_or(|previous: &AffectedConsumer| previous.confidence < confidence)
        {
            affected.insert(key, consumer);
        }
    }
    affected.into_values().collect()
}

/// Collect response reads and preserve their HTTP call and dataflow traces.
pub fn consumers_for_field(
    graph: &DependencyGraph,
    endpoint: &EndpointId,
    field_path: &str,
) -> Vec<AffectedConsumer> {
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
                    format!("{} {}", endpoint.method.as_str(), endpoint.normalized_path),
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

/// Find indirect dependents with the shortest deterministic chain from a direct consumer.
pub fn compute_indirect_consumers(
    graph: &DependencyGraph,
    target_repo: &str,
    direct_repos: &HashSet<String>,
) -> Vec<IndirectConsumer> {
    let mut indirect_paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for direct_repo in direct_repos {
        for (downstream_repo, chain) in graph.transitive_dependent_paths(direct_repo) {
            if downstream_repo == target_repo || direct_repos.contains(&downstream_repo) {
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
    indirect_paths
        .into_iter()
        .map(|(repo, dependency_chain)| IndirectConsumer {
            repo,
            confidence: Confidence::Possible,
            via_repo: dependency_chain[0].clone(),
            dependency_chain,
        })
        .collect()
}

/// Match contract changes against the graph to produce a reproducible report.
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
            }
            | ContractChange::ResponseFieldMadeOptional {
                endpoint,
                field_path,
                ..
            } => consumers_for_field(graph, endpoint, field_path),
            ContractChange::RequestFieldAddedRequired { .. }
            | ContractChange::RequestFieldTypeChanged { .. }
            | ContractChange::RequiredParameterAdded { .. }
            | ContractChange::RequestParameterTypeChanged { .. }
            | ContractChange::BodyTypeChanged {
                status_code: None, ..
            } => consumers_for_request_change(graph, &change),
            ContractChange::BodyTypeChanged {
                status_code: Some(_),
                ..
            } => consumers_for_endpoint(graph, change.endpoint())
                .into_iter()
                .map(|mut consumer| {
                    consumer.confidence = consumer.confidence.min(Confidence::Possible);
                    consumer
                })
                .collect(),
            ContractChange::EndpointRemoved { endpoint } => consumers_for_endpoint(graph, endpoint),
        };

        let severity = match (&change, consumers.is_empty()) {
            (ContractChange::EndpointRemoved { .. }, false) => Severity::Critical,
            (_, false) => Severity::High,
            (_, true) => Severity::Medium,
        };

        let direct_repo_set: HashSet<String> = consumers.iter().map(|c| c.repo.clone()).collect();
        let indirect_consumers =
            compute_indirect_consumers(graph, input.target_repo, &direct_repo_set);

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
    let response_type = matches!(
        change,
        ContractChange::ResponseFieldTypeChanged { .. }
            | ContractChange::BodyTypeChanged {
                status_code: Some(_),
                ..
            }
    );
    let type_changed = response_type
        || matches!(
            change,
            ContractChange::RequestFieldTypeChanged { .. }
                | ContractChange::RequestParameterTypeChanged { .. }
                | ContractChange::BodyTypeChanged { .. }
                | ContractChange::ResponseFieldMadeOptional { .. }
        );
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
            if !source.file.is_empty() {
                result.file.clone_from(&source.file);
            }
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
        ContractChange::ResponseFieldMadeOptional { field_path, .. } => {
            let presence = if base { "required" } else { "optional" };
            format!("Response field {field_path} ({presence})")
        }
        ContractChange::BodyTypeChanged {
            status_code,
            previous_type,
            new_type,
            ..
        } => {
            let ty = if base { previous_type } else { new_type };
            let body = if status_code.is_some() {
                "Response"
            } else {
                "Request"
            };
            format!("{body} body ({ty})")
        }
        ContractChange::RequestParameterTypeChanged {
            location,
            name,
            previous_type,
            new_type,
            ..
        } => {
            let ty = if base { previous_type } else { new_type };
            format!("{location} parameter {name} ({ty})")
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

    // Check producer lines and keep callers without argument evidence possible.
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
            assert_eq!(finding.consumers[0].confidence, Confidence::Possible);
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
