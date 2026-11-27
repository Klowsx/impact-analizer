use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use laine_core::config::WorkspaceConfig;
use laine_core::fixtures::{DemoWorkspacePaths, create_demo_workspace};
use laine_core::workspace::WorkspaceRegistry;
use serde_json::{Value, json};

const PRODUCER: &str = "customer-service";
const HEAD: &str = "feature/split-customer-name";

struct TestDirectory(PathBuf);

impl TestDirectory {
    // Give each test an isolated workspace and registry.
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("laine-mcp-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    // Materialize the committed fixture history shared with the CLI tests.
    fn demo(&self) -> DemoWorkspacePaths {
        create_demo_workspace(&self.0.join("workspace")).unwrap()
    }

    // Start the real binary with a private user-state directory.
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_laine"));
        command
            .current_dir(&self.0)
            .env("LAINE_HOME", self.0.join("state"));
        command
    }
}

impl Drop for TestDirectory {
    // Clean up workspace and registry files after the child has exited.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct McpClient {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl McpClient {
    // Open both sides of the stdio transport for an interactive session.
    fn start(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
        }
    }

    // Send a raw CRLF frame to exercise cross-platform line framing.
    fn send(&mut self, frame: &str) {
        write!(self.input.as_mut().unwrap(), "{frame}\r\n").unwrap();
    }

    // Read the response before sending more requests, proving the server flushes it.
    fn receive(&mut self) -> Value {
        let mut line = String::new();
        assert_ne!(
            self.output.read_line(&mut line).unwrap(),
            0,
            "unexpected EOF"
        );
        let response: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("non-JSON-RPC stdout: {error}: {line:?}"));
        assert_eq!(response["jsonrpc"], "2.0");
        response
    }

    // Send an identified request and make sure the exact ID comes back.
    fn request(&mut self, id: Value, method: &str, params: Value) -> Value {
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        let response = self.receive();
        assert_eq!(response["id"], id);
        response
    }

    // Call a tool and preserve the outer JSON-RPC response for error assertions.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            json!(5),
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        )
    }

    // Close stdin, verify that stdout is empty, and reap the server.
    fn finish(mut self) {
        self.input.take();
        let mut rest = String::new();
        self.output.read_to_string(&mut rest).unwrap();
        assert!(self.child.wait().unwrap().success());
        assert!(rest.is_empty(), "unexpected trailing stdout: {rest:?}");
    }
}

