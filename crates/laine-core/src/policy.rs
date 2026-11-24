use crate::config::PolicyConfig;
use crate::graph::routes_match;
use crate::model::{AnalysisReport, ContractChange, EndpointId, HttpMethod};
use crate::openapi::canonical_route_key;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEvaluation {
    pub should_fail: bool,
    pub blocking_findings: usize,
    pub reason: String,
}

/// Removes any finding whose endpoint matches an entry in `policy.ignore_endpoints`.
///
/// Entries may be:
/// - Exact or wildcard paths matched against canonical route keys via `routes_match`
///   (e.g. `"/health"`, `"/internal/*"`, `"/customers/:id"`).
/// - Recursive prefix globs ending in `/**` (e.g. `"/internal/**"`).
/// - Optional HTTP method prefix followed by whitespace (e.g. `"GET /health"`).
pub fn filter_ignored_findings(report: &mut AnalysisReport, policy: &PolicyConfig) {
    if policy.ignore_endpoints.is_empty() {
        return;
    }

    report.findings.retain(|finding| {
        let endpoint = change_endpoint(&finding.change);
        !policy
            .ignore_endpoints
            .iter()
            .any(|pattern| endpoint_matches_ignore_pattern(endpoint, pattern))
    });
}

/// Evaluates whether an `AnalysisReport` violates `policy` and should fail the CLI / CI check.
///
/// Fails when:
/// - Any finding has `finding.severity >= policy.fail_on`, OR
/// - `policy.fail_on_unresolved` is enabled and `report.unresolved_calls` is non-empty.
pub fn evaluate_policy(report: &AnalysisReport, policy: &PolicyConfig) -> PolicyEvaluation {
    let blocking_findings = report
        .findings
        .iter()
        .filter(|finding| finding.severity >= policy.fail_on)
        .count();

    let unresolved_count = report.unresolved_calls.len();
    let unresolved_blocks = policy.fail_on_unresolved && unresolved_count > 0;
    let should_fail = blocking_findings > 0 || unresolved_blocks;

    let reason = match (blocking_findings > 0, unresolved_blocks) {
        (true, true) => format!(
            "{blocking_findings} finding(s) meet or exceed fail_on threshold ({:?}) and {unresolved_count} unresolved call(s) blocked by fail_on_unresolved",
            policy.fail_on
        ),
        (true, false) => format!(
            "{blocking_findings} finding(s) meet or exceed fail_on threshold ({:?})",
            policy.fail_on
        ),
        (false, true) => {
            format!(
                "{unresolved_count} unresolved call(s) detected with fail_on_unresolved enabled"
            )
        }
        (false, false) => "No policy violations detected".to_string(),
    };

    PolicyEvaluation {
        should_fail,
        blocking_findings,
        reason,
    }
}

fn change_endpoint(change: &ContractChange) -> &EndpointId {
    match change {
        ContractChange::EndpointRemoved { endpoint }
        | ContractChange::ResponseFieldRemoved { endpoint, .. }
        | ContractChange::ResponseFieldTypeChanged { endpoint, .. }
        | ContractChange::RequestFieldAddedRequired { endpoint, .. }
        | ContractChange::RequestFieldTypeChanged { endpoint, .. }
        | ContractChange::RequiredParameterAdded { endpoint, .. } => endpoint,
    }
}

