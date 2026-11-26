use std::fmt::Write;

use laine_core::model::{
    AnalysisCoverage, AnalysisReport, Confidence, ContractChange, ExtractionDiagnostic,
    ProducerEvidence, Severity,
};

// Quita las descripciones de evidencia conservando sus ubicaciones y el contrato JSON.
pub fn omit_snippets(report: &mut AnalysisReport) {
    for finding in &mut report.findings {
        finding.producer_evidence.snippet.clear();
        finding.producer_evidence.description.clear();
        if let Some(comparison) = finding.producer_evidence_comparison.as_mut() {
            comparison.snippet.clear();
            comparison.description.clear();
        }
        for consumer in &mut finding.consumers {
            consumer.evidence.snippet.clear();
        }
    }
    for call in &mut report.unresolved_calls {
        call.evidence.snippet.clear();
    }
}

// Muestra evidencia, cobertura y decisión de política del análisis.
// Presenta hallazgos y cobertura con las mismas descripciones que el informe Markdown.
pub fn render_terminal(report: &AnalysisReport, elapsed_ms: u128) -> String {
    let mut out = String::new();

    let base_short = short_sha(&report.base_rev);
    let head_short = short_sha(&report.head_rev);

    let _ = writeln!(
        out,
        "Laine v{} — Cross-Repo Impact Analysis",
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(
        out,
        "Target: {} ({base_short} -> {head_short})",
        report.target_repo
    );
    let _ = writeln!(
        out,
        "Analyzed: {} repos | {} files | {} nodes | {} edges | {} ms",
        report.stats.repositories,
        report.stats.files_analyzed,
        report.stats.graph_nodes,
        report.stats.graph_edges,
        elapsed_ms
    );
    let _ = writeln!(out);

    if let Some(coverage) = &report.coverage {
        let _ = writeln!(out, "Coverage: {}", describe_coverage(coverage));
        for diagnostic in &report.diagnostics {
            let _ = writeln!(out, "  - {}", describe_extraction_diagnostic(diagnostic));
        }
        let _ = writeln!(out);
    }

    if report.findings.is_empty() {
        let _ = writeln!(out, "{}", empty_findings_message(report));
    } else {
        for finding in &report.findings {
            let _ = writeln!(
                out,
                "BREAKING CHANGE [{:?} | {:?}] ({})",
                finding.confidence, finding.severity, finding.id
            );
            let endpoint = finding.change.endpoint();
            let _ = writeln!(
                out,
                "  Endpoint: {} {}",
                endpoint.method.as_str(),
                endpoint.normalized_path
            );
            let _ = writeln!(out, "  Change:   {}", describe_change(&finding.change));

            let _ = writeln!(
                out,
                "  Source:   {} (revision {})",
                producer_location(&finding.producer_evidence),
                short_sha(&finding.producer_evidence.revision)
            );
            if let Some(comparison) = &finding.producer_evidence_comparison {
                let _ = writeln!(
                    out,
                    "  Compared: {} (revision {})",
                    producer_location(comparison),
                    short_sha(&comparison.revision)
                );
            }
            let _ = writeln!(out);

            if finding.consumers.is_empty() {
                let _ = writeln!(out, "  Affected consumers: 0 detected");
            } else {
                let _ = writeln!(out, "  Affected consumers ({}):", finding.consumers.len());
                for (i, consumer) in finding.consumers.iter().enumerate() {
                    let _ = writeln!(
                        out,
                        "    {}. {} [{:?}]",
                        i + 1,
                        consumer.repo,
                        consumer.confidence
                    );
                    let _ = writeln!(
                        out,
                        "       File: {}/{}:{} (commit {})",
                        consumer.evidence.repo,
                        consumer.evidence.file,
                        consumer.evidence.line,
                        short_sha(&consumer.evidence.revision)
                    );
                    if !consumer.evidence.snippet.is_empty() {
                        let _ = writeln!(out, "       Evidence: {}", consumer.evidence.snippet);
                    }
                    let _ = writeln!(
                        out,
                        "       Path: {}",
                        consumer.dependency_path.join(" -> ")
                    );
                }
            }
            if !finding.indirect_consumers.is_empty() {
                let _ = writeln!(
                    out,
                    "  Indirectly affected consumers ({} possible):",
                    finding.indirect_consumers.len()
                );
                for indirect in &finding.indirect_consumers {
                    let _ = writeln!(
                        out,
                        "    - {} [{:?}] via {} ({})",
                        indirect.repo,
                        indirect.confidence,
                        indirect.via_repo,
                        indirect.dependency_chain.join(" -> ")
                    );
                }
            }
            let _ = writeln!(out);
        }
    }

    if !report.discrepancies.is_empty() {
        let _ = writeln!(
            out,
            "CONTRACT DISCREPANCIES ({})",
            report.discrepancies.len()
        );
        for d in &report.discrepancies {
            let _ = writeln!(
                out,
                "  - [{:?}] {} {} — {}",
                d.kind,
                d.endpoint.method.as_str(),
                d.endpoint.normalized_path,
                d.detail
            );
        }
        let _ = writeln!(out);
    }

    if !report.unresolved_calls.is_empty() {
        let _ = writeln!(
            out,
            "UNRESOLVED CALLS [UNKNOWN] ({})",
            report.unresolved_calls.len()
        );
        for call in &report.unresolved_calls {
            let _ = writeln!(
                out,
                "  - {}/{}:{} ({})",
                call.evidence.repo, call.evidence.file, call.evidence.line, call.reason
            );
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "RESULT: {}", describe_policy(report));

    out
}

pub fn render_json(report: &AnalysisReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

/// Renders a GitHub-flavored Markdown report designed for pull request comments.
// Genera un reporte Markdown con la política, la cobertura y los hallazgos.
pub fn render_markdown(report: &AnalysisReport) -> String {
    let mut out = String::new();

    let base_short = short_sha(&report.base_rev);
    let head_short = short_sha(&report.head_rev);
    let direct_consumers: usize = report.findings.iter().map(|f| f.consumers.len()).sum();
    let indirect_consumers: usize = report
        .findings
        .iter()
        .map(|f| f.indirect_consumers.len())
        .sum();

    let _ = writeln!(out, "## Laine Impact Analysis");
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "**Policy:** {} | **Target:** `{}` (`{base_short}` -> `{head_short}`)",
        describe_policy(report),
        report.target_repo,
    );
    let _ = writeln!(
        out,
        "**Summary:** {} breaking change(s) | {} direct consumer ref(s) | {} indirect consumer(s) | {} unresolved call(s)",
        report.findings.len(),
        direct_consumers,
        indirect_consumers,
        report.unresolved_calls.len()
    );
    let _ = writeln!(
        out,
        "**Analyzed:** {} repos | {} files | {} graph nodes | {} graph edges",
        report.stats.repositories,
        report.stats.files_analyzed,
        report.stats.graph_nodes,
        report.stats.graph_edges
    );
    let _ = writeln!(out);

    if let Some(coverage) = &report.coverage {
        let _ = writeln!(out, "**Coverage:** {}", describe_coverage(coverage));
        for diagnostic in &report.diagnostics {
            let _ = writeln!(
                out,
                "- `{}`",
                escape_inline_code(&describe_extraction_diagnostic(diagnostic))
            );
        }
        let _ = writeln!(out);
    }

    if report.findings.is_empty() {
        let _ = writeln!(out, "{}", empty_findings_message(report));
        let _ = writeln!(out);
    } else {
        let _ = writeln!(
            out,
            "| ID | Severity | Confidence | Endpoint | Change | Consumers |"
        );
        let _ = writeln!(out, "| --- | --- | --- | --- | --- | --- |");

        for finding in &report.findings {
            let endpoint = finding.change.endpoint();
            let endpoint_str = format!(
                "`{} {}`",
                endpoint.method.as_str(),
                escape_md_cell(&endpoint.normalized_path)
            );
            let change_desc = escape_md_cell(&describe_change(&finding.change));
            let consumers_summary = if finding.indirect_consumers.is_empty() {
                format!("{} direct", finding.consumers.len())
            } else {
                format!(
                    "{} direct, {} indirect",
                    finding.consumers.len(),
                    finding.indirect_consumers.len()
                )
            };

            let _ = writeln!(
                out,
                "| `{}` | `{}` | `{}` | {} | {} | {} |",
                escape_md_cell(&finding.id),
                severity_label(finding.severity),
                confidence_label(finding.confidence),
                endpoint_str,
                change_desc,
                consumers_summary
            );
        }
        let _ = writeln!(out);

        for finding in &report.findings {
            let endpoint = finding.change.endpoint();
            let change_desc = describe_change(&finding.change);
            let _ = writeln!(out, "<details>");
            let _ = writeln!(
                out,
                "<summary><strong>{}</strong> [<code>{}</code> | <code>{}</code>] <code>{} {}</code> — {}</summary>",
                finding.id,
                severity_label(finding.severity),
                confidence_label(finding.confidence),
                endpoint.method.as_str(),
                endpoint.normalized_path,
                change_desc
            );
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "- **Producer:** `{}` (revision `{}`)",
                escape_inline_code(&producer_location(&finding.producer_evidence)),
                short_sha(&finding.producer_evidence.revision)
            );
            if let Some(comparison) = &finding.producer_evidence_comparison {
                let _ = writeln!(
                    out,
                    "- **Compared:** `{}` (revision `{}`)",
                    escape_inline_code(&producer_location(comparison)),
                    short_sha(&comparison.revision)
                );
            }

            if finding.consumers.is_empty() {
                let _ = writeln!(out, "- **Affected consumers:** 0 detected");
            } else {
                let _ = writeln!(
                    out,
                    "- **Affected consumers ({}):**",
                    finding.consumers.len()
                );
                for consumer in &finding.consumers {
                    let _ = writeln!(
                        out,
                        "  - `{}` [`{}`] — `{}/{}:{}` (commit `{}`)",
                        consumer.repo,
                        confidence_label(consumer.confidence),
                        consumer.evidence.repo,
                        consumer.evidence.file,
                        consumer.evidence.line,
                        short_sha(&consumer.evidence.revision)
                    );
                    if !consumer.evidence.snippet.is_empty() {
                        let _ = writeln!(
                            out,
                            "    - Evidence: `{}`",
                            escape_inline_code(&consumer.evidence.snippet)
                        );
                    }
                    let _ = writeln!(
                        out,
                        "    - Path: `{}`",
                        consumer.dependency_path.join(" -> ")
                    );
                }
            }

            if !finding.indirect_consumers.is_empty() {
                let _ = writeln!(
                    out,
                    "- **Indirectly affected consumers ({} `POSSIBLE`):**",
                    finding.indirect_consumers.len()
                );
                for indirect in &finding.indirect_consumers {
                    let _ = writeln!(
                        out,
                        "  - `{}` [`{}`] via `{}` (`{}`)",
                        indirect.repo,
                        confidence_label(indirect.confidence),
                        indirect.via_repo,
                        indirect.dependency_chain.join(" -> ")
                    );
                }
            }

            let _ = writeln!(out);
            let _ = writeln!(out, "</details>");
            let _ = writeln!(out);
        }
    }

    if !report.discrepancies.is_empty() {
        let _ = writeln!(
            out,
            "### Contract Discrepancies ({})",
            report.discrepancies.len()
        );
        let _ = writeln!(out);
        for d in &report.discrepancies {
            let _ = writeln!(
                out,
                "- [`{:?}`] `{} {}` — {}",
                d.kind,
                d.endpoint.method.as_str(),
                d.endpoint.normalized_path,
                d.detail
            );
        }
        let _ = writeln!(out);
    }

    if !report.unresolved_calls.is_empty() {
        let _ = writeln!(
            out,
            "### Unresolved Calls (`UNKNOWN`) ({})",
            report.unresolved_calls.len()
        );
        let _ = writeln!(out);
        for call in &report.unresolved_calls {
            let _ = write!(
                out,
                "- `{}/{}:{}` — {}",
                call.evidence.repo, call.evidence.file, call.evidence.line, call.reason,
            );
            if !call.evidence.snippet.is_empty() {
                let _ = write!(out, " (`{}`)", escape_inline_code(&call.evidence.snippet));
            }
            let _ = writeln!(out);
        }
        let _ = writeln!(out);
    }

    out
}

// Resume la decisión evaluada y conserva un estado explícito para informes antiguos.
fn describe_policy(report: &AnalysisReport) -> String {
    let Some(policy) = &report.policy else {
        return "POLICY NOT EVALUATED".into();
    };
    let status = if policy.should_fail {
        "POLICY BLOCKED"
    } else {
        "POLICY PASSED"
    };
    format!(
        "{status} (exit code {}) — {}",
        policy.exit_code, policy.reason
    )
}

// Formatea solo las coordenadas verificadas y conserva el pointer cuando está disponible.
fn producer_location(evidence: &ProducerEvidence) -> String {
    let mut location = format!("{}/{}", evidence.repo, evidence.file);
    if let Some(line) = evidence.line {
        let _ = write!(location, ":{line}");
        if let Some(span) = &evidence.span {
            let _ = write!(location, ":{}", span.column);
        }
    }
    if let Some(pointer) = &evidence.json_pointer {
        let _ = write!(location, " ({pointer})");
    }
    location
}

// Resume el resultado de la extracción de cada grupo de archivos.
fn describe_coverage(coverage: &AnalysisCoverage) -> String {
    let state = if coverage.complete {
        "COMPLETE"
    } else {
        "INCOMPLETE"
    };
    let mut summary = state.to_string();
    for (label, counts) in [
        ("producer base", coverage.producer_base),
        ("producer head", coverage.producer_head),
        ("consumers", coverage.consumers),
    ] {
        let _ = write!(
            summary,
            " | {label}: {}/{} analyzed, {} read, {} failed",
            counts.analyzed, counts.discovered, counts.read, counts.failed
        );
    }
    summary
}

// Evita que un resultado parcial sin hallazgos se interprete como cobertura completa.
fn empty_findings_message(report: &AnalysisReport) -> &'static str {
    if report
        .coverage
        .as_ref()
        .is_some_and(|coverage| !coverage.complete)
    {
        "No breaking contract changes confirmed in the analyzed files."
    } else {
        "No breaking contract changes detected."
    }
}

