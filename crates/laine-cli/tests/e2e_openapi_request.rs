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
        let argument = match &finding.change {
            ContractChange::RequestFieldAddedRequired { .. } => r#"body argument "currency""#,
            ContractChange::RequiredParameterAdded { .. } => {
                r#"header argument "X-Idempotency-Key""#
            }
            change => panic!("unexpected change: {change:?}"),
        };
        assert_eq!(
            finding.consumers[0].evidence.snippet,
            format!("Incompatible HTTP {argument}")
        );
    }

    let _ = fs::remove_dir_all(&tmp);
}

// Check request arguments in TS, Python, and Vue, including query array serialization.
#[test]
fn checks_request_arguments_before_attributing_impact() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-arguments-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let api = tmp.join("api");
    let consumer = tmp.join("consumer");
    init_repo(&api);
    init_repo(&consumer);

    let base = r#"
openapi: 3.1.0
paths:
  /orders:
    post:
      parameters:
        - {in: query, name: limit, schema: {type: integer}}
        - {in: header, name: X-Mode, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                tenant: {type: string}
  /search:
    get:
      parameters:
        - {in: query, name: tags, schema: {type: array, items: {type: string}}}
"#;
    fs::write(api.join("openapi.yaml"), base).unwrap();
    git(&api, &["add", "."]);
    git(&api, &["commit", "-m", "test: optional request arguments"]);
    fs::write(
        api.join("openapi.yaml"),
        base.replace("name: limit,", "name: limit, required: true,")
            .replace("name: X-Mode,", "name: X-Mode, required: true,")
            .replace("name: tags,", "name: tags, required: true,")
            .replace(
                "              properties:",
                "              required: [tenant]\n              properties:",
            ),
    )
    .unwrap();

    fs::write(
        consumer.join("client.ts"),
        r#"import axios from 'axios';
fetch('http://api/orders?limit=2', {
  method: 'POST', headers: { 'x-mode': 'fast' },
  body: JSON.stringify({ tenant: 'example' }),
});
fetch('http://api/orders', {
  method: 'POST', headers: { 'x-mode': 'slow' },
  body: JSON.stringify({ tenant: 42 }),
});
fetch('http://api/search?tags=a');
fetch('http://api/search?tags=a&tags=b');
axios.get('http://api/search', { params: { tags: ['a', 'b'] } });
fetch('http://api/search');
"#,
    )
    .unwrap();
    fs::write(
        consumer.join("worker.py"),
        r#"import requests as http
http.post("http://api/orders",
          json={"tenant": "example"}, params={"limit": 2}, headers={"X-MODE": "fast"})
http.post("http://api/orders",
          json={"tenant": current_tenant}, params={"limit": 7}, headers={"X-mode": "slow"})
http.get("http://api/search?tags=a")
http.get("http://api/search?tags=a&tags=b")
http.get("http://api/search", params={"tags": ["a", "b"]})
http.get("http://api/search")
"#,
    )
    .unwrap();
    fs::write(
        consumer.join("transport.ts"),
        r#"export function submit(body: object) {
  return fetch('http://api/orders?limit=3', {
    method: 'POST', body: JSON.stringify(body), headers: { 'x-mode': 'fast' },
  });
}
"#,
    )
    .unwrap();
    fs::write(
        consumer.join("Orders.vue"),
        r#"<template><div /></template>
<script setup lang="ts">
import { submit } from './transport';
submit({ tenant: 'example' });
useFetch('http://api/orders', {
  method: 'POST', body: { tenant: 'example' }, query: { limit: 3 }, headers: { 'X-MODE': 'fast' },
});
useFetch('http://api/orders', {
  method: 'POST', query: { limit: 2 }, headers: { 'X-mode': 'fast' },
  body: { tenant: false },
});
</script>
"#,
    )
    .unwrap();
    git(&consumer, &["add", "."]);
    git(
        &consumer,
        &["commit", "-m", "test: call service from TS and Python"],
    );
    let config_path = tmp.join("laine.toml");
    fs::write(
        &config_path,
        r#"
version = 1
[[services]]
name = "api"
path = "./api"
openapi = "openapi.yaml"
base_urls = ["http://api"]
[[services]]
name = "consumer"
path = "./consumer"
"#,
    )
    .unwrap();

    let report = run_analysis(&AnalyzeRequest {
        config_path,
        target_repo: "api".into(),
        base_rev: "HEAD".into(),
        head_rev: "WORKTREE".into(),
    })
    .unwrap();
    assert_eq!(report.findings.len(), 4);
    let tenant = report.findings.iter().find(|finding| matches!(
        &finding.change, ContractChange::RequestFieldAddedRequired { field_path, .. } if field_path == "tenant"
    )).unwrap();
    assert_eq!(
        tenant
            .consumers
            .iter()
            .map(|consumer| (
                consumer.evidence.file.as_str(),
                consumer.evidence.line,
                consumer.confidence
            ))
            .collect::<Vec<_>>(),
        [
            ("Orders.vue", 10, Confidence::Confirmed),
            ("client.ts", 8, Confidence::Confirmed),
            ("transport.ts", 3, Confidence::Possible),
            ("worker.py", 5, Confidence::Possible)
        ]
    );
    for consumer in &tenant.consumers {
        let status = if consumer.confidence == Confidence::Confirmed {
            "Incompatible"
        } else {
            "Unverified"
        };
        let label = format!(r#"{status} HTTP body argument "tenant""#);
        assert_eq!(consumer.evidence.snippet, label);
        assert!(consumer.dependency_path[2].starts_with(&label));
    }
    let limit = report.findings.iter().find(|finding| matches!(
        &finding.change, ContractChange::RequiredParameterAdded { name, .. } if name == "limit"
    )).unwrap();
    assert_eq!(limit.consumers.len(), 1);
    assert_eq!(limit.consumers[0].evidence.file, "client.ts");
    assert_eq!(limit.consumers[0].confidence, Confidence::Confirmed);
    assert_eq!(
        limit.consumers[0].evidence.snippet,
        r#"Incompatible HTTP query argument "limit""#
    );
    let mode = report.findings.iter().find(|finding| matches!(
        &finding.change, ContractChange::RequiredParameterAdded { name, .. } if name == "X-Mode"
    )).unwrap();
    assert!(mode.consumers.is_empty());
    let tags = report
        .findings
        .iter()
        .find(|finding| {
            matches!(
                &finding.change, ContractChange::RequiredParameterAdded { name, param_type, .. }
                    if name == "tags" && param_type == "array"
            )
        })
        .unwrap();
    assert_eq!(
        tags.consumers
            .iter()
            .map(|consumer| (
                consumer.evidence.file.as_str(),
                consumer.evidence.line,
                consumer.confidence,
            ))
            .collect::<Vec<_>>(),
        [
            ("client.ts", 10, Confidence::Possible),
            ("client.ts", 11, Confidence::Possible),
            ("client.ts", 12, Confidence::Possible),
            ("client.ts", 13, Confidence::Confirmed),
            ("worker.py", 6, Confidence::Possible),
            ("worker.py", 7, Confidence::Possible),
            ("worker.py", 8, Confidence::Possible),
            ("worker.py", 9, Confidence::Confirmed),
        ]
    );
    fs::remove_dir_all(&tmp).unwrap();
}
