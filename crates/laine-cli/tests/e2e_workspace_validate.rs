use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use laine_core::fixtures::create_demo_workspace;
use laine_core::workspace::WorkspaceRegistry;
use serde_json::Value;

const OPENAPI: &str = "openapi: 3.0.3\npaths: {}\n";

struct Fixture(PathBuf);

impl Fixture {
    // Keep every workspace and its registry in a separate temporary directory.
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("laine-validate-{}-{name}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    // Set up the requested config without using the CLI config writer, which sanitizes URLs.
    fn write(&self, relative: &str, contents: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    // Initialize an empty Git checkout without requiring global Git settings.
    fn repo(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir(&path).unwrap();
        git(&path, &["init", "-b", "main"]);
        path
    }

    // Run the CLI with no standard input and a registry isolated from the user's settings.
    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_laine"))
            .current_dir(cwd)
            .env("LAINE_HOME", self.0.join("state"))
            .stdin(Stdio::null())
            .args(["workspace", "validate"])
            .args(args)
            .output()
            .unwrap()
    }

    // Validate from the workspace root and check the externally observable exit code.
    fn json(&self, expected: i32, args: &[&str]) -> Value {
        let mut flags = args.to_vec();
        flags.extend(["--format", "json"]);
        json_report(self.run(&self.0, &flags), expected)
    }
}

impl Drop for Fixture {
    // Clean up repositories, including their local Git state.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Run fixture Git commands under a local, per-invocation identity.
fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(path)
        .args([
            "-c",
            "user.name=Laine Test",
            "-c",
            "user.email=laine@example.test",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// Commit all prepared fixture files to make them visible at HEAD.
fn commit(path: &Path) {
    git(path, &["add", "."]);
    git(path, &["commit", "--no-gpg-sign", "-m", "initial snapshot"]);
}

// Verify the exit code and parse the single JSON document from stdout.
fn json_report(output: Output, expected: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["exit_code"], expected);
    report
}

// Locate issues by stable code without depending on their prose.
fn issues(report: &Value, workspace: usize, code: &str) -> Vec<Value> {
    report["workspaces"][workspace]["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|issue| issue["code"] == code)
        .cloned()
        .collect()
}

// Exercise each renderer with the real demo and check byte-for-byte JSON determinism.
#[test]
fn validates_demo_in_all_formats() {
    let fixture = Fixture::new("demo");
    let demo = create_demo_workspace(&fixture.0.join("demo")).unwrap();
    for (args, title) in [
        (&[][..], "Workspace Validation"),
        (&["--format", "markdown"][..], "# Workspace Validation"),
    ] {
        let output = fixture.run(&demo.root, args);
        assert_eq!(output.status.code(), Some(0));
        let report = String::from_utf8(output.stdout).unwrap();
        assert!(report.starts_with(title));
        assert!(report.contains("VALID"));
        assert!(report.contains("4 service(s), 0 error(s), 0 warning(s)"));
    }

    let first = fixture.run(&demo.root, &["--format", "json"]);
    let second = fixture.run(&demo.root, &["--format", "json"]);
    assert_eq!(first.stdout, second.stdout);
    let report = json_report(first, 0);
    assert_eq!(report["valid"], true);
    assert_eq!(report["summary"]["services_checked"], 4);
    assert_eq!(report["workspaces"][0]["issues"], serde_json::json!([]));
    assert!(report["workspaces"][0].get("workspace").is_none());

    let from_service = fixture.run(&demo.billing_service, &["--format", "json"]);
    assert_eq!(
        json_report(from_service, 0)["summary"]["services_checked"],
        4
    );
}

// Check registry ordering, invalid names, missing paths, named selection, and explicit override.
#[test]
fn validates_mixed_registry_and_named_selection() {
    let fixture = Fixture::new("registry");
    let demo = create_demo_workspace(&fixture.0.join("demo")).unwrap();
    fixture.write("broken.toml", "version = [");
    let registered_config = demo.root.join("registered.toml");
    fs::rename(&demo.config_path, &registered_config).unwrap();
    let mut registry = WorkspaceRegistry::empty();
    registry.workspaces.extend([
        ("z-valid".into(), registered_config),
        ("a-invalid".into(), fixture.0.join("broken.toml")),
        ("bad/name".into(), fixture.0.join("missing.toml")),
    ]);
    registry
        .save_to_path(&fixture.0.join("state/workspaces.toml"))
        .unwrap();

    let all = fixture.json(1, &["--all"]);
    let entries = all["workspaces"].as_array().unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry["workspace"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a-invalid", "bad/name", "z-valid"]
    );
    assert_eq!(all["summary"]["workspaces_checked"], 3);
    assert_eq!(all["summary"]["services_checked"], 4);
    assert_eq!(all["summary"]["errors"], 3);
    assert_eq!(issues(&all, 0, "invalid_config_syntax").len(), 1);
    assert_eq!(issues(&all, 1, "invalid_workspace_name").len(), 1);
    assert_eq!(issues(&all, 1, "missing_workspace_config").len(), 1);
    assert_eq!(entries[2]["valid"], true);

    let named = fixture.json(0, &["z-valid"]);
    assert_eq!(named["summary"]["workspaces_checked"], 1);
    assert_eq!(named["workspaces"][0]["workspace"], "z-valid");
    assert_eq!(
        fixture.json(1, &["a-invalid"])["workspaces"][0]["valid"],
        false
    );

    let auto = json_report(fixture.run(&demo.billing_service, &["--format", "json"]), 0);
    assert_eq!(auto["workspaces"][0]["workspace"], "z-valid");
    let explicit = fixture.json(0, &["a-invalid", "--config", "demo/registered.toml"]);
    assert!(explicit["workspaces"][0].get("workspace").is_none());
}

// A missing, malformed, or unsupported registry still produces a validation report.
#[test]
fn rejects_unreadable_or_invalid_registry() {
    let fixture = Fixture::new("invalid-registry");
    let report = fixture.json(1, &["--all"]);
    assert_eq!(issues(&report, 0, "invalid_registry").len(), 1);
    assert_eq!(report["summary"]["workspaces_checked"], 0);
    for raw in ["version = [", "version = 2"] {
        fixture.write("state/workspaces.toml", raw);
        let report = fixture.json(1, &["--all"]);
        assert_eq!(issues(&report, 0, "invalid_registry").len(), 1);
    }
}

// Warnings preserve their classification and withhold embedded credentials from reports.
#[test]
fn strict_fails_on_warnings_without_exposing_url_credentials() {
    let fixture = Fixture::new("strict");
    fixture.repo("service");
    fixture.write(
        "laine.toml",
        r#"
version = 1
[env]
UPSTREAM = "https://account:env-password@example.test"
[[services]]
name = "service"
path = "service"
base_urls = ["http://account:base-password@example.test/?token=hidden", "http://example.test"]
"#,
    );
    let normal = fixture.json(0, &[]);
    let strict = fixture.json(1, &["--strict"]);
    assert_eq!(normal["valid"], true);
    assert_eq!(strict["valid"], false);
    assert_eq!(strict["strict"], true);
    assert_eq!(strict["summary"]["warnings"], 4);
    assert_eq!(strict["summary"]["errors"], 0);
    assert_eq!(
        normal["workspaces"][0]["issues"],
        strict["workspaces"][0]["issues"]
    );
    for code in [
        "unsanitized_base_url",
        "unsanitized_env_url",
        "duplicate_base_url",
        "unborn_head_revision",
    ] {
        assert_eq!(issues(&strict, 0, code)[0]["severity"], "WARNING");
    }
    let serialized = normal.to_string();
    for value in ["env-password", "base-password", "token=hidden"] {
        assert!(!serialized.contains(value));
    }
}

// Independent service, client, policy, and cache errors are all reported together.
#[test]
fn reports_repository_and_workspace_setting_errors() {
    let fixture = Fixture::new("settings");
    fs::create_dir(fixture.0.join("plain")).unwrap();
    fixture.write(
        "laine.toml",
        r#"
version = 1
cache_path = "plain"

[policy]
ignore_endpoints = ["GETT /health", "GET health", "GET /health extra", " "]
[env]
"" = "ordinary"
[[services]]
name = "gone"
path = "gone"
base_urls = ["https://api.example.test/main/"]
openapi = "../outside.yaml"

[[services]]
name = " "
path = "plain"
base_urls = ["https://api.example.test/main", "ftp://api.example.test"]

[[services]]
name = "alias"
path = "plain/../plain"

[[clients]]
consumer_service = "unknown"
callee = " "
target_service = "other"
path_prefix = "http://example.test"

[[clients]]
consumer_service = "unknown"
callee = " "
target_service = "other"
"#,
    );
    let report = fixture.json(1, &[]);
    for code in [
        "missing_service_path",
        "invalid_git_repository",
        "empty_service_name",
        "duplicate_service_path",
        "invalid_openapi_path",
        "invalid_base_url",
        "ambiguous_base_url",
        "empty_client_callee",
        "unknown_client_target",
        "unknown_client_consumer",
        "duplicate_client_mapping",
        "suspicious_client_prefix",
        "invalid_ignore_pattern_method",
        "invalid_ignore_pattern_path",
        "empty_ignore_pattern",
        "empty_env_key",
        "invalid_cache_path",
    ] {
        assert!(
            !issues(&report, 0, code).is_empty(),
            "missing {code}: {report}"
        );
    }
}

// Distinguish absent, corrupt, uncommitted, and partially resolved OpenAPI documents.
#[test]
fn validates_openapi_snapshot_and_references() {
    let fixture = Fixture::new("openapi");
    let service = fixture.repo("service");
    fixture.write("service/broken.yaml", "paths: [");
    fixture.write(
        "service/reference.yaml",
        r#"
openapi: 3.0.3
paths:
  /health:
    $ref: "./not-found.yaml#/paths/~1health"
"#,
    );
    fixture.write(
        "service/cycle.yaml",
        "openapi: 3.0.3\npaths:\n  /health:\n    $ref: '#/paths/~1health'\n",
    );
    commit(&service);
    fixture.write("service/uncommitted.yaml", OPENAPI);
    for (file, expected, code) in [
        ("absent.yaml", 1, "missing_openapi_file"),
        ("broken.yaml", 1, "invalid_openapi_document"),
        ("reference.yaml", 0, "openapi_reference_diagnostic"),
        ("cycle.yaml", 0, "openapi_reference_diagnostic"),
        ("uncommitted.yaml", 0, "uncommitted_openapi_file"),
    ] {
        fixture.write(
            "laine.toml",
            &format!(
                "version = 1\n[[services]]\nname = 'api'\npath = 'service'\nopenapi = '{file}'"
            ),
        );
        assert!(
            !issues(&fixture.json(expected, &[]), 0, code).is_empty(),
            "{file}"
        );
    }
}

// Verify every supported framework can produce route health diagnostics.
#[test]
fn checks_route_extractors_and_file_availability() {
    let fixture = Fixture::new("routes");
    let repo = fixture.repo("repo");
    fixture.write("repo/app.ts", "const app = (");
    fixture.write(
        "repo/fast.py",
        "app = FastAPI()\napp.include_router(make_router())\n",
    );
    fixture.write(
        "repo/flask.py",
        "app = Flask(__name__)\napp.register_blueprint(make_blueprint())\n",
    );
    fixture.write(
        "repo/dynamic.php",
        "<?php\nRoute::get($path, Handler::class);",
    );
    commit(&repo);
    fixture.write(
        "repo/local.php",
        "<?php\nRoute::get('/ok', Handler::class);",
    );

    for (framework, file, exit, code) in [
        ("express", "app.ts", 0, "incomplete_route_extraction"),
        ("fastapi", "fast.py", 0, "incomplete_route_extraction"),
        ("flask", "flask.py", 0, "incomplete_route_extraction"),
        ("laravel", "dynamic.php", 0, "incomplete_route_extraction"),
        ("laravel", "local.php", 0, "uncommitted_route_file"),
        ("laravel", "missing.php", 1, "missing_route_file"),
        ("laravel", "", 0, "missing_default_laravel_routes"),
    ] {
        let files = if file.is_empty() {
            String::new()
        } else {
            format!("files = ['{file}']")
        };
        fixture.write("laine.toml", &format!("version = 1\n[[services]]\nname = 'api'\npath = 'repo'\n[services.routes]\nframework = '{framework}'\n{files}"));
        assert_eq!(
            issues(&fixture.json(exit, &[]), 0, code).len(),
            1,
            "{framework} {file}"
        );
    }
}

// Keep Clap usage failures machine-readable for both supported JSON flag spellings.
#[test]
fn returns_usage_error_for_conflicting_and_unknown_options() {
    let fixture = Fixture::new("usage");
    for args in [
        &["--all", "--config", "laine.toml", "--format", "json"][..],
        &["--all", "named", "--format=json"][..],
        &["--unknown", "--format=json"][..],
    ] {
        let report = json_report(fixture.run(&fixture.0, args), 64);
        assert_eq!(report["status"], "error");
        assert_eq!(report["diagnostics"][0]["code"], "invalid_arguments");
    }
    let text = fixture.run(&fixture.0, &["--unknown"]);
    assert_eq!(text.status.code(), Some(64));
    assert!(!text.stderr.is_empty());
}
