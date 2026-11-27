use std::fs;
use std::path::Path;
use std::process::Command;

use laine_cli::engine::{AnalyzeRequest, index_workspace, run_analysis, run_analysis_with_options};
use laine_cli::report::{render_json, render_markdown};
use laine_core::config::WorkspaceConfig;
use laine_core::fixtures::create_demo_workspace;
use laine_core::model::Severity;

// Compare serialized results across Rayon schedules and an analysis-populated cache.
#[test]
fn multi_repo_analysis_is_byte_identical_across_workers_and_cache_states() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-parallel-cache-{}", std::process::id()));
    let paths = create_demo_workspace(&tmp).unwrap();
    let mut config = WorkspaceConfig::load_from_path(&paths.config_path).unwrap();
    config.cache_path = Some("parallel.sqlite3".into());
    config.save_to_path(&paths.config_path).unwrap();

    let analyze = |threads, no_cache| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_laine"));
        command
            .env("RAYON_NUM_THREADS", threads)
            .args(["analyze", "--config"])
            .arg(&paths.config_path)
            .args([
                "--repo",
                "customer-service",
                "--base",
                "main",
                "--head",
                "feature/split-customer-name",
                "--format=json",
            ]);
        if no_cache {
            command.arg("--no-cache");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        output.stdout
    };

    let baseline = analyze("1", true);
    let report: serde_json::Value = serde_json::from_slice(&baseline).unwrap();
    let consumers = report["findings"][0]["consumers"].as_array().unwrap();
    let consumer_repos: Vec<_> = consumers
        .iter()
        .map(|consumer| consumer["repo"].as_str().unwrap())
        .collect();
    assert_eq!(consumer_repos, ["billing-service", "notification-service"]);
    for _ in 0..3 {
        assert_eq!(analyze("4", true), baseline);
    }
    assert!(!tmp.join("parallel.sqlite3").exists());
    assert_eq!(analyze("4", false), baseline);
    assert_eq!(analyze("4", false), baseline);
    let summary = index_workspace(&paths.config_path).unwrap();
    assert_eq!(summary.cache_hits, 4);
    assert_eq!(summary.cache_misses, 0);
    fs::remove_dir_all(&tmp).unwrap();
}

// Comprueba la ubicación del caché por defecto y el análisis cuando SQLite no está disponible.
#[test]
fn stores_default_cache_in_user_state_and_falls_back_when_unavailable() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-cache-state-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let paths = create_demo_workspace(&tmp.join("workspace")).unwrap();
    let state = tmp.join("state");
    let bin = env!("CARGO_BIN_EXE_laine");
    let index = || {
        Command::new(bin)
            .env("LAINE_HOME", &state)
            .args(["index", "--config"])
            .arg(&paths.config_path)
            .output()
            .unwrap()
    };
    assert!(index().status.success());
    assert!(!paths.root.join(".laine-cache.db").exists());
    let caches = fs::read_dir(state.join("cache"))
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(caches.len(), 1);
    assert_eq!(caches[0].path().extension().unwrap(), "sqlite3");

    let invalid = tmp.join("unavailable-cache");
    fs::create_dir(&invalid).unwrap();
    let mut config = WorkspaceConfig::load_from_path(&paths.config_path).unwrap();
    config.cache_path = Some(invalid);
    config.save_to_path(&paths.config_path).unwrap();
    let analyze = |no_cache| {
        let mut command = Command::new(bin);
        command
            .env("LAINE_HOME", &state)
            .args(["analyze", "--config"])
            .arg(&paths.config_path)
            .args([
                "--repo",
                "customer-service",
                "--base",
                "main",
                "--head",
                "main",
                "--format=json",
            ]);
        if no_cache {
            command.arg("--no-cache");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        output.stdout
    };
    assert_eq!(analyze(false), analyze(true));
    assert_eq!(index().status.code(), Some(1));
    fs::remove_dir_all(&tmp).unwrap();
}

