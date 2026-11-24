use std::fs;
use std::path::Path;
use std::process::Command;

use laine_cli::engine::{AnalyzeRequest, run_analysis};
use laine_cli::report::{render_json, render_terminal};
use laine_core::config::{ServiceConfig, WorkspaceConfig};
use laine_core::model::{Confidence, ContractChange};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-02-02T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-02-02T12:00:00Z")
        .output()
        .expect("git command should run");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.name", "Laine Test"]);
    git(dir, &["config", "user.email", "test@laine.local"]);
}

#[test]
fn propagates_indirect_possible_consumers_across_three_repos_with_cycles() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-indirect-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Repo A: customer-service (removes fullName)
    let customer = tmp.join("customer-service");
    init_repo(&customer);
    fs::write(
        customer.join("openapi.yaml"),
        r#"openapi: 3.0.3
paths:
  /customers/{id}:
    get:
      responses:
        '200':
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: { type: integer }
                  fullName: { type: string }
"#,
    )
    .unwrap();
    git(&customer, &["add", "."]);
    git(&customer, &["commit", "-m", "v1"]);

    git(&customer, &["checkout", "-b", "v2"]);
    fs::write(
        customer.join("openapi.yaml"),
        r#"openapi: 3.0.3
paths:
  /customers/{id}:
    get:
      responses:
        '200':
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: { type: integer }
                  firstName: { type: string }
"#,
    )
    .unwrap();
    git(&customer, &["add", "."]);
    git(&customer, &["commit", "-m", "v2"]);
    git(&customer, &["checkout", "main"]);

    // Repo B: billing-service (directly consumes customer-service.fullName)
    let billing = tmp.join("billing-service");
    init_repo(&billing);
    fs::create_dir_all(billing.join("src")).unwrap();
    fs::write(
        billing.join("src/invoice.ts"),
        r#"export async function makeInvoice(id: string) {
  const res = await fetch(`http://customer-service/customers/${id}`);
  const c = await res.json();
  return c.fullName;
}
"#,
    )
    .unwrap();
    git(&billing, &["add", "."]);
    git(&billing, &["commit", "-m", "init billing"]);

    // Repo C: accounting-service (consumes billing-service, so it is INDIRECTLY affected -> POSSIBLE)
    let accounting = tmp.join("accounting-service");
    init_repo(&accounting);
    fs::create_dir_all(accounting.join("src")).unwrap();
    fs::write(
        accounting.join("src/ledger.ts"),
        r#"export async function syncLedger(invoiceId: string) {
  const res = await fetch(`http://billing-service/invoices/${invoiceId}`);
  return res.json();
}
"#,
    )
    .unwrap();
    git(&accounting, &["add", "."]);
    git(&accounting, &["commit", "-m", "init accounting"]);

    // Introduce a cycle back from billing-service to accounting-service to prove cycle safety!
    fs::write(
        billing.join("src/sync.ts"),
        r#"export async function notifyAccounting(id: string) {
  return fetch(`http://accounting-service/entries/${id}`);
}
"#,
    )
    .unwrap();
    git(&billing, &["add", "."]);
    git(&billing, &["commit", "-m", "add cycle edge"]);

    // Config built via WorkspaceConfig::save_to_path (tests round-trip persistence!)
    let cfg_path = tmp.join("laine.toml");
    let cfg = WorkspaceConfig {
        version: 1,
        services: vec![
            ServiceConfig {
                name: "customer-service".into(),
                path: "./customer-service".into(),
                openapi: Some("openapi.yaml".into()),
                routes: None,
                base_urls: vec!["http://customer-service".into()],
            },
            ServiceConfig {
                name: "billing-service".into(),
                path: "./billing-service".into(),
                openapi: None,
                routes: None,
                base_urls: vec!["http://billing-service".into()],
            },
            ServiceConfig {
                name: "accounting-service".into(),
                path: "./accounting-service".into(),
                openapi: None,
                routes: None,
                base_urls: vec!["http://accounting-service".into()],
            },
        ],
        clients: Vec::new(),
        env: Default::default(),
        policy: Default::default(),
        cache_path: None,
    };
    cfg.save_to_path(&cfg_path)
        .expect("save_to_path should work");

    let report = run_analysis(&AnalyzeRequest {
        config_path: cfg_path,
        target_repo: "customer-service".into(),
        base_rev: "main".into(),
        head_rev: "v2".into(),
    })
    .expect("indirect analysis should succeed");

    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert!(matches!(
        &finding.change,
        ContractChange::ResponseFieldRemoved { field_path, .. } if field_path == "fullName"
    ));

    // Direct confirmed consumer: billing-service
    assert_eq!(finding.consumers.len(), 1);
    assert_eq!(finding.consumers[0].repo, "billing-service");
    assert_eq!(finding.consumers[0].confidence, Confidence::Confirmed);

    // Indirect possible consumer: accounting-service (via billing-service)
    assert_eq!(finding.indirect_consumers.len(), 1);
    let indirect = &finding.indirect_consumers[0];
    assert_eq!(indirect.repo, "accounting-service");
    assert_eq!(indirect.confidence, Confidence::Possible);
    assert_eq!(indirect.via_repo, "billing-service");
    assert_eq!(
        indirect.dependency_chain,
        vec!["billing-service", "accounting-service"]
    );

    // Golden JSON determinism & terminal rendering check
    let json = render_json(&report).unwrap();
    assert!(json.contains("\"schema_version\": 1"));
    assert!(json.contains("\"indirect_consumers\""));
    assert!(json.contains("\"POSSIBLE\""));

    let term = render_terminal(&report, 5);
    assert!(term.contains("Indirectly affected consumers (1 possible):"));
    assert!(term.contains("accounting-service [Possible] via billing-service"));

    let _ = fs::remove_dir_all(&tmp);
}
