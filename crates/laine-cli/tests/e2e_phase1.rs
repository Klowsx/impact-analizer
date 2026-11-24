use std::fs;

use laine_cli::engine::{AnalyzeRequest, run_analysis};
use laine_cli::report::{render_json, render_terminal};
use laine_core::fixtures::create_demo_workspace;
use laine_core::model::{Confidence, ContractChange, HttpMethod};

#[test]
fn phase1_vertical_slice_meets_all_acceptance_criteria() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-phase1-{}", std::process::id()));
    let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");

    let req = AnalyzeRequest {
        config_path: paths.config_path.clone(),
        target_repo: "customer-service".into(),
        base_rev: "main".into(),
        head_rev: "feature/split-customer-name".into(),
    };

    // Criterion 1 & 6: Detects breaking change across two Git commits locally without node_modules.
    let report = run_analysis(&req).expect("cross-repo analysis should succeed");
    assert_eq!(report.schema_version, 1);
    assert_eq!(report.findings.len(), 1);

    let finding = &report.findings[0];
    assert_eq!(finding.confidence, Confidence::Confirmed);
    assert!(matches!(
        &finding.change,
        ContractChange::ResponseFieldRemoved {
            endpoint,
            field_path,
            ..
        } if endpoint.service == "customer-service"
            && endpoint.method == HttpMethod::Get
            && endpoint.normalized_path == "/customers/:id"
            && field_path == "fullName"
    ));

    // Criterion 2: Reports billing-service and notification-service as CONFIRMED with full evidence.
    let consumer_repos: Vec<&str> = finding.consumers.iter().map(|c| c.repo.as_str()).collect();
    assert_eq!(
        consumer_repos,
        vec!["billing-service", "notification-service"]
    );

    for consumer in &finding.consumers {
        assert_eq!(consumer.confidence, Confidence::Confirmed);
        assert_eq!(consumer.evidence.revision.len(), 40);
        assert!(consumer.evidence.line > 0);
        assert!(consumer.evidence.snippet.contains("fullName"));
        assert!(consumer.dependency_path.len() >= 3);
    }

    // Criterion 3: shipping-service consumes GET /customers/:id but only reads `address`,
    // so it MUST NOT be reported as affected.
    assert!(
        !finding
            .consumers
            .iter()
            .any(|c| c.repo == "shipping-service")
    );

    // Criterion 4: Dynamic URL in billing-service/src/legacy/sync.ts is reported as unresolved (UNKNOWN).
    assert_eq!(report.unresolved_calls.len(), 1);
    assert_eq!(report.unresolved_calls[0].repo, "billing-service");
    assert_eq!(
        report.unresolved_calls[0].evidence.file,
        "src/legacy/sync.ts"
    );

    // Criterion 5: Output is strictly deterministic (byte-for-byte identical JSON across runs).
    let second_report = run_analysis(&req).expect("second analysis run should succeed");
    let json_1 = render_json(&report).expect("json serialization should succeed");
    let json_2 = render_json(&second_report).expect("json serialization should succeed");
    assert_eq!(json_1, json_2);

    // Terminal output contains key diagnostic details.
    let terminal = render_terminal(&report, 12);
    assert!(terminal.contains("BREAKING CHANGE"));
    assert!(terminal.contains("billing-service"));
    assert!(terminal.contains("notification-service"));
    assert!(terminal.contains("UNRESOLVED CALLS [UNKNOWN]"));
    assert!(!terminal.contains("shipping-service"));

    let _ = fs::remove_dir_all(&tmp);
}
