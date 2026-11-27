use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use laine_core::config::{PolicyConfig, WorkspaceConfig};
use laine_core::error::{LaineError, Result};
use laine_core::git::STAGED_REV;
use laine_core::model::{AnalysisReport, EndpointId, HttpMethod};
use laine_core::policy::{evaluate_policy, matching_ignore_patterns};
use laine_core::routes::normalize_route_path;
use laine_core::workspace::{ResolvedContext, WorkspaceRegistry, resolve_context_from_cwd};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::engine::{
    AnalyzeRequest, ConsumerRevision, ConsumersRequest, query_endpoint_consumers,
    run_analysis_with_consumer_revision,
};
use crate::report::{
    classify_laine_error, cli_error_report, omit_consumers_snippets, omit_snippets,
    render_consumers_markdown, render_markdown, render_workspace_validation_text,
    workspace_validation_report,
};

const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_PROTOCOL_VERSION: &str = "2025-03-26";
const HTTP_METHODS: [&str; 8] = [
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE",
];

#[derive(Debug, Default)]
pub struct ServerDefaults {
    pub config: Option<PathBuf>,
    pub workspace: Option<String>,
    pub no_snippets: bool,
    pub no_cache: bool,
}

#[derive(Deserialize)]
struct Selection {
    config: Option<PathBuf>,
    workspace: Option<String>,
    cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase", remote = "ConsumerRevision")]
enum Revision {
    Head,
    Staged,
    Worktree,
}

#[derive(Deserialize)]
struct DiffOptions {
    base: Option<String>,
    head: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    method: HttpMethod,
    path: String,
}

#[derive(Deserialize)]
struct ConsumersArgs {
    #[serde(flatten)]
    selection: Selection,
    method: HttpMethod,
    path: String,
    service: Option<String>,
    field: Option<String>,
    producer_revision: Option<String>,
    #[serde(flatten)]
    options: ReportOptions,
}

#[derive(Deserialize)]
struct AnalyzeArgs {
    #[serde(flatten)]
    selection: Selection,
    #[serde(flatten)]
    diff: DiffOptions,
    repo: Option<String>,
    #[serde(default)]
    staged: bool,
    #[serde(flatten)]
    options: ReportOptions,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ReportOptions {
    #[serde(with = "Revision")]
    consumer_revision: ConsumerRevision,
    no_cache: bool,
    no_snippets: bool,
    include_repo_roots: bool,
}

#[derive(Deserialize)]
struct ExplainArgs {
    #[serde(flatten)]
    selection: Selection,
    #[serde(flatten)]
    diff: DiffOptions,
    repo: Option<String>,
    endpoint: Option<Endpoint>,
    #[serde(default, with = "Revision")]
    consumer_revision: ConsumerRevision,
    #[serde(default)]
    no_cache: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ValidateArgs {
    name: Option<String>,
    config: Option<PathBuf>,
    all: bool,
    strict: bool,
    cwd: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum Tool {
    Consumers,
    Analyze,
    ExplainPolicy,
    WorkspaceValidate,
}

impl Tool {
    const ALL: [Self; 4] = [
        Self::Consumers,
        Self::Analyze,
        Self::ExplainPolicy,
        Self::WorkspaceValidate,
    ];

    // Use the same tool names for discovery and dispatch.
    fn name(self) -> &'static str {
        match self {
            Self::Consumers => "laine_consumers",
            Self::Analyze => "laine_analyze",
            Self::ExplainPolicy => "laine_explain_policy",
            Self::WorkspaceValidate => "laine_workspace_validate",
        }
    }

    // Describe each tool and the inputs it accepts.
    fn definition(self) -> Value {
        let mut properties = Map::new();
        let string = || json!({"type": "string"});
        let flag = || json!({"type": "boolean", "default": false});
        let method = || json!({"type": "string", "enum": HTTP_METHODS});
        let path = || json!({"type": "string", "minLength": 1});

        properties.extend([("config".into(), string()), ("cwd".into(), string())]);
        if !matches!(self, Self::WorkspaceValidate) {
            properties.extend([
                ("workspace".into(), string()),
                (
                    "consumer_revision".into(),
                    json!({
                        "type": "string", "enum": ["head", "staged", "worktree"], "default": "head"
                    }),
                ),
                ("no_cache".into(), flag()),
            ]);
        }
        if matches!(self, Self::Analyze | Self::Consumers) {
            properties.extend([
                ("no_snippets".into(), flag()),
                ("include_repo_roots".into(), flag()),
            ]);
        }
        if matches!(self, Self::Analyze | Self::ExplainPolicy) {
            properties.extend([
                ("repo".into(), string()),
                ("base".into(), json!({"type": "string", "default": "HEAD"})),
                (
                    "head".into(),
                    json!({"type": "string", "default": "WORKTREE"}),
                ),
            ]);
        }

        let (description, required): (&str, &[&str]) = match self {
            Self::Consumers => {
                properties.extend([
                    ("method".into(), method()),
                    ("path".into(), path()),
                    ("service".into(), string()),
                    ("field".into(), string()),
                    (
                        "producer_revision".into(),
                        json!({"type": "string", "default": "HEAD"}),
                    ),
                ]);
                (
                    "Find direct and indirect consumers of an HTTP endpoint or response field.",
                    &["method", "path"],
                )
            }
            Self::Analyze => {
                properties.insert("staged".into(), flag());
                (
                    "Analyze breaking API changes between two Git revisions and evaluate workspace policy.",
                    &[],
                )
            }
            Self::ExplainPolicy => {
                properties.insert(
                    "endpoint".into(),
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["method", "path"],
                        "properties": {"method": method(), "path": path()}
                    }),
                );
                (
                    "Inspect workspace policy, test ignore patterns for an endpoint, or explain the policy decision for a revision diff.",
                    &[],
                )
            }
            Self::WorkspaceValidate => {
                properties.extend([
                    ("name".into(), string()),
                    ("all".into(), json!({
                        "type": "boolean", "default": false,
                        "description": "Validate all registered workspaces. Cannot be combined with name or config."
                    })),
                    ("strict".into(), flag()),
                ]);
                (
                    "Validate Laine workspace configuration, Git checkouts, and producer sources.",
                    &[],
                )
            }
        };

