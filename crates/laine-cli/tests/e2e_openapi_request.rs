use std::fs;
use std::path::Path;
use std::process::Command;

use laine_cli::engine::{AnalyzeRequest, run_analysis};
use laine_core::model::{Confidence, ContractChange, HttpMethod, Severity};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-03-01T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-03-01T12:00:00Z")
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
fn detects_required_request_field_and_parameter_additions_affecting_ts_consumer() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-reqdiff-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let api_dir = tmp.join("order-api");
    init_repo(&api_dir);
    fs::write(
        api_dir.join("openapi.yaml"),
        r#"openapi: 3.0.3
paths:
  /orders:
    post:
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [customerId]
              properties:
                customerId:
                  type: string
      responses:
        '201':
          description: created
"#,
    )
    .unwrap();
    git(&api_dir, &["add", "."]);
    git(&api_dir, &["commit", "-m", "v1"]);

    git(&api_dir, &["checkout", "-b", "v2"]);
    fs::write(
        api_dir.join("openapi.yaml"),
        r#"openapi: 3.0.3
paths:
  /orders:
    post:
      parameters:
        - name: X-Idempotency-Key
          in: header
          required: true
          schema:
            type: string
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [customerId, currency]
              properties:
                customerId:
                  type: string
                currency:
                  type: string
                notes:
                  type: string
      responses:
        '201':
          description: created
"#,
    )
    .unwrap();
    git(&api_dir, &["add", "."]);
    git(
        &api_dir,
        &[
            "commit",
            "-m",
            "v2: require currency and idempotency header",
        ],
    );
    git(&api_dir, &["checkout", "main"]);

    let web_dir = tmp.join("checkout-web");
    init_repo(&web_dir);
    fs::create_dir_all(web_dir.join("src")).unwrap();
    fs::write(
        web_dir.join("src/checkout.ts"),
        r#"export async function submitOrder(customerId: string) {
  return fetch("http://order-api/orders", {
    method: "POST",
    body: JSON.stringify({ customerId }),
  });
}
"#,
    )
    .unwrap();
    git(&web_dir, &["add", "."]);
    git(&web_dir, &["commit", "-m", "add checkout consumer"]);

    let cfg_path = tmp.join("laine.toml");
    fs::write(
        &cfg_path,
        r#"version = 1

[[services]]
name = "order-api"
path = "./order-api"
openapi = "openapi.yaml"
base_urls = ["http://order-api"]

[[services]]
name = "checkout-web"
path = "./checkout-web"
"#,
    )
    .unwrap();

    let report = run_analysis(&AnalyzeRequest {
        config_path: cfg_path,
        target_repo: "order-api".into(),
        base_rev: "main".into(),
        head_rev: "v2".into(),
    })
    .expect("request-side diff analysis should succeed");

    // Expect 2 breaking changes (`currency` required request field + `X-Idempotency-Key` required header);
    // optional `notes` MUST NOT appear.
    assert_eq!(report.findings.len(), 2);
    assert!(report.findings.iter().any(|f| matches!(
        &f.change,
        ContractChange::RequestFieldAddedRequired { endpoint, field_path, .. }
            if endpoint.method == HttpMethod::Post && field_path == "currency"
    )));
    assert!(report.findings.iter().any(|f| matches!(
        &f.change,
        ContractChange::RequiredParameterAdded { location, name, .. }
            if location == "header" && name == "X-Idempotency-Key"
    )));

    for finding in &report.findings {
        assert_eq!(finding.severity, Severity::High);
        assert_eq!(finding.consumers.len(), 1);
        assert_eq!(finding.consumers[0].repo, "checkout-web");
        assert_eq!(finding.consumers[0].confidence, Confidence::Confirmed);
    }

    let _ = fs::remove_dir_all(&tmp);
}