impl Drop for McpClient {
    // Reap the server even when an assertion fails before normal EOF.
    fn drop(&mut self) {
        if self.input.take().is_some() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// Run the CLI report producer and decode its canonical JSON document.
fn cli_json(mut command: Command, exit_code: i32) -> Value {
    let output = command.arg("--format=json").output().unwrap();
    assert_eq!(output.status.code(), Some(exit_code), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

// Assert the common result shape and return its structured report.
fn success(response: &Value) -> &Value {
    let result = &response["result"];
    assert_eq!(result["isError"], false, "{response:#}");
    assert_eq!(result["content"][0]["type"], "text");
    assert!(!result["content"][0]["text"].as_str().unwrap().is_empty());
    &result["structuredContent"]
}

// Confirm every serialized evidence snippet is blank or omitted.
fn assert_no_snippets(value: &Value) {
    match value {
        Value::Object(fields) => {
            if let Some(snippet) = fields.get("snippet") {
                assert_eq!(*snippet, "");
            }
            fields.values().for_each(assert_no_snippets);
        }
        Value::Array(items) => items.iter().for_each(assert_no_snippets),
        _ => {}
    }
}

// Exercise negotiation, notification handling, tool discovery, and graceful shutdown.
#[test]
fn negotiates_mcp_and_lists_read_only_tools() {
    let temp = TestDirectory::new();
    let mut command = temp.command();
    command.arg("mcp");
    let mut mcp = McpClient::start(command);

    for (offered, expected) in [
        ("2025-06-18", "2025-06-18"),
        ("2025-03-26", "2025-03-26"),
        ("2024-11-05", "2024-11-05"),
        ("2099-01-01", "2025-03-26"),
    ] {
        let response = mcp.request(
            json!(offered),
            "initialize",
            json!({"protocolVersion": offered, "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}),
        );
        assert_eq!(response["result"]["protocolVersion"], expected);
        assert_eq!(
            response["result"]["capabilities"],
            json!({"tools": {"listChanged": false}})
        );
        assert_eq!(
            response["result"]["serverInfo"],
            json!({"name": "laine", "version": env!("CARGO_PKG_VERSION")})
        );
    }

    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    mcp.send(r#"{"jsonrpc":"2.0","method":"unknown_notification"}"#);
    assert_eq!(
        mcp.request(json!(1), "ping", json!({}))["result"],
        json!({})
    );
    let response = mcp.request(json!(2), "tools/list", json!({}));
    let tools = response["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    for (tool, name) in tools.iter().zip([
        "laine_consumers",
        "laine_analyze",
        "laine_explain_policy",
        "laine_workspace_validate",
    ]) {
        assert_eq!(tool["name"], name);
        assert_eq!(
            tool["annotations"],
            json!({
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            })
        );
        assert!(!tool["description"].as_str().unwrap().is_empty());
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    }
    assert_eq!(
        tools[0]["inputSchema"]["required"],
        json!(["method", "path"])
    );
    mcp.finish();
}

// Verify CLI parity and prove that session privacy and cache flags override tool inputs.
#[test]
fn returns_canonical_reports_and_enforces_read_only_session_flags() {
    let temp = TestDirectory::new();
    let paths = temp.demo();
    let cache = paths.root.join("mcp-cache.sqlite3");
    let mut config = WorkspaceConfig::load_from_path(&paths.config_path).unwrap();
    config.cache_path = Some(cache.clone());
    config.policy.ignore_endpoints = vec!["GET /internal/*".into()];
    config.save_to_path(&paths.config_path).unwrap();

    let mut command = temp.command();
    command
        .args(["mcp", "--no-cache", "--no-snippets", "--config"])
        .arg(&paths.config_path);
    let mut mcp = McpClient::start(command);

    let mut cli = temp.command();
    cli.args(["analyze", "--config"])
        .arg(&paths.config_path)
        .args([
            "--repo",
            PRODUCER,
            "--base",
            "main",
            "--head",
            HEAD,
            "--no-cache",
            "--no-snippets",
            "--include-repo-roots",
            "--consumer-revision",
            "worktree",
        ]);
    let response = mcp.call("laine_analyze", json!({
        "repo": PRODUCER, "base": "main", "head": HEAD,
        "no_cache": false, "no_snippets": false, "include_repo_roots": true, "consumer_revision": "worktree"
    }));
    let analyzed = success(&response);
    assert_eq!(*analyzed, cli_json(cli, 2));
    assert_eq!(analyzed["exit_code"], 2);
    assert_eq!(analyzed["policy"]["should_fail"], true);
    assert!(!analyzed["findings"].as_array().unwrap().is_empty());
    assert_no_snippets(analyzed);

    let mut cli = temp.command();
    cli.args(["consumers", "--config"])
        .arg(&paths.config_path)
        .args([
            "--service",
            PRODUCER,
            "--method",
            "GET",
            "--path",
            "/customers/42",
            "--field",
            "fullName",
            "--producer-revision",
            "main",
            "--no-cache",
            "--no-snippets",
        ]);
    let response = mcp.call(
        "laine_consumers",
        json!({
            "service": PRODUCER, "method": "GET", "path": "/customers/42", "field": "fullName",
            "producer_revision": "main", "no_cache": false, "no_snippets": false
        }),
    );
    let consumers = success(&response);
    assert_eq!(*consumers, cli_json(cli, 0));
    assert_no_snippets(consumers);

    let mut cli = temp.command();
    cli.args(["workspace", "validate", "--config"])
        .arg(&paths.config_path);
    assert_eq!(
        *success(&mcp.call("laine_workspace_validate", json!({}))),
        cli_json(cli, 0)
    );

    let response = mcp.call(
        "laine_explain_policy",
        json!({
            "endpoint": {"method": "GET", "path": "internal/metrics/"},
            "repo": PRODUCER, "base": "main", "head": HEAD, "no_cache": false
        }),
    );
    let explanation = success(&response);
    assert_eq!(explanation["schema_version"], 1);
    assert_eq!(
        explanation["endpoint_check"],
        json!({
            "method": "GET", "normalized_path": "/internal/metrics", "ignored": true,
            "matched_patterns": ["GET /internal/*"]
        })
    );
    assert_eq!(
        explanation["policy"]["ignore_endpoints"],
        json!(["GET /internal/*"])
    );
    let evaluation = &explanation["evaluation"];
    assert_eq!(evaluation["exit_code"], 2);
    assert_eq!(evaluation["should_fail"], true);
    assert_eq!(evaluation["reason"], analyzed["policy"]["reason"]);
    assert_eq!(
        evaluation["coverage_complete"],
        analyzed["coverage"]["complete"]
    );
    assert_eq!(
        evaluation["blocking_findings"],
        analyzed["policy"]["blocking_findings"]
    );
    assert_eq!(
        evaluation["finding_decisions"][0]["id"],
        analyzed["findings"][0]["id"]
    );
    assert_eq!(evaluation["finding_decisions"][0]["status"], "blocking");
    assert!(
        !evaluation["unresolved_calls"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_no_snippets(evaluation);

    mcp.finish();
    for path in [
        &cache,
        &paths.root.join("mcp-cache.sqlite3-wal"),
        &paths.root.join("mcp-cache.sqlite3-shm"),
        &temp.0.join("state"),
    ] {
        assert!(!path.exists(), "MCP wrote {}", path.display());
    }
}

// Cover workspace selection from cwd, named registry entries, and explicit config overrides.
#[test]
fn resolves_per_call_workspace_selection_and_explains_thresholds() {
    let temp = TestDirectory::new();
    let paths = temp.demo();
    let state = temp.0.join("state");
    fs::create_dir(&state).unwrap();
    let registry_path = state.join("workspaces.toml");
    let mut registry = WorkspaceRegistry::empty();
    registry
        .register("demo", paths.config_path.clone(), false)
        .unwrap();
    registry.save_to_path(&registry_path).unwrap();
    let mut config = WorkspaceConfig::load_from_path(&paths.config_path).unwrap();
    config.policy.fail_on = laine_core::model::Severity::Critical;
    config.save_to_path(&paths.config_path).unwrap();

    let mut command = temp.command();
    command.args(["mcp", "--config", "nonexistent.toml", "--no-cache"]);
    let mut mcp = McpClient::start(command);
    let response = mcp.call(
        "laine_explain_policy",
        json!({
            "config": "laine.toml", "cwd": paths.root,
            "endpoint": {"method": "GET", "path": "/customers/{id}"}
        }),
    );
    let policy = success(&response);
    assert_eq!(policy["policy"]["fail_on"], "CRITICAL");
    assert_eq!(policy["endpoint_check"]["ignored"], false);
    assert!(policy.get("evaluation").is_none());

    let response = mcp.call(
        "laine_explain_policy",
        json!({
            "workspace": "demo", "repo": PRODUCER, "base": "main", "head": HEAD
        }),
    );
    let evaluation = &success(&response)["evaluation"];
    assert_eq!(evaluation["should_fail"], false);
    assert_eq!(evaluation["blocking_findings"], 0);
    assert_eq!(evaluation["finding_decisions"][0]["status"], "non_blocking");

    let mut cli = temp.command();
    cli.args(["workspace", "validate", "--all", "--strict"]);
    assert_eq!(
        *success(&mcp.call(
            "laine_workspace_validate",
            json!({"all": true, "strict": true})
        )),
        cli_json(cli, 0),
    );

    mcp.finish();

    let mut command = temp.command();
    command.args(["mcp", "--no-cache"]);
    let mut mcp = McpClient::start(command);
    let mut cli = temp.command();
    cli.current_dir(&paths.customer_service)
        .args(["check", "--staged", "--no-cache"]);
    let response = mcp.call(
        "laine_analyze",
        json!({"cwd": paths.customer_service, "staged": true}),
    );
    assert_eq!(*success(&response), cli_json(cli, 0));
    mcp.finish();
}

// Keep malformed messages, invalid parameters, and sanitized runtime errors distinct.
#[test]
fn reports_protocol_and_runtime_errors_without_echoing_raw_input() {
    let temp = TestDirectory::new();
    let config = temp.0.join("invalid.toml");
    let private_input = "private-marker";
    fs::write(&config, format!("version = [{private_input}")).unwrap();
    let mut command = temp.command();
    command.args(["mcp", "--config"]).arg(&config);
    let mut mcp = McpClient::start(command);

    mcp.send("{ not JSON");
    let parse_error = mcp.receive();
    assert_eq!(parse_error["error"]["code"], -32700);
    assert_eq!(parse_error["id"], Value::Null);
    mcp.send(r#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#);
    assert_eq!(mcp.receive()["error"]["code"], -32600);
    assert_eq!(
        mcp.request(json!(3), "missing", json!({}))["error"]["code"],
        -32601
    );
    assert_eq!(
        mcp.request(json!(4), "initialize", json!({"protocolVersion": 7}))["error"]["code"],
        -32602
    );
    for (name, args) in [
        ("missing", json!({})),
        ("laine_analyze", json!(null)),
        ("laine_analyze", json!({"staged": "true"})),
        ("laine_analyze", json!({"consumer_revision": "main"})),
        ("laine_consumers", json!({"method": "GET", "path": ""})),
        (
            "laine_consumers",
            json!({"method": "CONNECT", "path": "/x"}),
        ),
        ("laine_consumers", json!({"path": "/x"})),
        (
            "laine_consumers",
            json!({"method": "GET", "path": "/x", "extra": true}),
        ),
        (
            "laine_explain_policy",
            json!({"endpoint": {"method": "GET", "path": "/x", "extra": true}}),
        ),
        (
            "laine_workspace_validate",
            json!({"all": true, "name": "demo"}),
        ),
        (
            "laine_workspace_validate",
            json!({"all": true, "config": "laine.toml"}),
        ),
    ] {
        assert_eq!(mcp.call(name, args)["error"]["code"], -32602, "{name}");
    }

    let mut cli = temp.command();
    cli.args(["analyze", "--config"])
        .arg(&config)
        .args(["--repo", PRODUCER]);
    let reply = mcp.call("laine_analyze", json!({"repo": PRODUCER}));
    let result = &reply["result"];
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"], cli_json(cli, 1));
    assert_eq!(
        result["structuredContent"]["diagnostics"][0]["code"],
        "invalid_config_syntax"
    );
    assert!(!result.to_string().contains(private_input));
    mcp.finish();
}