// Identifica el archivo y el snapshot que fallaron sin imprimir el texto de entrada.
// Incluye la línea cuando el extractor tiene la posición de una declaración incompleta.
fn describe_extraction_diagnostic(diagnostic: &ExtractionDiagnostic) -> String {
    let line = diagnostic
        .line
        .map_or_else(String::new, |line| format!(":{line}"));
    format!(
        "{}: {}/{}{} ({})",
        diagnostic.code.message(),
        diagnostic.repo,
        diagnostic.file,
        line,
        short_sha(&diagnostic.revision)
    )
}

// Describe cada cambio con el mismo formato para terminal y Markdown.
fn describe_change(change: &ContractChange) -> String {
    match change {
        ContractChange::EndpointRemoved { .. } => "Removed endpoint".to_string(),
        ContractChange::ResponseFieldRemoved {
            status_code,
            field_path,
            previous_type,
            ..
        } => {
            format!("Removed response field `{field_path}` ({previous_type}, HTTP {status_code})")
        }
        ContractChange::ResponseFieldTypeChanged {
            status_code,
            field_path,
            previous_type,
            new_type,
            ..
        } => format!(
            "Changed response field `{field_path}` type: {previous_type} -> {new_type} (HTTP {status_code})"
        ),
        ContractChange::ResponseFieldMadeOptional {
            status_code,
            field_path,
            ..
        } => format!("Response field `{field_path}` became optional (HTTP {status_code})"),
        ContractChange::BodyTypeChanged {
            status_code,
            previous_type,
            new_type,
            ..
        } => match status_code {
            Some(status) => {
                format!("Changed response body type: {previous_type} -> {new_type} (HTTP {status})")
            }
            None => format!("Changed request body type: {previous_type} -> {new_type}"),
        },
        ContractChange::RequestParameterTypeChanged {
            location,
            name,
            previous_type,
            new_type,
            ..
        } => format!("Changed {location} parameter `{name}` type: {previous_type} -> {new_type}"),
        ContractChange::RequestFieldAddedRequired {
            field_path,
            field_type,
            ..
        } => format!("Added required request field `{field_path}` ({field_type})"),
        ContractChange::RequestFieldTypeChanged {
            field_path,
            previous_type,
            new_type,
            ..
        } => format!("Changed request field `{field_path}` type: {previous_type} -> {new_type}"),
        ContractChange::RequiredParameterAdded {
            location,
            name,
            param_type,
            ..
        } => format!("Added required {location} parameter `{name}` ({param_type})"),
    }
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Low => "LOW",
        Severity::Medium => "MEDIUM",
        Severity::High => "HIGH",
        Severity::Critical => "CRITICAL",
    }
}

