use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use laine_core::fixtures::create_demo_workspace;
use laine_core::model::{
    AffectedConsumer, AnalysisReport, CliErrorReport, Confidence, ContractChange,
    ContractDiscrepancy, DiscrepancyKind, EndpointId, Evidence, ExtractionDiagnostic,
    ExtractionFailure, ExtractionStage, HttpMethod, ImpactFinding, IndirectConsumer,
    ProducerEvidence, Severity, SourceSpan, UnresolvedCall,
};
use serde_json::{Value, json};

const SCHEMA: &str = include_str!("../../../schemas/ir-v1.schema.json");
const GOLDEN: &str = include_str!("fixtures/golden/json.snap");
const SPEC: &str = include_str!("../../../docs/ir-spec.md");
const FIXTURE_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

// Resolve the local definition references supported by the published schema.
fn resolve<'a>(root: &'a Value, reference: &str) -> &'a Value {
    let name = reference
        .strip_prefix("#/$defs/")
        .expect("IR v1 references must point to local definitions");
    root["$defs"]
        .get(name)
        .unwrap_or_else(|| panic!("unresolved schema reference {reference}"))
}

// Walk only reachable schema nodes so unused definitions cannot hide behind one another.
fn visit_definitions<'a>(root: &'a Value, rule: &'a Value, seen: &mut BTreeSet<&'a str>) {
    if let Some(reference) = rule["$ref"].as_str()
        && seen.insert(reference.strip_prefix("#/$defs/").unwrap())
    {
        visit_definitions(root, resolve(root, reference), seen);
    }
    if let Some(properties) = rule["properties"].as_object() {
        for property in properties.values() {
            visit_definitions(root, property, seen);
        }
    }
    for keyword in ["items", "additionalProperties"] {
        if let Some(child) = rule.get(keyword) {
            visit_definitions(root, child, seen);
        }
    }
    for keyword in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = rule[keyword].as_array() {
            for branch in branches {
                visit_definitions(root, branch, seen);
            }
        }
    }
}