// Verifica que stdout JSON sea consumible tanto para resultados como para errores.
#[test]
fn reports_exit_codes_errors_and_optional_workspace_roots_in_json() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-json-contract-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let paths = create_demo_workspace(&tmp).unwrap();
    let bin = env!("CARGO_BIN_EXE_laine");
    let mut workspace_id = String::new();

    for (head, expected, local_roots) in
        [("main", 0, false), ("feature/split-customer-name", 2, true)]
    {
        let mut process = Command::new(bin);
        process
            .args(["analyze", "--config"])
            .arg(&paths.config_path)
            .args([
                "--repo",
                "customer-service",
                "--base",
                "main",
                "--head",
                head,
                "--format=json",
            ]);
        if local_roots {
            process.arg("--include-repo-roots");
        }
        let output = process.output().unwrap();
        assert_eq!(output.status.code(), Some(expected));
        assert!(output.stderr.is_empty());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["exit_code"], expected);
        assert_eq!(report["policy"]["exit_code"], expected);
        assert_eq!(report["coverage"]["complete"], true);
        let id = report["workspace"]["id"].as_str().unwrap();
        if local_roots {
            assert_eq!(id, workspace_id);
            let producer = report["workspace"]["repo_roots"]["customer-service"]
                .as_str()
                .unwrap();
            assert_eq!(
                fs::canonicalize(producer).unwrap(),
                fs::canonicalize(&paths.customer_service).unwrap()
            );
        } else {
            assert_eq!(id.len(), 16);
            assert!(report["workspace"].get("repo_roots").is_none());
            workspace_id = id.to_owned();
        }
    }

    let usage = Command::new(bin)
        .args(["analyze", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(64));
    assert!(usage.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&usage.stdout).unwrap();
    assert_eq!(report["status"], "error");
    assert_eq!(report["exit_code"], 64);
    assert_eq!(report["diagnostics"][0]["code"], "invalid_arguments");
    assert_eq!(
        report["diagnostics"][0]["remediation"],
        "check_command_help"
    );

    let malformed = tmp.join("invalid.toml");
    fs::write(&malformed, "FICTITIOUS_CONFIG = [ @").unwrap();
    for command in ["analyze", "check"] {
        let result = Command::new(bin)
            .args([command, "--config"])
            .arg(&malformed)
            .env("LAINE_HOME", tmp.join("laine-home"))
            .args(["--repo", "customer-service", "--format=json"])
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stderr.is_empty());
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(!text.contains("FICTITIOUS_CONFIG"));
        let report: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["status"], "error");
        assert_eq!(report["exit_code"], 1);
        assert_eq!(report["diagnostics"][0]["code"], "invalid_config_syntax");
        assert_eq!(
            report["diagnostics"][0]["remediation"],
            "repair_workspace_toml"
        );
    }
    fs::remove_dir_all(&tmp).unwrap();
}

