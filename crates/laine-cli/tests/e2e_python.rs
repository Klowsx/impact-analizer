use std::fs;
use std::path::Path;
use std::process::Command;

use laine_cli::engine::{AnalyzeRequest, run_analysis};
use laine_core::model::{Confidence, ContractChange, HttpMethod, Severity};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-03-03T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-03-03T12:00:00Z")
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
fn detects_fastapi_producer_route_removal_affecting_typescript_consumer() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-fastapi-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Producer: Python FastAPI service removing GET /api/v1/customers/{id}
    let py_api = tmp.join("customer-fastapi");
    init_repo(&py_api);
    fs::write(
        py_api.join("main.py"),
        r#"from fastapi import FastAPI, APIRouter

app = FastAPI()
router = APIRouter(prefix="/customers")

@router.get("/{id}")
def get_customer(id: int):
    return {"id": id, "fullName": "Jane"}

@router.get("/")
def list_customers():
    return []

app.include_router(router, prefix="/api/v1")
"#,
    )
    .unwrap();
    git(&py_api, &["add", "."]);
    git(&py_api, &["commit", "-m", "v1"]);

    git(&py_api, &["checkout", "-b", "v2"]);
    fs::write(
        py_api.join("main.py"),
        r#"from fastapi import FastAPI, APIRouter

app = FastAPI()
router = APIRouter(prefix="/customers")

@router.get("/")
def list_customers():
    return []

app.include_router(router, prefix="/api/v1")
"#,
    )
    .unwrap();
    git(&py_api, &["add", "."]);
    git(&py_api, &["commit", "-m", "v2: remove get_customer"]);
    git(&py_api, &["checkout", "main"]);

    // Consumer: TypeScript web app calling the FastAPI endpoint
    let ts_web = tmp.join("admin-web");
    init_repo(&ts_web);
    fs::create_dir_all(ts_web.join("src")).unwrap();
    fs::write(
        ts_web.join("src/customer.ts"),
        r#"export async function fetchCustomer(id: string) {
  const res = await fetch(`http://customer-fastapi/api/v1/customers/${id}`);
  return res.json();
}
"#,
    )
    .unwrap();
    git(&ts_web, &["add", "."]);
    git(&ts_web, &["commit", "-m", "add admin consumer"]);

    let cfg_path = tmp.join("laine.toml");
    fs::write(
        &cfg_path,
        r#"version = 1

[[services]]
name = "customer-fastapi"
path = "./customer-fastapi"
base_urls = ["http://customer-fastapi"]
[services.routes]
framework = "fastapi"

[[services]]
name = "admin-web"
path = "./admin-web"
"#,
    )
    .unwrap();

    let report = run_analysis(&AnalyzeRequest {
        config_path: cfg_path,
        target_repo: "customer-fastapi".into(),
        base_rev: "main".into(),
        head_rev: "v2".into(),
    })
    .expect("FastAPI -> TS analysis should succeed");

    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(finding.severity, Severity::Critical);
    assert!(matches!(
        &finding.change,
        ContractChange::EndpointRemoved { endpoint }
            if endpoint.method == HttpMethod::Get
                && endpoint.normalized_path == "/api/v1/customers/:id"
    ));
    assert_eq!(finding.consumers.len(), 1);
    assert_eq!(finding.consumers[0].repo, "admin-web");
    assert_eq!(finding.consumers[0].confidence, Confidence::Confirmed);

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn detects_openapi_field_removal_affecting_python_requests_consumer() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-py-consumer-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Producer: OpenAPI service removing `fullName`
    let api_dir = tmp.join("customer-api");
    init_repo(&api_dir);
    fs::write(
        api_dir.join("openapi.yaml"),
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
                  address: { type: string }
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
                  address: { type: string }
"#,
    )
    .unwrap();
    git(&api_dir, &["add", "."]);
    git(&api_dir, &["commit", "-m", "v2"]);
    git(&api_dir, &["checkout", "main"]);

    // Python consumer 1: reads `customer["fullName"]` + has a dynamic URL (`UNKNOWN`)
    let py_billing = tmp.join("billing-worker");
    init_repo(&py_billing);
    fs::write(
        py_billing.join("worker.py"),
        r#"import requests

def generate_invoice(customer_id: str) -> str:
    res = requests.get(f"http://customer-api/customers/{customer_id}")
    customer = res.json()
    return f"Invoice for {customer['fullName']}"

def sync_legacy(url: str):
    return requests.get(url)
"#,
    )
    .unwrap();
    git(&py_billing, &["add", "."]);
    git(&py_billing, &["commit", "-m", "add python billing worker"]);

    // Python consumer 2: reads ONLY `customer.get("address")` (negative case)
    let py_shipping = tmp.join("shipping-worker");
    init_repo(&py_shipping);
    fs::write(
        py_shipping.join("labels.py"),
        r#"import requests

def make_label(customer_id: str) -> str:
    res = requests.get(f"http://customer-api/customers/{customer_id}")
    customer = res.json()
    return customer.get("address", "")
"#,
    )
    .unwrap();
    git(&py_shipping, &["add", "."]);
    git(
        &py_shipping,
        &["commit", "-m", "add python shipping worker"],
    );

    let cfg_path = tmp.join("laine.toml");
    fs::write(
        &cfg_path,
        r#"version = 1

[[services]]
name = "customer-api"
path = "./customer-api"
openapi = "openapi.yaml"
base_urls = ["http://customer-api"]

[[services]]
name = "billing-worker"
path = "./billing-worker"

[[services]]
name = "shipping-worker"
path = "./shipping-worker"
"#,
    )
    .unwrap();

    let report = run_analysis(&AnalyzeRequest {
        config_path: cfg_path,
        target_repo: "customer-api".into(),
        base_rev: "main".into(),
        head_rev: "v2".into(),
    })
    .expect("OpenAPI -> Python analysis should succeed");

    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(finding.consumers.len(), 1);
    assert_eq!(finding.consumers[0].repo, "billing-worker");
    assert_eq!(finding.consumers[0].evidence.file, "worker.py");
    assert_eq!(finding.consumers[0].evidence.line, 6);
    assert_eq!(finding.consumers[0].confidence, Confidence::Confirmed);

    // Negative case: shipping-worker MUST NOT be flagged
    assert!(
        !finding
            .consumers
            .iter()
            .any(|c| c.repo == "shipping-worker")
    );

    // Dynamic URL reported as UNKNOWN
    assert_eq!(report.unresolved_calls.len(), 1);
    assert_eq!(report.unresolved_calls[0].repo, "billing-worker");
    assert_eq!(report.unresolved_calls[0].evidence.file, "worker.py");

    let _ = fs::remove_dir_all(&tmp);
}
