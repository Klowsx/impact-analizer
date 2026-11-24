use std::fmt::Write;

use laine_core::model::{AnalysisReport, ContractChange};

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

    if report.findings.is_empty() {
        let _ = writeln!(out, "No breaking contract changes detected.");
    } else {
        for finding in &report.findings {
            let _ = writeln!(
                out,
                "BREAKING CHANGE [{:?} | {:?}] ({})",
                finding.confidence, finding.severity, finding.id
            );
            match &finding.change {
                ContractChange::EndpointRemoved { endpoint } => {
                    let _ = writeln!(
                        out,
                        "  Endpoint: {} {}",
                        endpoint.method.as_str(),
                        endpoint.normalized_path
                    );
                    let _ = writeln!(out, "  Change:   Removed endpoint");
                }
                ContractChange::ResponseFieldRemoved {
                    endpoint,
                    status_code,
                    field_path,
                    previous_type,
                } => {
                    let _ = writeln!(
                        out,
                        "  Endpoint: {} {}",
                        endpoint.method.as_str(),
                        endpoint.normalized_path
                    );
                    let _ = writeln!(
                        out,
                        "  Change:   Removed response field `{field_path}` ({previous_type}, HTTP {status_code})"
                    );
                }
                ContractChange::ResponseFieldTypeChanged {
                    endpoint,
                    status_code,
                    field_path,
                    previous_type,
                    new_type,
                } => {
                    let _ = writeln!(
                        out,
                        "  Endpoint: {} {}",
                        endpoint.method.as_str(),
                        endpoint.normalized_path
                    );
                    let _ = writeln!(
                        out,
                        "  Change:   Changed response field `{field_path}` type: {previous_type} -> {new_type} (HTTP {status_code})"
                    );
                }
            }

            let _ = writeln!(
                out,
                "  Source:   {}/{}:{} (commit {})",
                finding.producer_evidence.repo,
                finding.producer_evidence.file,
                finding.producer_evidence.line,
                short_sha(&finding.producer_evidence.revision)
            );
            let _ = writeln!(out);

            if finding.consumers.is_empty() {
                let _ = writeln!(out, "  Affected consumers: 0 detected");
            } else {
                let _ = writeln!(
                    out,
                    "  Affected consumers ({} confirmed):",
                    finding.consumers.len()
                );
                for (i, consumer) in finding.consumers.iter().enumerate() {
                    let _ = writeln!(out, "    {}. {}", i + 1, consumer.repo);
                    let _ = writeln!(
                        out,
                        "       File: {}/{}:{} (commit {})",
                        consumer.evidence.repo,
                        consumer.evidence.file,
                        consumer.evidence.line,
                        short_sha(&consumer.evidence.revision)
                    );
                    let _ = writeln!(out, "       Code: {}", consumer.evidence.snippet);
                    let _ = writeln!(
                        out,
                        "       Path: {}",
                        consumer.dependency_path.join(" -> ")
                    );
                }
            }
            let _ = writeln!(out);
        }
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

    let direct_consumers: usize = report.findings.iter().map(|f| f.consumers.len()).sum();
    if report.findings.is_empty() {
        let _ = writeln!(out, "RESULT: COMPATIBLE (exit code 0)");
    } else {
        let _ = writeln!(
            out,
            "RESULT: INCOMPATIBLE CHANGE ({} breaking change(s), {} confirmed consumer reference(s), exit code 2)",
            report.findings.len(),
            direct_consumers
        );
    }

    out
}

pub fn render_json(report: &AnalysisReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

fn short_sha(rev: &str) -> &str {
    rev.get(..7).unwrap_or(rev)
}