        json!({
            "name": self.name(),
            "description": description,
            "inputSchema": {
                "type": "object", "properties": properties, "required": required,
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": true, "destructiveHint": false,
                "idempotentHint": true, "openWorldHint": false
            }
        })
    }

    // Validate optional fields before Serde applies defaults, then run the typed handler.
    fn call(self, arguments: Value, defaults: &ServerDefaults) -> Option<Value> {
        let definition = self.definition();
        let properties = definition["inputSchema"]["properties"].as_object()?;
        if arguments
            .as_object()?
            .iter()
            .any(|(key, value)| value.is_null() || !properties.contains_key(key))
        {
            return None;
        }
        let result = match self {
            Self::Consumers => {
                let args: ConsumersArgs = parse_args(arguments)?;
                if args.path.is_empty() {
                    return None;
                }
                consumers(args, defaults)
            }
            Self::Analyze => analyze(parse_args(arguments)?, defaults),
            Self::ExplainPolicy => {
                let args: ExplainArgs = parse_args(arguments)?;
                if args
                    .endpoint
                    .as_ref()
                    .is_some_and(|endpoint| endpoint.path.is_empty())
                {
                    return None;
                }
                explain_policy(args, defaults)
            }
            Self::WorkspaceValidate => {
                let args: ValidateArgs = parse_args(arguments)?;
                if args.all && (args.name.is_some() || args.config.is_some()) {
                    return None;
                }
                Ok(validate_workspace(args, defaults))
            }
        };
        Some(result.unwrap_or_else(|error| {
            let command = if matches!(self, Self::Consumers) {
                "Consumers"
            } else {
                "Analysis"
            };
            let (code, remediation, message) = classify_laine_error(command, &error);
            tool_result(
                message,
                json!(cli_error_report(1, code, remediation, message)),
                true,
            )
        }))
    }
}

// Discard parser diagnostics that may contain user input.
fn parse_args<T: DeserializeOwned>(arguments: Value) -> Option<T> {
    serde_json::from_value(arguments).ok()
}

/// Serve one JSON-RPC message per line, flushing responses before reading the next request.
pub fn serve(
    mut input: impl BufRead,
    mut output: impl Write,
    defaults: &ServerDefaults,
) -> io::Result<()> {
    let mut line = Vec::new();
    while input.read_until(b'\n', &mut line)? != 0 {
        let frame = line.strip_suffix(b"\n").unwrap_or(&line);
        let frame = frame.strip_suffix(b"\r").unwrap_or(frame);
        if let Some(response) = respond(frame, defaults) {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
        line.clear();
    }
    Ok(())
}

// Validate the envelope before dispatching requests and silently discard notifications.
fn respond(frame: &[u8], defaults: &ServerDefaults) -> Option<Value> {
    let Ok(request) = serde_json::from_slice::<Value>(frame) else {
        return Some(rpc_error(&Value::Null, -32700, "Parse error"));
    };
    let id = request.get("id");
    let method = request.get("method").and_then(Value::as_str);
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || method.is_none()
        || id.is_some_and(|id| !(id.is_string() || id.is_number() || id.is_null()))
    {
        return Some(rpc_error(&Value::Null, -32600, "Invalid Request"));
    }
    let id = id?;
    let params = request.get("params");
    let result = match method? {
        "initialize" => params.and_then(initialize),
        "ping" => params.is_none_or(Value::is_object).then(|| json!({})),
        "tools/list" => params
            .is_none_or(Value::is_object)
            .then(|| json!({"tools": Tool::ALL.map(Tool::definition)})),
        "tools/call" => params.and_then(|params| call_tool(params, defaults)),
        _ => return Some(rpc_error(id, -32601, "Method not found")),
    };

    Some(match result {
        Some(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        None => rpc_error(id, -32602, "Invalid params"),
    })
}

// Negotiate the requested version or fall back to the server's default.
fn initialize(params: &Value) -> Option<Value> {
    let requested = params.get("protocolVersion")?.as_str()?;
    let version = if PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        DEFAULT_PROTOCOL_VERSION
    };
    Some(json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "laine", "version": env!("CARGO_PKG_VERSION")}
    }))
}

