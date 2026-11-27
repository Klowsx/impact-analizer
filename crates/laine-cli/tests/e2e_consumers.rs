use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const OPENAPI: &str = "openapi: 3.0.3\npaths:\n  /customers/{id}:\n    get:\n      responses: {}\n  /unused:\n    get:\n      responses: {}\n";
const FULL_NAME_CALL: &str = r#"
export async function loadCustomer() {
  const response = await fetch("http://customer-service/customers/42");
  const customer = await response.json();
  return customer.fullName;
}
export function loadDynamic(url: string) {
  return fetch(url);
}
"#;
const OTHER_CALLS: &str = r#"
export async function loadAddress() {
  const response = await fetch("http://customer-service/customers/123");
  const customer = await response.json();
  return customer.address.city;
}
export function ping() {
  return fetch("http://customer-service/customers/42");
}
"#;
const DOWNSTREAM_CALL: &str = r#"fetch("http://billing-service/invoices/1");"#;

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    // Create independent Git repositories and a cache isolated from the user's state.
    fn new(test: &str, producer_config: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("laine-consumers-{}-{test}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let workspace = Self { root };
        for (repo, file, contents) in [
            ("customer-service", "openapi.yaml", OPENAPI),
            ("billing-service", "invoice.ts", FULL_NAME_CALL),
            ("shipping-service", "shipping.ts", OTHER_CALLS),
            ("accounting-service", "ledger.ts", DOWNSTREAM_CALL),
        ] {
            let path = workspace.root.join(repo);
            fs::create_dir(&path).unwrap();
            git(&path, &["init", "-b", "main"]);
            workspace.write(repo, file, contents);
            git(&path, &["add", "."]);
            git(
                &path,
                &["commit", "--no-gpg-sign", "-m", "initial snapshot"],
            );
        }
        fs::write(
            workspace.root.join("laine.toml"),
            format!(
                r#"version = 1
cache_path = "consumer-cache.sqlite3"

[[services]]
name = "customer-service"
path = "./customer-service"
base_urls = ["http://customer-service"]
{producer_config}

[[services]]
name = "billing-service"
path = "./billing-service"
base_urls = ["http://billing-service"]

[[services]]
name = "shipping-service"
path = "./shipping-service"
base_urls = ["http://shipping-service"]

[[services]]
name = "accounting-service"
path = "./accounting-service"
base_urls = ["http://accounting-service"]
"#
            ),
        )
        .unwrap();
        workspace
    }

    // Write a source file into one of the temporary repositories.
    fn write(&self, repo: &str, file: &str, contents: &str) {
        fs::write(self.root.join(repo).join(file), contents).unwrap();
    }

    // Run the CLI with an isolated registry and the requested working directory.
    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_laine"))
            .current_dir(cwd)
            .env("LAINE_HOME", self.root.join("laine-home"))
            .args(args)
            .output()
            .unwrap()
    }

    // Query the producer by name from the workspace directory.
    fn query(&self, path: &str, args: &[&str]) -> Output {
        let mut command = vec![
            "consumers",
            "--service",
            "customer-service",
            "--method",
            "gEt",
            "--path",
            path,
        ];
        command.extend_from_slice(args);
        self.run(&self.root, &command)
    }
}

impl Drop for Workspace {
    // Clean up the temporary directory owned by this test.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// Run Git using process-local author information for the fixture commits.
fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Laine Test")
        .env("GIT_AUTHOR_EMAIL", "test@laine.local")
        .env("GIT_COMMITTER_NAME", "Laine Test")
        .env("GIT_COMMITTER_EMAIL", "test@laine.local")
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
}