fn confidence_label(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Unknown => "UNKNOWN",
        Confidence::Possible => "POSSIBLE",
        Confidence::Likely => "LIKELY",
        Confidence::Confirmed => "CONFIRMED",
    }
}

fn escape_md_cell(raw: &str) -> String {
    raw.replace('|', "\\|").replace(['\r', '\n'], " ")
}

fn escape_inline_code(raw: &str) -> String {
    raw.replace('`', "'").replace(['\r', '\n'], " ")
}

fn short_sha(rev: &str) -> &str {
    if !rev.is_empty() && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        rev.get(..7).unwrap_or(rev)
    } else {
        rev
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use laine_core::model::{
        AnalysisStats, Confidence, EndpointId, Evidence, HttpMethod, ImpactFinding, Severity,
    };

    #[test]
    fn renders_request_side_breaking_changes_in_terminal_output() {
        let ep = EndpointId {
            service: "customer-service".into(),
            method: HttpMethod::Post,
            normalized_path: "/customers".into(),
        };
        let evidence = ProducerEvidence {
            repo: "customer-service".into(),
            revision: "abcdef123456".into(),
            file: "openapi.yaml".into(),
            line: Some(12),
            snippet: "taxId".into(),
            ..ProducerEvidence::default()
        };

        let report = AnalysisReport {
            schema_version: 1,
            target_repo: "customer-service".into(),
            base_rev: "1111111aaa".into(),
            head_rev: "2222222bbb".into(),
            policy: None,
            exit_code: None,
            workspace: None,
            consumer_revisions: Default::default(),
            coverage: None,
            diagnostics: Vec::new(),
            stats: AnalysisStats::default(),
            findings: vec![
                ImpactFinding {
                    id: "LAINE-001".into(),
                    change: ContractChange::RequestFieldAddedRequired {
                        endpoint: ep.clone(),
                        field_path: "taxId".into(),
                        field_type: "string".into(),
                    },
                    severity: Severity::High,
                    confidence: Confidence::Confirmed,
                    producer_evidence: evidence.clone(),
                    producer_evidence_comparison: None,
                    consumers: Vec::new(),
                    indirect_consumers: Vec::new(),
                },
                ImpactFinding {
                    id: "LAINE-002".into(),
                    change: ContractChange::RequestFieldTypeChanged {
                        endpoint: ep.clone(),
                        field_path: "amount".into(),
                        previous_type: "integer".into(),
                        new_type: "string".into(),
                    },
                    severity: Severity::High,
                    confidence: Confidence::Confirmed,
                    producer_evidence: evidence.clone(),
                    producer_evidence_comparison: None,
                    consumers: Vec::new(),
                    indirect_consumers: Vec::new(),
                },
                ImpactFinding {
                    id: "LAINE-003".into(),
                    change: ContractChange::RequiredParameterAdded {
                        endpoint: ep,
                        location: "header".into(),
                        name: "X-Tenant-Id".into(),
                        param_type: "string".into(),
                    },
                    severity: Severity::Medium,
                    confidence: Confidence::Confirmed,
                    producer_evidence: evidence,
                    producer_evidence_comparison: None,
                    consumers: Vec::new(),
                    indirect_consumers: Vec::new(),
                },
            ],
            unresolved_calls: Vec::new(),
            discrepancies: Vec::new(),
        };

        let rendered = render_terminal(&report, 5);
        assert!(rendered.contains("Added required request field `taxId` (string)"));
        assert!(rendered.contains("Changed request field `amount` type: integer -> string"));
        assert!(rendered.contains("Added required header parameter `X-Tenant-Id` (string)"));
    }

    #[test]
    fn renders_markdown_pr_report_with_table_details_indirect_discrepancies_and_unresolved() {
        use laine_core::model::{
            AffectedConsumer, ContractDiscrepancy, DiscrepancyKind, IndirectConsumer,
            UnresolvedCall,
        };

        let ep = EndpointId {
            service: "customer-service".into(),
            method: HttpMethod::Get,
            normalized_path: "/customers/:id".into(),
        };

        let report = AnalysisReport {
            schema_version: 1,
            target_repo: "customer-service".into(),
            base_rev: "1111111aaaa".into(),
            head_rev: "2222222bbbb".into(),
            policy: None,
            exit_code: None,
            workspace: None,
            consumer_revisions: Default::default(),
            coverage: None,
            diagnostics: Vec::new(),
            stats: AnalysisStats {
                repositories: 3,
                files_analyzed: 12,
                graph_nodes: 20,
                graph_edges: 25,
            },
            findings: vec![ImpactFinding {
                id: "LAINE-001".into(),
                change: ContractChange::ResponseFieldRemoved {
                    endpoint: ep.clone(),
                    status_code: "200".into(),
                    field_path: "fullName".into(),
                    previous_type: "string".into(),
                },
                severity: Severity::High,
                confidence: Confidence::Confirmed,
                producer_evidence: ProducerEvidence {
                    repo: "customer-service".into(),
                    revision: "2222222bbbb".into(),
                    file: "openapi.yaml".into(),
                    line: Some(14),
                    snippet: "field `fullName`".into(),
                    ..ProducerEvidence::default()
                },
                producer_evidence_comparison: None,
                consumers: vec![AffectedConsumer {
                    repo: "billing-service".into(),
                    evidence: Evidence {
                        repo: "billing-service".into(),
                        revision: "3333333cccc".into(),
                        file: "src/invoice.ts".into(),
                        line: 9,
                        snippet: "return c.fullName;".into(),
                    },
                    confidence: Confidence::Confirmed,
                    dependency_path: vec![
                        "GET /customers/:id".into(),
                        "HTTP call @ billing-service/src/invoice.ts:7".into(),
                        "read `fullName` @ billing-service/src/invoice.ts:9".into(),
                    ],
                }],
                indirect_consumers: vec![IndirectConsumer {
                    repo: "accounting-service".into(),
                    confidence: Confidence::Possible,
                    via_repo: "billing-service".into(),
                    dependency_chain: vec!["billing-service".into(), "accounting-service".into()],
                }],
            }],
            unresolved_calls: vec![UnresolvedCall {
                repo: "billing-service".into(),
                evidence: Evidence {
                    repo: "billing-service".into(),
                    revision: "3333333cccc".into(),
                    file: "src/legacy.ts".into(),
                    line: 22,
                    snippet: "fetch(dynamicUrl)".into(),
                },
                reason: "dynamic URL cannot be resolved statically".into(),
            }],
            discrepancies: vec![ContractDiscrepancy {
                endpoint: ep,
                kind: DiscrepancyKind::UndocumentedInOpenApi,
                detail: "Route exposed in routes/api.php:10 is missing from OpenAPI".into(),
            }],
        };

        let md = render_markdown(&report);
        assert!(md.contains("## Laine Impact Analysis"));
        assert!(md.contains("| ID | Severity | Confidence | Endpoint | Change | Consumers |"));
        assert!(md.contains("| `LAINE-001` | `HIGH` | `CONFIRMED` | `GET /customers/:id` |"));
        assert!(md.contains("1 direct, 1 indirect"));
        assert!(md.contains("<details>"));
        assert!(md.contains("billing-service/src/invoice.ts:9"));
        assert!(md.contains("return c.fullName;"));
        assert!(md.contains("accounting-service"));
        assert!(md.contains("POSSIBLE"));
        assert!(md.contains("### Contract Discrepancies (1)"));
        assert!(md.contains("### Unresolved Calls (`UNKNOWN`) (1)"));
        assert!(md.contains("billing-service/src/legacy.ts:22"));
    }

    // Verifica el reporte compatible con una política evaluada.
    #[test]
    fn renders_markdown_for_compatible_report() {
        let mut report = AnalysisReport {
            schema_version: 1,
            target_repo: "customer-service".into(),
            base_rev: "1111111".into(),
            head_rev: "2222222".into(),
            policy: None,
            exit_code: None,
            workspace: None,
            consumer_revisions: Default::default(),
            coverage: None,
            diagnostics: Vec::new(),
            stats: AnalysisStats::default(),
            findings: Vec::new(),
            unresolved_calls: Vec::new(),
            discrepancies: Vec::new(),
        };
        report.policy = Some(laine_core::policy::evaluate_policy(
            &report,
            &Default::default(),
        ));

        let md = render_markdown(&report);
        assert!(md.contains("## Laine Impact Analysis"));
        assert!(md.contains("POLICY PASSED (exit code 0)"));
        assert!(md.contains("No breaking contract changes detected."));
        assert!(!md.contains("<details>"));
    }

    #[test]
    fn preserves_worktree_and_staged_revision_labels_without_truncation() {
        let report = AnalysisReport {
            schema_version: 1,
            target_repo: "cerebro-web".into(),
            base_rev: "abcdef1234567890".into(),
            head_rev: "WORKTREE".into(),
            policy: None,
            exit_code: None,
            workspace: None,
            consumer_revisions: Default::default(),
            coverage: None,
            diagnostics: Vec::new(),
            stats: AnalysisStats::default(),
            findings: Vec::new(),
            unresolved_calls: Vec::new(),
            discrepancies: Vec::new(),
        };

        let term = render_terminal(&report, 10);
        assert!(
            term.contains("(abcdef1 -> WORKTREE)"),
            "expected full WORKTREE label, got:\n{term}"
        );
        assert!(!term.contains("WORKTRE)"));

        let md = render_markdown(&report);
        assert!(
            md.contains("`abcdef1` -> `WORKTREE`"),
            "expected full WORKTREE label in markdown, got:\n{md}"
        );
    }
}