// Look up a published tool and allow omitted arguments for tools with optional inputs.
fn call_tool(params: &Value, defaults: &ServerDefaults) -> Option<Value> {
    let name = params.get("name")?.as_str()?;
    let tool = Tool::ALL.into_iter().find(|tool| tool.name() == name)?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    tool.call(arguments, defaults)
}

// Wrap a machine-readable report and its text representation in an MCP tool result.
fn tool_result(text: impl Into<String>, report: Value, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": text.into()}],
        "structuredContent": report,
        "isError": is_error
    })
}

// Preserve the request identifier in JSON-RPC errors when one is known.
fn rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

impl Selection {
    // Resolve per-call paths using the supplied cwd while keeping startup paths server-relative.
    fn target<'a>(&'a self, defaults: &'a ServerDefaults) -> (Option<PathBuf>, Option<&'a str>) {
        if self.config.is_some() || self.workspace.is_some() {
            (
                self.config.as_ref().map(|path| match &self.cwd {
                    Some(cwd) => cwd.join(path),
                    None => path.clone(),
                }),
                self.workspace.as_deref(),
            )
        } else {
            (defaults.config.clone(), defaults.workspace.as_deref())
        }
    }

    // Resolve a workspace and its producer through the same context finder as the CLI.
    fn producer(&self, defaults: &ServerDefaults, repo: Option<&str>) -> Result<ResolvedContext> {
        let (config, workspace) = self.target(defaults);
        resolve_context_from_cwd(
            &self.cwd()?,
            config.as_deref(),
            workspace,
            repo,
            &WorkspaceRegistry::default_path()?,
        )
    }

    // Select the directory to use for workspace discovery.
    fn cwd(&self) -> Result<PathBuf> {
        self.cwd.clone().map_or_else(
            || {
                env::current_dir().map_err(|source| LaineError::Io {
                    path: ".".into(),
                    source,
                })
            },
            Ok,
        )
    }

    // Resolve config-only policy queries even if cwd is outside a producer checkout.
    fn policy_config_path(&self, defaults: &ServerDefaults) -> Result<PathBuf> {
        let (config, workspace) = self.target(defaults);
        if let Some(path) = config {
            return Ok(path);
        }
        let registry = WorkspaceRegistry::default_path()?;
        if let Some(name) = workspace {
            return WorkspaceRegistry::load_from_path(&registry)?
                .workspaces
                .remove(name)
                .ok_or_else(|| {
                    LaineError::ConfigInvalid("The selected workspace is not registered.".into())
                });
        }

        let cwd = self.cwd()?;
        let local = cwd.join("laine.toml");
        if local.is_file() {
            return Ok(local);
        }
        match resolve_context_from_cwd(&cwd, None, None, None, &registry) {
            Ok(context) => Ok(context.config_path),
            Err(error @ LaineError::ConfigInvalid(_)) => cwd
                .ancestors()
                .skip(1)
                .map(|dir| dir.join("laine.toml"))
                .find(|path| path.is_file())
                .ok_or(error),
            Err(error) => Err(error),
        }
    }
}