// Assert a successful command before decoding its text output.
fn stdout(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

// Decode the successful query's single JSON document.
fn json(output: Output) -> Value {
    serde_json::from_str(&stdout(output)).unwrap()
}

// Compare repositories in the direct evidence order, including distinct call locations.
fn direct_repos(report: &Value) -> Vec<&str> {
    report["consumers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|consumer| consumer["repo"].as_str().unwrap())
        .collect()
}

// Verify automatic context, route spellings, concrete paths, and all report formats.
#[test]
fn reports_direct_and_indirect_consumers_in_every_format() {
    let workspace = Workspace::new("formats", r#"openapi = "openapi.yaml""#);
    for path in ["/customers/:id", "/customers/{id}", "/customers/42"] {
        let report = json(workspace.query(path, &["--format", "json"]));
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["exit_code"], 0);
        assert_eq!(report["method"], "GET");
        assert_eq!(report["queried_path"], path);
        assert_eq!(report["normalized_path"], path.replace("{id}", ":id"));
        assert_eq!(report["in_catalog"], true);
        assert_eq!(
            report["matched_endpoints"][0]["normalized_path"],
            "/customers/:id"
        );
        assert_eq!(report["producer_evidence"]["file"], "openapi.yaml");
        assert_eq!(report["coverage"]["complete"], true);
        assert_eq!(report["coverage"]["producer"]["analyzed"], 1);
        assert_eq!(
            direct_repos(&report),
            ["billing-service", "shipping-service", "shipping-service"]
        );
        assert_eq!(
            report["fields_read"],
            serde_json::json!(["address.city", "fullName"])
        );
        assert_eq!(
            report["indirect_consumers"][0]["repo"],
            "accounting-service"
        );
        assert_eq!(
            report["indirect_consumers"][0]["dependency_chain"],
            serde_json::json!(["billing-service", "accounting-service"])
        );
        assert_eq!(report["unresolved_calls"][0]["repo"], "billing-service");
        assert!(
            !report["unresolved_calls"][0]["reason"]
                .as_str()
                .unwrap()
                .is_empty()
        );
    }

    let terminal = stdout(workspace.run(
        &workspace.root.join("customer-service"),
        &["consumers", "--method", "GET", "--path", "/customers/{id}"],
    ));
    assert!(terminal.contains("Direct Consumers (3)"));
    assert!(terminal.contains("billing-service/invoice.ts:"));
    assert!(terminal.contains("Indirect Consumers (1)"));
    assert!(terminal.contains("billing-service -> accounting-service"));
    assert!(terminal.contains("Unresolved Calls (1)"));

    let _ = stdout(workspace.run(
        &workspace.root,
        &["workspace", "add", "local", "laine.toml"],
    ));
    let markdown = stdout(workspace.run(
        &workspace.root.join("customer-service"),
        &[
            "consumers",
            "--workspace",
            "local",
            "--format",
            "markdown",
            "--method",
            "GET",
            "--path",
            "/customers/42",
        ],
    ));
    assert!(markdown.contains("## Direct Consumers (3)"));
    assert!(markdown.contains("`billing-service/invoice.ts:"));
    assert!(markdown.contains("## Indirect Consumers (1)"));
    assert!(markdown.contains("`billing-service -> accounting-service`"));
    assert!(markdown.contains("## Unresolved Calls (1)"));
}

// Verify field readers are selected at the read location, including nested fields.
#[test]
fn filters_response_field_reads() {
    let workspace = Workspace::new("fields", r#"openapi = "openapi.yaml""#);
    let name = json(workspace.query("/customers/42", &["--field", "fullName", "--format=json"]));
    assert_eq!(name["field"], "fullName");
    assert_eq!(direct_repos(&name), ["billing-service"]);
    assert_eq!(name["consumers"][0]["evidence"]["line"], 5);
    assert_eq!(name["indirect_consumers"][0]["repo"], "accounting-service");
    assert!(name.get("fields_read").is_none());
    let city = json(workspace.query(
        "/customers/:id",
        &["--field", "address.city", "--format=json"],
    ));
    assert_eq!(direct_repos(&city), ["shipping-service"]);
    assert_eq!(city["consumers"][0]["evidence"]["line"], 5);
    assert!(city.get("indirect_consumers").is_none());
}

// Distinguish committed, staged, and unstaged calls and verify mutable cache invalidation.
#[test]
fn selects_consumer_snapshots() {
    let workspace = Workspace::new("consumer-revision", r#"openapi = "openapi.yaml""#);
    let billing = workspace.root.join("billing-service");
    let greetings = r#"fetch("http://customer-service/unused");"#;
    workspace.write("billing-service", "staged.ts", greetings);
    git(&billing, &["add", "staged.ts"]);
    workspace.write("billing-service", "worktree.ts", greetings);
    let head = json(workspace.query("/unused", &["--format=json"]));
    assert_eq!(head["consumers"], serde_json::json!([]));
    let staged = json(workspace.query(
        "/unused",
        &["--format=json", "--consumer-revision", "StAgEd"],
    ));
    assert_eq!(direct_repos(&staged), ["billing-service"]);
    assert_eq!(staged["consumers"][0]["evidence"]["file"], "staged.ts");
    assert_eq!(staged["consumer_revisions"]["billing-service"], "STAGED");
    let worktree = json(workspace.query(
        "/unused",
        &["--format=json", "--consumer-revision", "worktree"],
    ));
    assert_eq!(
        direct_repos(&worktree),
        ["billing-service", "billing-service"]
    );
    assert_eq!(worktree["consumers"][1]["evidence"]["file"], "worktree.ts");
    assert_eq!(
        worktree["consumer_revisions"]["billing-service"],
        "WORKTREE"
    );
    workspace.write(
        "billing-service",
        "worktree.ts",
        "export const idle = true;",
    );
    let updated = json(workspace.query(
        "/unused",
        &["--format=json", "--consumer-revision", "worktree"],
    ));
    assert_eq!(direct_repos(&updated), ["billing-service"]);
}

// Verify producer catalog evidence comes from the selected Git snapshot.
#[test]
fn selects_producer_snapshots() {
    let workspace = Workspace::new("producer-revision", r#"openapi = "openapi.yaml""#);
    let producer = workspace.root.join("customer-service");
    workspace.write(
        "customer-service",
        "openapi.yaml",
        &OPENAPI.replace("/unused:", "/staged:"),
    );
    git(&producer, &["add", "openapi.yaml"]);
    workspace.write(
        "customer-service",
        "openapi.yaml",
        &OPENAPI.replace("/unused:", "/local:"),
    );

    let branch =
        json(workspace.query("/unused", &["--format=json", "--producer-revision", "main"]));
    assert_eq!(branch["in_catalog"], true);
    assert_eq!(branch["producer_revision"].as_str().unwrap().len(), 40);
    let staged = json(workspace.query(
        "/staged",
        &["--format=json", "--producer-revision", "STAGED"],
    ));
    assert_eq!(staged["in_catalog"], true);
    assert_eq!(staged["producer_evidence"]["revision"], "STAGED");
    let worktree = json(workspace.query(
        "/local",
        &["--format=json", "--producer-revision", "worktree"],
    ));
    assert_eq!(worktree["in_catalog"], true);
    assert_eq!(worktree["producer_evidence"]["revision"], "WORKTREE");
}

// Compare cold, warm and disabled cache output before testing evidence redaction.
#[test]
fn preserves_cache_equivalence_and_optional_report_details() {
    let workspace = Workspace::new("cache", r#"openapi = "openapi.yaml""#);
    let path = "/customers/:id";
    let cache_path = workspace.root.join("consumer-cache.sqlite3");
    assert!(!cache_path.exists());
    let cold = stdout(workspace.query(path, &["--format=json"]));
    assert!(cache_path.is_file());
    assert_eq!(cold, stdout(workspace.query(path, &["--format=json"])));
    assert_eq!(
        cold,
        stdout(workspace.query(path, &["--format=json", "--no-cache"]))
    );
    let full: Value = serde_json::from_str(&cold).unwrap();
    assert!(full["workspace"].get("repo_roots").is_none());
    assert!(
        !full["consumers"][0]["evidence"]["snippet"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert!(
        !full["unresolved_calls"][0]["evidence"]["snippet"]
            .as_str()
            .unwrap()
            .is_empty()
    );

    let portable = json(workspace.query(
        path,
        &["--format=json", "--no-snippets", "--include-repo-roots"],
    ));
    assert_eq!(portable["producer_evidence"]["snippet"], "");
    for collection in ["consumers", "unresolved_calls"] {
        for evidence in portable[collection].as_array().unwrap() {
            assert_eq!(evidence["evidence"]["snippet"], "");
            assert!(evidence["evidence"]["line"].as_u64().unwrap() > 0);
        }
    }
    let root = portable["workspace"]["repo_roots"]["customer-service"]
        .as_str()
        .unwrap();
    assert_eq!(
        fs::canonicalize(root).unwrap(),
        fs::canonicalize(workspace.root.join("customer-service")).unwrap()
    );
}

// Verify the query itself can anchor callers when the service has no catalog.
#[test]
fn finds_calls_for_services_without_a_catalog() {
    let workspace = Workspace::new("fallback", "");
    let found = json(workspace.query(
        "/customers/{id}",
        &["--format=json", "--config", "laine.toml"],
    ));
    assert_eq!(found["in_catalog"], false);
    assert_eq!(found["matched_endpoints"], serde_json::json!([]));
    assert!(found.get("producer_evidence").is_none());
    assert_eq!(
        direct_repos(&found),
        ["billing-service", "shipping-service", "shipping-service"]
    );
    let empty = json(workspace.query("/missing", &["--format=json"]));
    assert_eq!(empty["exit_code"], 0);
    assert_eq!(empty["consumers"], serde_json::json!([]));
}

// Prove catalog matching prefers exact identities and preserves ambiguous matches.
#[test]
fn resolves_optional_code_routes_and_deduplicates_overlapping_calls() {
    let workspace = Workspace::new("routes", r#"routes = { framework = "express" }"#);
    workspace.write(
        "customer-service",
        "app.ts",
        "const app = express();\napp.get('/customers/:id', handler);\napp.get('/customers/:id?', handler);\napp.get('/customers/123', handler);\n",
    );
    let opts = ["--format=json", "--producer-revision", "WORKTREE"];
    let concrete = json(workspace.query("/customers/42", &opts));
    assert_eq!(concrete["in_catalog"], true);
    let matches = concrete["matched_endpoints"].as_array().unwrap();
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0]["normalized_path"], "/customers/:id");
    assert_eq!(matches[1]["normalized_path"], "/customers/:id?");
    assert_eq!(concrete["producer_evidence"]["file"], "app.ts");
    assert_eq!(concrete["producer_evidence"]["line"], 2);
    assert_eq!(
        direct_repos(&concrete),
        ["billing-service", "shipping-service"]
    );

    let literal = json(workspace.query("/customers/123", &opts));
    assert_eq!(literal["matched_endpoints"].as_array().unwrap().len(), 1);
    assert_eq!(
        literal["matched_endpoints"][0]["normalized_path"],
        "/customers/123"
    );
    assert_eq!(direct_repos(&literal), ["shipping-service"]);

    let template = json(workspace.query("/customers/{id}", &opts));
    assert_eq!(template["matched_endpoints"].as_array().unwrap().len(), 1);
    let optional = json(workspace.query("/customers", &opts));
    assert_eq!(
        optional["matched_endpoints"][0]["normalized_path"],
        "/customers/:id?"
    );
}

// Assert structured error documents and their distinct usage and runtime exit codes.
#[test]
fn reports_usage_and_runtime_errors() {
    let workspace = Workspace::new("errors", r#"openapi = "openapi.yaml""#);
    workspace.write("customer-service", "openapi.yaml", "paths: [");
    for (args, code, diagnostic) in [
        (
            vec![
                "--service",
                "missing",
                "--method",
                "GET",
                "--path",
                "/unused",
                "--format=json",
            ],
            1,
            "invalid_workspace_config",
        ),
        (
            vec!["--method", "BOGUS", "--path", "/unused", "--format", "json"],
            64,
            "invalid_arguments",
        ),
        (
            vec!["--method", "GET", "--format=json"],
            64,
            "invalid_arguments",
        ),
        (
            vec![
                "--service",
                "customer-service",
                "--method",
                "GET",
                "--path",
                "/unused",
                "--producer-revision",
                "missing-ref",
                "--format=json",
            ],
            1,
            "git_snapshot_error",
        ),
        (
            vec![
                "--service",
                "customer-service",
                "--method",
                "GET",
                "--path",
                "/unused",
                "--producer-revision",
                "WORKTREE",
                "--format=json",
            ],
            1,
            "invalid_openapi_document",
        ),
    ] {
        let mut command = vec!["consumers"];
        command.extend(args);
        let output = workspace.run(&workspace.root, &command);
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        assert!(output.stderr.is_empty());
        let problem: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(problem["status"], "error");
        assert_eq!(problem["exit_code"], code);
        assert_eq!(problem["diagnostics"][0]["code"], diagnostic);
    }
    let output = workspace.run(
        &workspace.root,
        &["consumers", "--method", "BOGUS", "--path", "/unused"],
    );
    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("BOGUS"));
    assert!(
        stdout(workspace.run(&workspace.root, &["consumers", "--help"]))
            .contains("--producer-revision")
    );
}