fn endpoint_matches_ignore_pattern(endpoint: &EndpointId, raw_pattern: &str) -> bool {
    let trimmed = raw_pattern.trim();
    if trimmed.is_empty() {
        return false;
    }

    let path_pattern = {
        let mut parts = trimmed.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some(method_token), Some(path_token), None) => {
                if let Some(expected_method) = HttpMethod::parse(method_token) {
                    if endpoint.method != expected_method {
                        return false;
                    }
                    path_token
                } else {
                    trimmed
                }
            }
            _ => trimmed,
        }
    };

    let endpoint_canonical = canonical_route_key(&endpoint.normalized_path);

    if let Some(prefix_raw) = path_pattern.strip_suffix("/**") {
        let prefix_canonical = canonical_route_key(prefix_raw);
        if prefix_canonical == "/" {
            return true;
        }
        return endpoint_canonical == prefix_canonical
            || endpoint_canonical.starts_with(&format!("{prefix_canonical}/"));
    }

    let pattern_canonical = canonical_route_key(path_pattern);
    routes_match(&endpoint_canonical, &pattern_canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AnalysisStats, Confidence, Evidence, ImpactFinding, Severity, UnresolvedCall,
    };

    fn mk_finding(id: &str, method: HttpMethod, path: &str, severity: Severity) -> ImpactFinding {
        ImpactFinding {
            id: id.into(),
            change: ContractChange::EndpointRemoved {
                endpoint: EndpointId {
                    service: "customer-service".into(),
                    method,
                    normalized_path: path.into(),
                },
            },
            severity,
            confidence: Confidence::Confirmed,
            producer_evidence: Evidence {
                repo: "customer-service".into(),
                revision: "abc1234".into(),
                file: "openapi.yaml".into(),
                line: 10,
                snippet: format!("{} {path}", method.as_str()),
            },
            consumers: Vec::new(),
            indirect_consumers: Vec::new(),
        }
    }

    fn mk_report(
        findings: Vec<ImpactFinding>,
        unresolved_calls: Vec<UnresolvedCall>,
    ) -> AnalysisReport {
        AnalysisReport {
            schema_version: 1,
            target_repo: "customer-service".into(),
            base_rev: "base".into(),
            head_rev: "head".into(),
            stats: AnalysisStats::default(),
            findings,
            unresolved_calls,
            discrepancies: Vec::new(),
        }
    }

    #[test]
    fn filters_ignored_endpoints_using_exact_wildcard_and_method_patterns() {
        let mut report = mk_report(
            vec![
                mk_finding("LAINE-001", HttpMethod::Get, "/health", Severity::Medium),
                mk_finding(
                    "LAINE-002",
                    HttpMethod::Get,
                    "/internal/metrics",
                    Severity::High,
                ),
                mk_finding(
                    "LAINE-003",
                    HttpMethod::Post,
                    "/internal/v1/debug",
                    Severity::High,
                ),
                mk_finding(
                    "LAINE-004",
                    HttpMethod::Get,
                    "/customers/:id",
                    Severity::Critical,
                ),
                mk_finding(
                    "LAINE-005",
                    HttpMethod::Post,
                    "/customers/:id",
                    Severity::Critical,
                ),
            ],
            Vec::new(),
        );

        let policy = PolicyConfig {
            fail_on: Severity::Low,
            ignore_endpoints: vec![
                "/health".into(),
                "/internal/*".into(),
                "/internal/**".into(),
                "GET /customers/:customerId".into(),
            ],
            fail_on_unresolved: false,
        };

        filter_ignored_findings(&mut report, &policy);
        let remaining_ids: Vec<&str> = report.findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(remaining_ids, vec!["LAINE-005"]);
    }

    #[test]
    fn evaluates_fail_on_severity_thresholds() {
        let report = mk_report(
            vec![
                mk_finding("LAINE-001", HttpMethod::Get, "/orders", Severity::Medium),
                mk_finding("LAINE-002", HttpMethod::Get, "/customers", Severity::High),
            ],
            Vec::new(),
        );

        // fail_on = Critical allows High and Medium to pass
        let critical_policy = PolicyConfig {
            fail_on: Severity::Critical,
            ignore_endpoints: Vec::new(),
            fail_on_unresolved: false,
        };
        let eval_critical = evaluate_policy(&report, &critical_policy);
        assert!(!eval_critical.should_fail);
        assert_eq!(eval_critical.blocking_findings, 0);

        // fail_on = High blocks only the High finding
        let high_policy = PolicyConfig {
            fail_on: Severity::High,
            ignore_endpoints: Vec::new(),
            fail_on_unresolved: false,
        };
        let eval_high = evaluate_policy(&report, &high_policy);
        assert!(eval_high.should_fail);
        assert_eq!(eval_high.blocking_findings, 1);

        // Default fail_on = Low blocks all findings
        let eval_default = evaluate_policy(&report, &PolicyConfig::default());
        assert!(eval_default.should_fail);
        assert_eq!(eval_default.blocking_findings, 2);
    }

    #[test]
    fn evaluates_fail_on_unresolved_policy() {
        let report = mk_report(
            Vec::new(),
            vec![UnresolvedCall {
                repo: "billing-service".into(),
                evidence: Evidence {
                    repo: "billing-service".into(),
                    revision: "abc1234".into(),
                    file: "src/sync.ts".into(),
                    line: 8,
                    snippet: "fetch(dynamicUrl)".into(),
                },
                reason: "dynamic URL expression".into(),
            }],
        );

        let permissive = PolicyConfig::default();
        assert!(!evaluate_policy(&report, &permissive).should_fail);

        let strict = PolicyConfig {
            fail_on: Severity::Low,
            ignore_endpoints: Vec::new(),
            fail_on_unresolved: true,
        };
        let eval = evaluate_policy(&report, &strict);
        assert!(eval.should_fail);
        assert_eq!(eval.blocking_findings, 0);
        assert!(eval.reason.contains("unresolved"));
    }
}