impl DiffOptions {
    // Build the analysis request with the same revision defaults as the CLI.
    fn request(self, context: ResolvedContext, staged: bool) -> AnalyzeRequest {
        AnalyzeRequest {
            config_path: context.config_path,
            target_repo: context.repo_name,
            base_rev: self.base.unwrap_or_else(|| "HEAD".into()),
            head_rev: if staged {
                STAGED_REV.into()
            } else {
                self.head.unwrap_or_else(|| "WORKTREE".into())
            },
        }
    }
}

// Query the selected producer snapshot and apply session privacy and cache settings.
fn consumers(args: ConsumersArgs, defaults: &ServerDefaults) -> Result<Value> {
    let context = args.selection.producer(defaults, args.service.as_deref())?;
    let mut report = query_endpoint_consumers(
        &ConsumersRequest {
            config_path: context.config_path,
            service: context.repo_name,
            method: args.method,
            path: args.path,
            field: args.field,
            producer_rev: args.producer_revision.unwrap_or_else(|| "HEAD".into()),
        },
        defaults.no_cache || args.options.no_cache,
        args.options.include_repo_roots,
        args.options.consumer_revision,
    )?;
    if defaults.no_snippets || args.options.no_snippets {
        omit_consumers_snippets(&mut report);
    }
    Ok(tool_result(
        render_consumers_markdown(&report),
        json!(report),
        false,
    ))
}

// Treat a policy block as a completed analysis and return the canonical report.
fn analyze(args: AnalyzeArgs, defaults: &ServerDefaults) -> Result<Value> {
    let context = args.selection.producer(defaults, args.repo.as_deref())?;
    let mut report = run_analysis_with_consumer_revision(
        &args.diff.request(context, args.staged),
        defaults.no_cache || args.options.no_cache,
        args.options.include_repo_roots,
        args.options.consumer_revision,
    )?;
    if defaults.no_snippets || args.options.no_snippets {
        omit_snippets(&mut report);
    }
    Ok(tool_result(render_markdown(&report), json!(report), false))
}

