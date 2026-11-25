use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use laine_cli::engine::{AnalyzeRequest, run_analysis_with_options};
use laine_core::model::ContractChange;

// Ejecuta Git en el checkout sintético y devuelve la salida útil.
fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

// Verifica que los DTO homónimos y los cambios de tipo apunten a su objeto y revisión reales.
#[test]
fn cites_base_removals_and_both_type_definitions_in_json() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("laine-evidence-{}-{stamp}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let source = root.join("openapi.json");
    let config = root.join("laine.toml");
    let base = r##"{
  "openapi": "3.1.0",
  "paths": {
    "/gone": { "get": {} },
    "/right": { "get": { "responses": { "200": { "content": { "application/json": {
      "schema": { "$ref": "#/components/schemas/Right" }
    } } } } } }
  },
  "components": { "schemas": {
    "Other": { "properties": {
      "id": { "type": "integer" },
      "name": { "type": "integer" }
    } },
    "Right": { "properties": {
      "id": { "type": "string" },
      "name": { "type": "string" }
    } }
  } }
}"##;
    let head = base
        .replace("    \"/gone\": { \"get\": {} },\n", "")
        .replace("      \"id\": { \"type\": \"string\" },\n", "")
        .replace(
            "\"name\": { \"type\": \"string\" }",
            "\"name\": { \"type\": \"boolean\" }",
        );
    fs::write(&source, base).unwrap();
    fs::write(
        &config,
        "version = 1\n[[services]]\nname = 'api'\npath = '.'\nopenapi = 'openapi.json'\n",
    )
    .unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["config", "user.name", "Laine Test"]);
    git(&root, &["config", "user.email", "test@laine.local"]);
    git(&root, &["add", "openapi.json"]);
    git(&root, &["commit", "-m", "base contract"]);
    let base_sha = git(&root, &["rev-parse", "HEAD"]);
    fs::write(&source, &head).unwrap();

    let report = run_analysis_with_options(
        &AnalyzeRequest {
            config_path: config,
            target_repo: "api".into(),
            base_rev: "HEAD".into(),
            head_rev: "WORKTREE".into(),
        },
        true,
    )
    .unwrap();
    assert_eq!(report.findings.len(), 3);

    for finding in &report.findings {
        let evidence = &finding.producer_evidence;
        assert_eq!(evidence.revision, base_sha);
        assert_eq!(evidence.file, "openapi.json");
        assert!(evidence.snippet.is_empty());
        assert!(!evidence.description.is_empty());
        match &finding.change {
            ContractChange::EndpointRemoved { .. } => {
                assert_eq!(evidence.json_pointer.as_deref(), Some("/paths/~1gone/get"));
                assert_eq!(evidence.line, Some(4));
                assert!(finding.producer_evidence_comparison.is_none());
            }
            ContractChange::ResponseFieldRemoved { .. } => {
                assert_eq!(
                    evidence.json_pointer.as_deref(),
                    Some("/components/schemas/Right/properties/id")
                );
                assert_eq!(evidence.line, Some(15));
            }
            ContractChange::ResponseFieldTypeChanged { .. } => {
                let comparison = finding.producer_evidence_comparison.as_ref().unwrap();
                assert_eq!(
                    evidence.json_pointer.as_deref(),
                    Some("/components/schemas/Right/properties/name")
                );
                assert_eq!(evidence.line, Some(16));
                assert_eq!(comparison.revision, "WORKTREE");
                assert_eq!(comparison.json_pointer, evidence.json_pointer);
                assert_eq!(comparison.line, Some(14));
                assert_eq!(comparison.span.as_ref().unwrap().column, 7);
            }
            other => panic!("unexpected change: {other:?}"),
        }
    }
    fs::remove_dir_all(&root).unwrap();
}