// Match the single JSON Schema primitive types used by this validator.
fn matches_type(instance: &Value, expected: &str) -> bool {
    match expected {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "integer" => instance.as_f64().is_some_and(|value| value.fract() == 0.0),
        "number" => instance.is_number(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        other => panic!("unsupported schema type {other}"),
    }
}

// Match the anchored lowercase hexadecimal patterns used for Git object IDs.
fn matches_pattern(text: &str, pattern: &str) -> bool {
    let digits = match pattern {
        "^[0-9a-f]{40}$" => 40,
        "^[0-9a-f]{64}$" => 64,
        other => panic!("unsupported schema pattern {other}"),
    };
    text.len() == digits
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// Compare the scalar literals used by const and enum using JSON Schema numeric equality.
fn same_literal(left: &Value, right: &Value) -> bool {
    match (left.as_number(), right.as_number()) {
        (Some(left), Some(right)) if left.is_f64() || right.is_f64() => {
            left.as_f64() == right.as_f64()
        }
        _ => left == right,
    }
}

// Apply the Draft 2020-12 vocabulary used by the IR schema and report the instance path on failure.
fn validate(root: &Value, rule: &Value, instance: &Value, path: &str) -> Result<(), String> {
    let fail = |message: &str| Err(format!("{path}: {message}"));
    if let Some(allowed) = rule.as_bool() {
        return if allowed {
            Ok(())
        } else {
            fail("value is forbidden")
        };
    }

    for (keyword, constraint) in rule.as_object().expect("expected a schema object") {
        match keyword.as_str() {
            "$schema" | "$id" | "title" | "description" | "$defs" => {}
            "$ref" => validate(
                root,
                resolve(root, constraint.as_str().unwrap()),
                instance,
                path,
            )?,
            "type" => {
                if !matches_type(instance, constraint.as_str().unwrap()) {
                    return fail(&format!("expected type {constraint}"));
                }
            }
            "const" => {
                if !same_literal(instance, constraint) {
                    return fail(&format!("expected {constraint}"));
                }
            }
            "enum" => {
                if !constraint
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| same_literal(instance, value))
                {
                    return fail(&format!("expected one of {constraint}"));
                }
            }
            "pattern" => {
                let pattern = constraint.as_str().unwrap();
                if instance
                    .as_str()
                    .is_some_and(|text| !matches_pattern(text, pattern))
                {
                    return fail(&format!("does not match {pattern}"));
                }
            }
            "minimum" => {
                if instance
                    .as_f64()
                    .is_some_and(|value| value < constraint.as_f64().unwrap())
                {
                    return fail(&format!("expected value >= {constraint}"));
                }
            }
            "minLength" => {
                if instance.as_str().is_some_and(|text| {
                    (text.chars().count() as u64) < constraint.as_u64().unwrap()
                }) {
                    return fail(&format!("expected at least {constraint} characters"));
                }
            }
            "minItems" => {
                if instance
                    .as_array()
                    .is_some_and(|items| (items.len() as u64) < constraint.as_u64().unwrap())
                {
                    return fail(&format!("expected at least {constraint} items"));
                }
            }
            "required" => {
                if let Some(object) = instance.as_object() {
                    for name in constraint.as_array().unwrap() {
                        let name = name.as_str().unwrap();
                        if !object.contains_key(name) {
                            return fail(&format!("missing required property {name}"));
                        }
                    }
                }
            }
            "properties" | "additionalProperties" => {
                if let Some(object) = instance.as_object() {
                    for (name, value) in object {
                        let child_path = format!("{path}/{name}");
                        match (keyword.as_str(), rule["properties"].get(name)) {
                            ("properties", Some(property)) => {
                                validate(root, property, value, &child_path)?
                            }
                            ("additionalProperties", None) => {
                                validate(root, constraint, value, &child_path)?
                            }
                            _ => {}
                        }
                    }
                }
            }
            "items" => {
                if let Some(items) = instance.as_array() {
                    for (index, item) in items.iter().enumerate() {
                        validate(root, constraint, item, &format!("{path}/{index}"))?;
                    }
                }
            }
            "allOf" => {
                for branch in constraint.as_array().unwrap() {
                    validate(root, branch, instance, path)?;
                }
            }
            "oneOf" | "anyOf" => {
                let branches = constraint.as_array().unwrap();
                let errors: Vec<_> = branches
                    .iter()
                    .filter_map(|branch| validate(root, branch, instance, path).err())
                    .collect();
                let matches = branches.len() - errors.len();
                if matches == 0 || (keyword == "oneOf" && matches != 1) {
                    return fail(&format!(
                        "{keyword} matched {matches} branches ({})",
                        errors.join("; ")
                    ));
                }
            }
            other => panic!("unsupported schema keyword {other}"),
        }
    }
    Ok(())
}

// Validate a complete CLI output against the published root schema.
fn assert_valid(schema: &Value, report: &Value) {
    if let Err(error) = validate(schema, schema, report, "$") {
        panic!("report does not conform to IR v1: {error}");
    }
}

// Replace only the complete SHA placeholder values inserted by golden-output normalization.
fn golden_report() -> Value {
    serde_json::from_str(&GOLDEN.replace(r#""<SHA>""#, &format!("\"{FIXTURE_SHA}\""))).unwrap()
}

// Verify that the schema is Draft 2020-12 and all definitions resolve and are reached.
#[test]
fn schema_definitions_are_reachable() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    let mut seen = BTreeSet::new();
    visit_definitions(&schema, &schema, &mut seen);
    let declared = schema["$defs"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(seen, declared);
}

// Exercise assertions supported by the test validator even when the current schema omits them.
#[test]
fn validator_checks_combinators_and_keyword_boundaries() {
    let rule = json!({
        "allOf": [
            {"type": "string"},
            {"minLength": 2},
            {"anyOf": [{"enum": ["éx", "abc"]}, {"const": "abc"}]}
        ]
    });
    for (value, valid) in [("éx", true), ("abc", true), ("é", false), ("xy", false)] {
        assert_eq!(validate(&rule, &rule, &json!(value), "$").is_ok(), valid);
    }
    let one_of = json!({"oneOf": [{"type": "integer"}, {"minimum": 0}]});
    assert!(validate(&one_of, &one_of, &json!(-1), "$").is_ok());
    assert!(validate(&one_of, &one_of, &json!(0), "$").is_err());
    let integer = json!({"type": "integer", "const": 1});
    assert!(validate(&integer, &integer, &json!(1.0), "$").is_ok());
    assert!(validate(&integer, &integer, &json!(1.5), "$").is_err());
}

// Verify the committed JSON snapshot against the production schema.
#[test]
fn committed_golden_report_conforms() {
    let schema = serde_json::from_str(SCHEMA).unwrap();
    assert_valid(&schema, &golden_report());
}

// Validate the fenced JSON example from section 5 of the IR specification.
#[test]
fn documented_example_conforms() {
    let (_, section) = SPEC
        .split_once("\n## 5. ")
        .expect("section 5 of the IR spec");
    let section = section.split("\n## ").next().unwrap();
    let (_, fence) = section
        .split_once("```json")
        .expect("section 5 JSON example");
    let (example, _) = fence.split_once("```").expect("closing example fence");
    let schema = serde_json::from_str(SCHEMA).unwrap();
    assert_valid(&schema, &serde_json::from_str(example).unwrap());
}

struct Fixture(PathBuf);

impl Fixture {
    // Give each live test its own temporary workspace and CLI state.
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("laine-ir-schema-{}-{name}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    // Run analyze in JSON mode and verify stdout, stderr and the process exit code.
    fn analyze(&self, config: &Path, args: &[&str], expected_exit: i32) -> (String, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_laine"))
            .current_dir(&self.0)
            .env("LAINE_HOME", self.0.join("state"))
            .args(["analyze", "--format", "json", "--no-cache", "--config"])
            .arg(config)
            .args(args)
            .output()
            .expect("laine analyze should run");

        assert_eq!(output.status.code(), Some(expected_exit), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let report: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(report["exit_code"], expected_exit);
        (stdout, report)
    }
}

impl Drop for Fixture {
    // Clean up the fixture even if validation fails.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Run Git inside a temporary fixture and surface failures alongside the arguments.
fn run_git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
}

// Validate successful and policy-blocked analyses with committed and local Git snapshots.
#[test]
fn live_analysis_reports_conform() {
    let schema = serde_json::from_str(SCHEMA).unwrap();
    let fixture = Fixture::new("demo");
    let demo = create_demo_workspace(&fixture.0.join("demo")).unwrap();
    let (_, breaking) = fixture.analyze(
        &demo.config_path,
        &[
            "--repo",
            "customer-service",
            "--base",
            "main",
            "--head",
            "feature/split-customer-name",
        ],
        2,
    );
    assert_valid(&schema, &breaking);
    for field in ["base_rev", "head_rev"] {
        assert!(matches_pattern(
            breaking[field].as_str().unwrap(),
            "^[0-9a-f]{40}$"
        ));
    }
    assert!(!breaking["findings"].as_array().unwrap().is_empty());
    assert!(breaking["workspace"].get("repo_roots").is_none());
    assert_ne!(
        breaking["findings"][0]["consumers"][0]["evidence"]["snippet"],
        ""
    );

    let (_, compatible) = fixture.analyze(
        &demo.config_path,
        &[
            "--repo",
            "customer-service",
            "--base",
            "main",
            "--head",
            "main",
        ],
        0,
    );
    assert_valid(&schema, &compatible);
    assert_eq!(compatible["findings"], json!([]));

    run_git(
        &demo.customer_service,
        &[
            "checkout",
            "feature/split-customer-name",
            "--",
            "openapi.yaml",
        ],
    );
    for (argument, revision) in [("worktree", "WORKTREE"), ("staged", "STAGED")] {
        let (_, local) = fixture.analyze(
            &demo.config_path,
            &[
                "--repo",
                "customer-service",
                "--base",
                "main",
                "--head",
                argument,
                "--consumer-revision",
                argument,
                "--include-repo-roots",
                "--no-snippets",
            ],
            2,
        );
        assert_valid(&schema, &local);
        assert_eq!(local["head_rev"], revision);
        assert_eq!(local["consumer_revisions"]["billing-service"], revision);
        assert!(
            Path::new(
                local["workspace"]["repo_roots"]["customer-service"]
                    .as_str()
                    .unwrap()
            )
            .is_absolute()
        );
        let finding = &local["findings"][0];
        assert_eq!(finding["producer_evidence"]["description"], "");
        assert_eq!(finding["producer_evidence"]["snippet"], "");
        assert_eq!(finding["consumers"][0]["evidence"]["revision"], revision);
        assert_eq!(finding["consumers"][0]["evidence"]["snippet"], "");
        assert_eq!(local["unresolved_calls"][0]["evidence"]["snippet"], "");
    }
}

// Check real Git SHA-256 output so valid reports from these repositories remain accepted.
#[test]
fn live_sha256_revisions_conform() {
    let fixture = Fixture::new("sha256");
    run_git(&fixture.0, &["init", "--object-format=sha256"]);
    fs::write(
        fixture.0.join("openapi.yaml"),
        "openapi: 3.0.3\npaths: {}\n",
    )
    .unwrap();
    run_git(&fixture.0, &["add", "openapi.yaml"]);
    run_git(
        &fixture.0,
        &[
            "-c",
            "user.name=Laine Test",
            "-c",
            "user.email=test@laine.local",
            "commit",
            "-m",
            "seed contract",
        ],
    );
    let config = fixture.0.join("laine.toml");
    fs::write(
        &config,
        r#"version = 1
[[services]]
name = "api"
path = "."
openapi = "openapi.yaml"
"#,
    )
    .unwrap();

    let (_, report) = fixture.analyze(
        &config,
        &["--repo", "api", "--base", "HEAD", "--head", "HEAD"],
        0,
    );
    let schema = serde_json::from_str(SCHEMA).unwrap();
    assert_valid(&schema, &report);
    for revision in [
        &report["base_rev"],
        &report["head_rev"],
        &report["consumer_revisions"]["api"],
    ] {
        assert!(matches_pattern(
            revision.as_str().unwrap(),
            "^[0-9a-f]{64}$"
        ));
    }
}

// Verify the error schema and the established compact stdout payload for each failure path.
#[test]
fn live_cli_error_envelopes_conform() {
    let fixture = Fixture::new("errors");
    let missing = fixture.0.join("missing.toml");
    let invalid = fixture.0.join("invalid.toml");
    fs::write(&invalid, "version = [").unwrap();
    let schema = serde_json::from_str(SCHEMA).unwrap();

    for (config, args, exit, code, remediation, message) in [
        (
            missing.as_path(),
            &["--repo", "api"][..],
            1,
            "input_io_error",
            "check_input_paths",
            "An analysis input could not be read. Verify the configuration path and file access.",
        ),
        (
            invalid.as_path(),
            &["--repo", "api"][..],
            1,
            "invalid_config_syntax",
            "repair_workspace_toml",
            "The workspace TOML could not be parsed.",
        ),
        (
            missing.as_path(),
            &["--unknown-argument"][..],
            64,
            "invalid_arguments",
            "check_command_help",
            "Invalid analysis arguments. Run the command with --help.",
        ),
    ] {
        let (stdout, report) = fixture.analyze(config, args, exit);
        assert_valid(&schema, &report);
        let typed: CliErrorReport = serde_json::from_value(report).unwrap();
        assert_eq!(typed.diagnostics[0].code, code);
        assert_eq!(
            stdout.trim_end(),
            json!({
                "schema_version": 1,
                "status": "error",
                "exit_code": exit,
                "diagnostics": [{ "code": code, "remediation": remediation, "message": message }],
            })
            .to_string()
        );
    }
}

// Construct every ContractChange tag, including both shapes of body_type_changed.
fn contract_changes() -> Vec<ContractChange> {
    let methods = [
        HttpMethod::Get,
        HttpMethod::Post,
        HttpMethod::Put,
        HttpMethod::Patch,
        HttpMethod::Delete,
        HttpMethod::Head,
        HttpMethod::Options,
        HttpMethod::Trace,
    ];
    let endpoint = |index: usize| EndpointId {
        service: "api".into(),
        method: methods[index],
        normalized_path: "/orders/:id".into(),
    };
    vec![
        ContractChange::EndpointRemoved {
            endpoint: endpoint(0),
        },
        ContractChange::ResponseFieldRemoved {
            endpoint: endpoint(1),
            status_code: "200".into(),
            field_path: "name".into(),
            previous_type: "string".into(),
        },
        ContractChange::ResponseFieldTypeChanged {
            endpoint: endpoint(2),
            status_code: "200".into(),
            field_path: "name".into(),
            previous_type: "string".into(),
            new_type: "integer".into(),
        },
        ContractChange::ResponseFieldMadeOptional {
            endpoint: endpoint(3),
            status_code: "200".into(),
            field_path: "name".into(),
        },
        ContractChange::BodyTypeChanged {
            endpoint: endpoint(4),
            previous_type: "string".into(),
            new_type: "object".into(),
            status_code: Some("200".into()),
        },
        ContractChange::BodyTypeChanged {
            endpoint: endpoint(4),
            previous_type: "string".into(),
            new_type: "object".into(),
            status_code: None,
        },
        ContractChange::RequestFieldAddedRequired {
            endpoint: endpoint(5),
            field_path: "name".into(),
            field_type: "string".into(),
        },
        ContractChange::RequestFieldTypeChanged {
            endpoint: endpoint(6),
            field_path: "name".into(),
            previous_type: "integer".into(),
            new_type: "string".into(),
        },
        ContractChange::RequestParameterTypeChanged {
            endpoint: endpoint(7),
            location: "query".into(),
            name: "id".into(),
            previous_type: "string".into(),
            new_type: "integer".into(),
        },
        ContractChange::RequiredParameterAdded {
            endpoint: endpoint(0),
            location: "header".into(),
            name: "X-Tenant-Id".into(),
            param_type: "string".into(),
        },
    ]
}

// Serialize Rust report structs to exercise every change, diagnostic code and evidence shape.
#[test]
fn typed_analysis_report_conforms() {
    let schema = serde_json::from_str(SCHEMA).unwrap();
    let producer_evidence = ProducerEvidence {
        repo: "api".into(),
        revision: FIXTURE_SHA.into(),
        file: "openapi.yaml".into(),
        line: Some(4),
        span: Some(SourceSpan {
            line: 4,
            column: 3,
            end_line: 4,
            end_column: 9,
        }),
        json_pointer: Some("/paths".into()),
        ..ProducerEvidence::default()
    };
    let consumer_evidence = Evidence {
        repo: "web".into(),
        revision: "WORKTREE".into(),
        file: "index.ts".into(),
        line: 1,
        snippet: "fetch(url)".into(),
    };
    let findings: Vec<_> = contract_changes()
        .into_iter()
        .enumerate()
        .map(|(index, change)| ImpactFinding {
            id: format!("LAINE-{:03}", index + 1),
            change,
            severity: Severity::High,
            confidence: Confidence::Confirmed,
            producer_evidence: producer_evidence.clone(),
            producer_evidence_comparison: Some(ProducerEvidence {
                revision: "STAGED".into(),
                ..producer_evidence.clone()
            }),
            consumers: vec![AffectedConsumer {
                repo: "web".into(),
                evidence: consumer_evidence.clone(),
                confidence: Confidence::Likely,
                dependency_path: vec!["GET /orders/:id".into()],
            }],
            indirect_consumers: vec![IndirectConsumer {
                repo: "dashboard".into(),
                confidence: Confidence::Possible,
                via_repo: "web".into(),
                dependency_chain: vec!["web".into(), "dashboard".into()],
            }],
        })
        .collect();
    let diagnostics = [
        (ExtractionFailure::FileReadFailed, ExtractionStage::Consumer),
        (
            ExtractionFailure::FileParseFailed,
            ExtractionStage::Consumer,
        ),
        (
            ExtractionFailure::DataflowLimitReached,
            ExtractionStage::Consumer,
        ),
        (
            ExtractionFailure::RouteDeclarationUnresolved,
            ExtractionStage::ProducerBase,
        ),
        (
            ExtractionFailure::OpenApiReferenceUnresolved,
            ExtractionStage::ProducerHead,
        ),
        (
            ExtractionFailure::OpenApiReferenceCycle,
            ExtractionStage::ProducerHead,
        ),
    ]
    .into_iter()
    .map(|(code, stage)| ExtractionDiagnostic {
        repo: "api".into(),
        revision: "STAGED".into(),
        file: "openapi.yaml".into(),
        stage,
        code,
        line: Some(4),
        json_pointer: Some("/paths".into()),
        remediation: code.remediation().into(),
    })
    .collect();
    let discrepancies = [
        DiscrepancyKind::MissingInCode,
        DiscrepancyKind::UndocumentedInOpenApi,
    ]
    .into_iter()
    .map(|kind| ContractDiscrepancy {
        endpoint: findings[0].change.endpoint().clone(),
        kind,
        detail: "OpenAPI and code expose different routes".into(),
    })
    .collect();
    let report = AnalysisReport {
        schema_version: 1,
        target_repo: "api".into(),
        base_rev: FIXTURE_SHA.into(),
        head_rev: "STAGED".into(),
        policy: None,
        coverage: None,
        diagnostics,
        exit_code: None,
        workspace: None,
        consumer_revisions: Default::default(),
        stats: Default::default(),
        findings,
        unresolved_calls: vec![UnresolvedCall {
            repo: "web".into(),
            evidence: consumer_evidence,
            reason: "dynamic URL".into(),
        }],
        discrepancies,
    };
    assert_valid(&schema, &serde_json::to_value(report).unwrap());
}

// Show that values and fields outside the v1 contract are rejected at their instance paths.
#[test]
fn analysis_report_mutations_are_rejected() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let valid = golden_report();
    for (path, value) in [
        ("/schema_version", json!(2)),
        ("/head_rev", json!("main")),
        ("/base_rev", json!("a".repeat(39))),
        ("/consumer_revisions/billing-service", json!("a".repeat(41))),
        (
            "/findings/0/producer_evidence/revision",
            json!("a".repeat(40).to_uppercase()),
        ),
        ("/findings/0/change/endpoint/method", json!("CONNECT")),
        ("/findings/0/confidence", json!("confirmed")),
    ] {
        let mut invalid = valid.clone();
        *invalid.pointer_mut(path).unwrap() = value;
        let error = validate(&schema, &schema, &invalid, "$").unwrap_err();
        assert!(error.contains(path), "{error}");
    }

    let mut unknown = valid.clone();
    unknown["unexpected"] = json!(true);
    let error = validate(&schema, &schema, &unknown, "$").unwrap_err();
    assert!(error.contains("$/unexpected"), "{error}");

    let mut missing = valid;
    missing["findings"][0]["change"]
        .as_object_mut()
        .unwrap()
        .remove("previous_type");
    let error = validate(&schema, &schema, &missing, "$").unwrap_err();
    assert!(
        error.contains("missing required property previous_type"),
        "{error}"
    );
}

// Require a diagnostic and the error-only exit codes in every error envelope.
#[test]
fn error_report_mutations_are_rejected() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let valid = json!({
        "schema_version": 1,
        "status": "error",
        "exit_code": 64,
        "diagnostics": [{ "code": "invalid_arguments", "remediation": "check_command_help", "message": "Invalid analysis arguments." }],
    });
    for (property, replacement, expected) in [
        ("status", json!("success"), "$/status"),
        ("exit_code", json!(2), "$/exit_code"),
        ("diagnostics", json!([]), "$/diagnostics"),
    ] {
        let mut invalid = valid.clone();
        invalid[property] = replacement;
        let error = validate(&schema, &schema, &invalid, "$").unwrap_err();
        assert!(error.contains(expected), "{error}");
    }
}