// Read effective policy and optionally explain an endpoint match and an analyzed diff.
fn explain_policy(args: ExplainArgs, defaults: &ServerDefaults) -> Result<Value> {
    let context = args
        .repo
        .as_deref()
        .map(|repo| args.selection.producer(defaults, Some(repo)))
        .transpose()?;
    let config_path = match &context {
        Some(context) => context.config_path.clone(),
        None => args.selection.policy_config_path(defaults)?,
    };
    let policy = WorkspaceConfig::load_from_path(&config_path)?.policy;
    let mut explanation = json!({
        "schema_version": 1,
        "config_path": config_path.display().to_string(),
        "policy": policy,
    });

    if let Some(endpoint) = args.endpoint {
        let endpoint = EndpointId {
            // Ignore patterns are scoped by HTTP method and path.
            service: String::new(),
            method: endpoint.method,
            normalized_path: normalize_route_path(&[], &endpoint.path),
        };
        let patterns = matching_ignore_patterns(&endpoint, &policy);
        explanation["endpoint_check"] = json!({
            "method": endpoint.method,
            "normalized_path": endpoint.normalized_path,
            "ignored": !patterns.is_empty(),
            "matched_patterns": patterns,
        });
    }

    if let Some(context) = context {
        let mut report = run_analysis_with_consumer_revision(
            &args.diff.request(context, false),
            defaults.no_cache || args.no_cache,
            false,
            args.consumer_revision,
        )?;
        if defaults.no_snippets {
            omit_snippets(&mut report);
        }
        explanation["evaluation"] = policy_evaluation(&report, &policy);
    }
    let text = serde_json::to_string_pretty(&explanation).expect("Policy explanation is JSON");
    Ok(tool_result(text, explanation, false))
}

// Explain the disposition of findings retained by the engine's ignore filter.
fn policy_evaluation(report: &AnalysisReport, policy: &PolicyConfig) -> Value {
    let evaluation = evaluate_policy(report, policy);
    let threshold = json!(policy.fail_on);
    let decisions: Vec<_> = report.findings.iter().map(|finding| {
        let endpoint = finding.change.endpoint();
        let blocking = finding.severity >= policy.fail_on;
        let status = if blocking { "blocking" } else { "non_blocking" };
        let comparison = if blocking { ">=" } else { "<" };
        let severity = json!(finding.severity);
        json!({
            "id": finding.id,
            "endpoint": format!("{} {}", endpoint.method.as_str(), endpoint.normalized_path),
            "severity": severity,
            "status": status,
            "reason": format!("severity {} {comparison} fail_on {}", severity.as_str().unwrap_or_default(), threshold.as_str().unwrap_or_default()),
        })
    }).collect();

    json!({
        "should_fail": evaluation.should_fail,
        "exit_code": evaluation.exit_code,
        "blocking_findings": evaluation.blocking_findings,
        "non_blocking_findings": decisions.len() - evaluation.blocking_findings,
        "blocking_unresolved_calls": evaluation.blocking_unresolved_calls,
        "unresolved_calls": report.unresolved_calls,
        "coverage_complete": report.coverage.as_ref().map(|coverage| coverage.complete),
        "reason": evaluation.reason,
        "finding_decisions": decisions,
    })
}

// Validate the requested target, allowing explicit calls to override session defaults.
fn validate_workspace(args: ValidateArgs, defaults: &ServerDefaults) -> Value {
    let selection = Selection {
        config: args.config,
        workspace: args.name,
        cwd: args.cwd,
    };
    let (config, name) = if args.all {
        (None, None)
    } else {
        selection.target(defaults)
    };
    let report = workspace_validation_report(
        config.as_deref(),
        name,
        args.all,
        selection.cwd().ok().as_deref(),
        WorkspaceRegistry::default_path().ok().as_deref(),
        args.strict,
    );
    tool_result(
        render_workspace_validation_text(&report, true),
        json!(report),
        false,
    )
}
