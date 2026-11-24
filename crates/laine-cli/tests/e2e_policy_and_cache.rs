use std::fs;
use std::process::Command;

use laine_cli::engine::{AnalyzeRequest, index_workspace, run_analysis, run_analysis_with_options};
use laine_cli::report::{render_json, render_markdown};
use laine_core::config::WorkspaceConfig;
use laine_core::fixtures::create_demo_workspace;
use laine_core::model::Severity;
use laine_core::policy::evaluate_policy;

#[test]
fn index_workspace_populates_sqlite_cache_and_matches_uncached_json_byte_for_byte() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-cache-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");

    // Run 1: cold cache -> all 4 repos are cache misses and .laine-cache.db is created
    let summary_1 = index_workspace(&paths.config_path).expect("first index run should succeed");
    assert_eq!(summary_1.repos_indexed, 4);
    assert_eq!(summary_1.cache_hits, 0);
    assert_eq!(summary_1.cache_misses, 4);
    assert!(summary_1.cache_path.exists());
    assert!(
        summary_1
            .cache_path
            .file_name()
            .is_some_and(|n| n == ".laine-cache.db")
    );

    // Run 2: warm cache -> all 4 repos are cache hits
    let summary_2 = index_workspace(&paths.config_path).expect("second index run should succeed");
    assert_eq!(summary_2.repos_indexed, 4);
    assert_eq!(summary_2.cache_hits, 4);
    assert_eq!(summary_2.cache_misses, 0);
    assert_eq!(summary_2.cache_path, summary_1.cache_path);

    let req = AnalyzeRequest {
        config_path: paths.config_path.clone(),
        target_repo: "customer-service".into(),
        base_rev: "main".into(),
        head_rev: "feature/split-customer-name".into(),
    };

    // Cached analysis vs --no-cache analysis MUST produce byte-for-byte identical JSON
    let cached_report = run_analysis(&req).expect("cached analysis should succeed");
    let uncached_report =
        run_analysis_with_options(&req, true).expect("uncached analysis should succeed");

    let cached_json = render_json(&cached_report).expect("cached json should serialize");
    let uncached_json = render_json(&uncached_report).expect("uncached json should serialize");
    assert_eq!(cached_json, uncached_json);
    assert_eq!(cached_report.findings.len(), 1);
    assert_eq!(cached_report.findings[0].severity, Severity::High);

    // Markdown PR report contains expected sections and consumer references
    let md = render_markdown(&cached_report);
    assert!(md.contains("## Laine Impact Analysis"));
    assert!(md.contains("| ID | Severity | Confidence | Endpoint | Change | Consumers |"));
    assert!(md.contains("billing-service"));
    assert!(md.contains("notification-service"));
    assert!(md.contains("### Unresolved Calls (`UNKNOWN`) (1)"));

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn policy_ignore_endpoints_and_fail_on_thresholds_control_filtering_and_cli_exit_codes() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-policy-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");
    let bin = env!("CARGO_BIN_EXE_laine");

    // 1. Default policy: High finding triggers exit code 2 + Markdown output works via CLI
    let default_out = Command::new(bin)
        .args([
            "analyze",
            "--config",
            paths.config_path.to_str().unwrap(),
            "--repo",
            "customer-service",
            "--base",
            "main",
            "--head",
            "feature/split-customer-name",
            "--format",
            "markdown",
        ])
        .output()
        .expect("laine binary should run");
    assert_eq!(default_out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&default_out.stdout);
    assert!(stdout.contains("## Laine Impact Analysis"));
    assert!(stdout.contains("| `LAINE-001` | `HIGH` | `CONFIRMED` |"));

    // 2. CLI `laine index` subcommand works and reports cache hits after analyze populated cache
    let index_out = Command::new(bin)
        .args(["index", "--config", paths.config_path.to_str().unwrap()])
        .output()
        .expect("laine index should run");
    assert_eq!(index_out.status.code(), Some(0));
    let index_stdout = String::from_utf8_lossy(&index_out.stdout);
    assert!(index_stdout.contains("Indexed 4 repo(s)"));

    // 3. Policy fail_on = "critical" allows High findings to exit cleanly (exit code 0)
    let mut cfg_raw = fs::read_to_string(&paths.config_path).unwrap();
    cfg_raw.push_str("\n[policy]\nfail_on = \"critical\"\n");
    let critical_cfg_path = tmp.join("laine-critical.toml");
    fs::write(&critical_cfg_path, &cfg_raw).unwrap();

    let critical_cfg = WorkspaceConfig::load_from_path(&critical_cfg_path).unwrap();
    assert_eq!(critical_cfg.policy.fail_on, Severity::Critical);

    let report_critical = run_analysis(&AnalyzeRequest {
        config_path: critical_cfg_path.clone(),
        target_repo: "customer-service".into(),
        base_rev: "main".into(),
        head_rev: "feature/split-customer-name".into(),
    })
    .unwrap();
    assert_eq!(report_critical.findings.len(), 1);
    assert_eq!(report_critical.findings[0].severity, Severity::High);
    let eval = evaluate_policy(&report_critical, &critical_cfg.policy);
    assert!(!eval.should_fail);

    let critical_cli = Command::new(bin)
        .args([
            "analyze",
            "--config",
            critical_cfg_path.to_str().unwrap(),
            "--repo",
            "customer-service",
            "--base",
            "main",
            "--head",
            "feature/split-customer-name",
            "--no-cache",
        ])
        .output()
        .expect("laine analyze with fail_on=critical should run");
    assert_eq!(critical_cli.status.code(), Some(0));

    // 4. Policy ignore_endpoints suppresses matching routes cleanly -> 0 findings, exit code 0
    let mut ignore_raw = fs::read_to_string(&paths.config_path).unwrap();
    ignore_raw.push_str("\n[policy]\nignore_endpoints = [\"/customers/*\"]\n");
    let ignore_cfg_path = tmp.join("laine-ignore.toml");
    fs::write(&ignore_cfg_path, &ignore_raw).unwrap();

    let ignored_report = run_analysis(&AnalyzeRequest {
        config_path: ignore_cfg_path.clone(),
        target_repo: "customer-service".into(),
        base_rev: "main".into(),
        head_rev: "feature/split-customer-name".into(),
    })
    .unwrap();
    assert!(ignored_report.findings.is_empty());
    assert_eq!(ignored_report.unresolved_calls.len(), 1);

    let ignore_cli = Command::new(bin)
        .args([
            "analyze",
            "--config",
            ignore_cfg_path.to_str().unwrap(),
            "--repo",
            "customer-service",
            "--base",
            "main",
            "--head",
            "feature/split-customer-name",
        ])
        .output()
        .expect("laine analyze with ignore_endpoints should run");
    assert_eq!(ignore_cli.status.code(), Some(0));

    // 5. Same ignored endpoints + fail_on_unresolved = true -> fails with exit code 2 due to unresolved call
    let mut strict_unresolved_raw = fs::read_to_string(&paths.config_path).unwrap();
    strict_unresolved_raw
        .push_str("\n[policy]\nignore_endpoints = [\"/customers/*\"]\nfail_on_unresolved = true\n");
    let strict_cfg_path = tmp.join("laine-strict-unresolved.toml");
    fs::write(&strict_cfg_path, &strict_unresolved_raw).unwrap();

    let strict_cli = Command::new(bin)
        .args([
            "analyze",
            "--config",
            strict_cfg_path.to_str().unwrap(),
            "--repo",
            "customer-service",
            "--base",
            "main",
            "--head",
            "feature/split-customer-name",
        ])
        .output()
        .expect("laine analyze with fail_on_unresolved should run");
    assert_eq!(strict_cli.status.code(), Some(2));

    let _ = fs::remove_dir_all(&tmp);
}