// Verifica que dos productores con la misma URL den el mismo diagnóstico en cualquier orden.
#[test]
fn reports_ambiguous_service_urls_consistently_with_and_without_cache() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-ambiguous-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let paths = create_demo_workspace(&tmp).unwrap();
    let mut config = WorkspaceConfig::load_from_path(&paths.config_path).unwrap();
    let duplicate = config.services[0].base_urls.clone();
    config
        .services
        .iter_mut()
        .find(|s| s.name == "shipping-service")
        .unwrap()
        .base_urls = duplicate;
    config.save_to_path(&paths.config_path).unwrap();

    fs::write(
        paths.billing_service.join("ambiguous_url.py"),
        "import requests\nrequests.get('http://customer-service/customers/42')\n",
    )
    .unwrap();
    for args in [
        &["add", "ambiguous_url.py"][..],
        &["commit", "-m", "test: add Python call"][..],
    ] {
        let output = Command::new("git")
            .current_dir(&paths.billing_service)
            .args([
                "-c",
                "user.name=Laine Test",
                "-c",
                "user.email=test@laine.local",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    let request = AnalyzeRequest {
        config_path: paths.config_path.clone(),
        target_repo: "customer-service".into(),
        base_rev: "main".into(),
        head_rev: "feature/split-customer-name".into(),
    };
    let original = run_analysis_with_options(&request, false).unwrap();
    let ambiguous: Vec<_> = original
        .unresolved_calls
        .iter()
        .filter(|c| {
            c.reason
                == "ambiguous service base URL (candidates: customer-service, shipping-service)"
        })
        .collect();
    assert!(
        ambiguous
            .iter()
            .any(|call| call.evidence.file.ends_with(".ts"))
    );
    assert!(
        ambiguous
            .iter()
            .any(|call| call.evidence.file == "ambiguous_url.py")
    );
    assert!(original.findings.iter().all(|f| f.consumers.is_empty()));

    config.services.reverse();
    config.save_to_path(&paths.config_path).unwrap();
    for bypass in [false, true] {
        let reordered = run_analysis_with_options(&request, bypass).unwrap();
        assert_eq!(
            render_json(&original).unwrap(),
            render_json(&reordered).unwrap()
        );
    }
    fs::remove_dir_all(&tmp).unwrap();
}

// Verifica el caché explícito y la equivalencia de las salidas caliente y sin caché.
#[test]
fn index_workspace_populates_sqlite_cache_and_matches_uncached_json_byte_for_byte() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-cache-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");
    let mut config = WorkspaceConfig::load_from_path(&paths.config_path).unwrap();
    config.cache_path = Some(".laine-cache.db".into());
    config.save_to_path(&paths.config_path).unwrap();

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

// Comprueba que los formatos y el proceso reflejen la misma evaluación de política.
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
    assert!(stdout.contains("POLICY BLOCKED (exit code 2)"));
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
    let eval = report_critical.policy.unwrap();
    assert!(!eval.should_fail);
    assert_policy_output(bin, &critical_cfg_path, 0, 1, 0);

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

    assert_policy_output(bin, &strict_cfg_path, 2, 0, 1);

    let _ = fs::remove_dir_all(&tmp);
}

// Ejecuta la misma fixture en los tres formatos y comprueba decisión y hallazgos.
fn assert_policy_output(
    bin: &str,
    config: &Path,
    exit_code: i32,
    findings: usize,
    blocking_unresolved: usize,
) {
    for format in ["terminal", "markdown", "json"] {
        let output = Command::new(bin)
            .args(["analyze", "--config"])
            .arg(config)
            .args([
                "--repo",
                "customer-service",
                "--base",
                "main",
                "--head",
                "feature/split-customer-name",
                "--format",
                format,
            ])
            .output()
            .expect("laine should print a policy decision");
        assert_eq!(output.status.code(), Some(exit_code), "{format}");
        assert!(output.stderr.is_empty(), "{format}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        if format == "json" {
            let report: laine_core::model::AnalysisReport = serde_json::from_str(&stdout).unwrap();
            let policy = report.policy.unwrap();
            assert_eq!(i32::from(policy.exit_code), exit_code);
            assert_eq!(policy.should_fail, exit_code == 2);
            assert_eq!(policy.blocking_findings, 0);
            assert_eq!(policy.blocking_unresolved_calls, blocking_unresolved);
            assert_eq!(report.findings.len(), findings);
        } else {
            let decision = if exit_code == 0 { "PASSED" } else { "BLOCKED" };
            assert!(stdout.contains(&format!("POLICY {decision} (exit code {exit_code})")));
            assert_eq!(
                stdout.contains("No breaking contract changes detected."),
                findings == 0
            );
        }
    }
}

#[test]
// Comprueba que la evidencia conserve su ubicación sin guardar ni publicar literales sensibles.
fn preserves_located_evidence_without_exporting_credentials() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-privacy-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");
    let mut config = WorkspaceConfig::load_from_path(&paths.config_path).unwrap();
    config.cache_path = Some(".laine-cache.db".into());
    config.services[0].base_urls = vec![
        "https://user:FICTITIOUS_PASSWORD@api.example:8443/v1?sig=FICTITIOUS_SIGNATURE#FICTITIOUS_FRAGMENT".into(),
    ];
    config.env.insert(
        "CUSTOMER_API".into(),
        "https://api.example:8443/v1?sig=FICTITIOUS_ENV_SIGNATURE".into(),
    );
    config.save_to_path(&paths.config_path).unwrap();

    let config_text = fs::read_to_string(&paths.config_path).unwrap();
    assert!(config_text.contains("https://api.example:8443/v1"));
    assert!(!config_text.contains("FICTITIOUS_"));

    fs::write(
        paths.billing_service.join("src/invoices/customer.ts"),
        r#"export async function getInvoiceCustomer(id: string): Promise<string> {
  const res = await fetch(`${process.env.CUSTOMER_API}/customers/${id}?sig=FICTITIOUS_QUERY`, { headers: { Authorization: 'FICTITIOUS_HEADER' } });
  const customer = await res.json();
  return customer.fullName; // FICTITIOUS_FIELD_READ
}
"#,
    )
    .unwrap();
    fs::write(
        paths.billing_service.join("src/invoices/credentialed_url.ts"),
        r#"export async function readCustomer() {
  const res = await fetch('https://example:FICTITIOUS_TS_USERINFO@api.example:8443/v1/customers/42');
  const customer = await res.json();
  return customer.fullName;
}
fetch(`${process.env.FICTITIOUS_TS_ENV_KEY}/customers/42`);
"#,
    )
    .unwrap();
    fs::write(
        paths.billing_service.join("credentialed_url.py"),
        r#"import requests
customer = requests.get('https://example:FICTITIOUS_PY_USERINFO@api.example:8443/v1/customers/42').json()
name = customer['fullName']
"#,
    )
    .unwrap();
    fs::write(
        paths.billing_service.join("private_call.py"),
        r#"import requests
requests.get(resolve_endpoint("FICTITIOUS_DYNAMIC_EXPRESSION"))
requests.get(os.getenv("FICTITIOUS_ENV_KEY"))
"#,
    )
    .unwrap();
    for args in [
        &["add", "."][..],
        &[
            "-c",
            "user.name=Laine Test",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-m",
            "test: add privacy fixtures",
        ],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(&paths.billing_service)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    let bin = env!("CARGO_BIN_EXE_laine");
    let index_output = Command::new(bin)
        .arg("index")
        .arg("--config")
        .arg(&paths.config_path)
        .output()
        .unwrap();
    assert!(index_output.status.success(), "{index_output:?}");
    let sqlite = fs::read(tmp.join(".laine-cache.db")).unwrap();
    let payload = String::from_utf8_lossy(&sqlite);
    assert!(payload.contains("src/invoices/customer.ts"));
    assert!(payload.contains("src/invoices/credentialed_url.ts"));
    assert!(payload.contains("credentialed_url.py"));
    assert!(!payload.contains("FICTITIOUS_"));

    // Compara resultados con las tres opciones de salida y con y sin caché.
    for format in ["terminal", "markdown", "json"] {
        for flags in [
            &[][..],
            &["--no-cache"],
            &["--no-snippets"],
            &["--no-cache", "--no-snippets"],
        ] {
            let output = Command::new(bin)
                .arg("analyze")
                .arg("--config")
                .arg(&paths.config_path)
                .args([
                    "--repo",
                    "customer-service",
                    "--base",
                    "main",
                    "--head",
                    "feature/split-customer-name",
                    "--format",
                    format,
                ])
                .args(flags)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2), "{output:?}");
            let report = String::from_utf8(output.stdout).unwrap();
            assert!(!report.contains("FICTITIOUS_"), "{report}");
            assert!(report.contains("src/invoices/customer.ts"), "{report}");
            assert!(
                report.contains("src/invoices/credentialed_url.ts"),
                "{report}"
            );
            assert!(report.contains("credentialed_url.py"), "{report}");
            assert!(report.contains("fullName"), "{report}");
            assert!(
                report.contains("dynamic URL expression cannot be resolved statically"),
                "{report}"
            );
            assert!(!String::from_utf8_lossy(&output.stderr).contains("FICTITIOUS_"));

            let omit = flags.contains(&"--no-snippets");
            assert_eq!(
                report.contains("Response field: fullName"),
                !omit,
                "{report}"
            );
            if format == "json" {
                let parsed: serde_json::Value = serde_json::from_str(&report).unwrap();
                let finding = &parsed["findings"][0];
                let consumers = finding["consumers"].as_array().unwrap();
                for file in [
                    "src/invoices/customer.ts",
                    "src/invoices/credentialed_url.ts",
                    "credentialed_url.py",
                ] {
                    assert!(
                        consumers
                            .iter()
                            .any(|item| item["evidence"]["file"] == file),
                        "{report}"
                    );
                }
                if omit {
                    assert_eq!(finding["producer_evidence"]["snippet"], "");
                    assert_eq!(consumers[0]["evidence"]["snippet"], "");
                    assert_eq!(parsed["unresolved_calls"][0]["evidence"]["snippet"], "");
                }
            }
        }
    }

    // Los errores de parseo del TOML pueden incluir la línea que los originó.
    let invalid_path = tmp.join("invalid.toml");
    fs::write(&invalid_path, "FICTITIOUS_INVALID_CONFIG = [ @").unwrap();
    let output = Command::new(bin)
        .args(["analyze", "--config"])
        .arg(invalid_path)
        .args(["--repo", "customer-service", "--no-snippets"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("Analysis failed"));
    assert!(!error.contains("FICTITIOUS_"));

    let _ = fs::remove_dir_all(&tmp);
}
