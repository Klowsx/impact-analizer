use std::fs;
use std::path::Path;
use std::process::Command;

use laine_cli::engine::{AnalyzeRequest, run_analysis, run_analysis_with_options};
use laine_core::model::{Confidence, ContractChange};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-03-02T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-03-02T12:00:00Z")
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

/// Exercises the generic and runtime schema evidence paths with colliding declarations.
#[test]
fn resolves_response_contracts_from_the_consumers_module() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-scoped-types-{}", std::process::id()));
    let api = tmp.join("producer");
    let frontend = tmp.join("frontend");
    init_repo(&api);
    init_repo(&frontend);

    let spec = r#"openapi: 3.0.3
paths:
  /alias:
    get:
      responses:
        '200':
          content:
            application/json:
              schema:
                type: object
"#;
    let inferred_route = spec
        .split_once("paths:\n")
        .unwrap()
        .1
        .replace("/alias:", "/inferred:");
    fs::write(
        api.join("openapi.yaml"),
        format!(
            "{spec}                properties:\n{}{inferred_route}                properties:\n                  correctSchema: {{ type: string }}\n",
            [
                "wrongDto",
                "correctDto",
                "parentField",
                "wrongSchema",
                "correctSchema",
                "privateField",
            ]
            .iter()
            .map(|field| format!("                  {field}: {{ type: string }}\n"))
            .collect::<String>()
        ),
    )
    .unwrap();
    git(&api, &["add", "."]);
    git(&api, &["commit", "-m", "v1"]);
    fs::write(api.join("openapi.yaml"), format!("{spec}{inferred_route}")).unwrap();
    git(&api, &["commit", "-am", "remove all fields"]);

    for (file, source) in [
        (
            "client.ts",
            "import axios from 'axios';\nexport const api = axios.create({ baseURL: 'http://producer' });\n",
        ),
        ("a.ts", "export interface DTO { wrongDto: string }\n"),
        (
            "parent.ts",
            "export default interface Parent { parentField: string }\n",
        ),
        (
            "b.ts",
            "import type Model from './parent';\nexport interface DTO extends Model { correctDto: string }\ninterface Private { privateField: string }\n",
        ),
        ("types.ts", "export type { DTO as Exported } from './b';\n"),
        (
            "dto.ts",
            "import type * as Contracts from './types';\nimport { api } from './client';\ntype Result = Contracts.Exported;\napi.get<Result>('/alias');\n",
        ),
        (
            "missing.ts",
            "import type { DTO as Missing } from './missing-module';\nimport type { Private } from './b';\nimport { api } from './client';\napi.get<Missing | Private | DTO>('/alias');\n",
        ),
        (
            "schema-a.ts",
            "import { z } from 'zod';\nexport const Schema = z.object({ correctSchema: z.string() });\nexport type Schema = z.infer<typeof Schema>;\n",
        ),
        (
            "schema-z.ts",
            "import { z } from 'zod';\nexport const Schema = z.object({ wrongSchema: z.string() });\n",
        ),
        (
            "schema-exports.ts",
            "export { Schema as default } from './schema-a';\n",
        ),
        (
            "validate.ts",
            "import Validator from './schema-exports';\nimport { api } from './client';\nconst result = await api.get('/alias');\nValidator.parse(result.data);\n",
        ),
        (
            "inferred.ts",
            "import type { Schema as Result } from './schema-a';\nimport { api } from './client';\napi.get<Result>('/inferred');\n",
        ),
    ] {
        fs::write(frontend.join(file), source).unwrap();
    }
    git(&frontend, &["add", "."]);
    git(
        &frontend,
        &["commit", "-m", "add scoped response contracts"],
    );

    let config_path = tmp.join("laine.toml");
    fs::write(
        &config_path,
        r#"version = 1
[[services]]
name = "producer"
path = "./producer"
openapi = "openapi.yaml"
base_urls = ["http://producer"]
[[services]]
name = "frontend"
path = "./frontend"
"#,
    )
    .unwrap();
    let report = run_analysis_with_options(
        &AnalyzeRequest {
            config_path,
            target_repo: "producer".into(),
            base_rev: "HEAD~1".into(),
            head_rev: "HEAD".into(),
        },
        true,
    )
    .unwrap();

    assert_eq!(report.findings.len(), 7);
    for finding in &report.findings {
        let ContractChange::ResponseFieldRemoved {
            field_path,
            endpoint,
            ..
        } = &finding.change
        else {
            panic!("unexpected change: {:?}", finding.change);
        };
        let (definition, caller) = match field_path.as_str() {
            "correctDto" => ("b.ts", "dto.ts:4"),
            "parentField" => ("parent.ts", "dto.ts:4"),
            "correctSchema" if endpoint.normalized_path == "/inferred" => {
                ("schema-a.ts", "inferred.ts:3")
            }
            "correctSchema" => ("schema-a.ts", "validate.ts:3"),
            "wrongDto" | "wrongSchema" | "privateField" => {
                assert!(
                    finding.consumers.is_empty(),
                    "unexpected evidence for {field_path}"
                );
                continue;
            }
            _ => panic!("unexpected field: {field_path}"),
        };
        assert_eq!(finding.consumers.len(), 1, "{field_path}");
        let consumer = &finding.consumers[0];
        assert_eq!(consumer.confidence, Confidence::Likely);
        assert_eq!(consumer.evidence.file, definition);
        assert!(
            consumer
                .dependency_path
                .iter()
                .any(|step| step.ends_with(caller))
        );
    }
    assert!(report.unresolved_calls.is_empty());

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn detects_vue_sfc_confirmed_read_and_cross_file_ts_generic_type_likely_contract() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-vue-types-{}", std::process::id()));
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
"#,
    )
    .unwrap();
    git(&api_dir, &["add", "."]);
    git(&api_dir, &["commit", "-m", "v2"]);
    git(&api_dir, &["checkout", "main"]);

    // Consumer 1: Vue SFC component reading `customer.fullName` directly -> CONFIRMED with exact .vue line
    let vue_dir = tmp.join("vue-app");
    init_repo(&vue_dir);
    fs::create_dir_all(vue_dir.join("app/components")).unwrap();
    fs::write(
        vue_dir.join("app/components/CustomerCard.vue"),
        r#"<template>
  <div>{{ label }}</div>
</template>

<script setup lang="ts">
async function load(id: string) {
  const res = await fetch(`http://customer-api/customers/${id}`);
  const customer = await res.json();
  return customer.fullName;
}
</script>
"#,
    )
    .unwrap();
    git(&vue_dir, &["add", "."]);
    git(&vue_dir, &["commit", "-m", "add vue component"]);

    // Consumer 2: Nuxt-style service calling `getClient().fetch<CustomerDTO>('/customers/1')`
    // while `CustomerDTO` is defined in a separate `app/types/customer.ts` file -> LIKELY
    let nuxt_dir = tmp.join("nuxt-portal");
    init_repo(&nuxt_dir);
    fs::create_dir_all(nuxt_dir.join("app/services")).unwrap();
    fs::create_dir_all(nuxt_dir.join("app/types")).unwrap();
    fs::write(
        nuxt_dir.join("app/types/customer.ts"),
        r#"export interface CustomerDTO {
  id: number;
  fullName: string;
}
"#,
    )
    .unwrap();
    fs::write(
        nuxt_dir.join("app/services/customer.service.ts"),
        r#"import type { CustomerDTO } from "~/types/customer";

export const customerService = {
  getById(id: string) {
    return getClient().fetch<CustomerDTO>(`/customers/${id}`);
  },
};
"#,
    )
    .unwrap();
    git(&nuxt_dir, &["add", "."]);
    git(&nuxt_dir, &["commit", "-m", "add nuxt typed service"]);

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
name = "vue-app"
path = "./vue-app"

[[services]]
name = "nuxt-portal"
path = "./nuxt-portal"

[[clients]]
callee = "getClient().fetch"
target_service = "customer-api"
"#,
    )
    .unwrap();

    let report = run_analysis(&AnalyzeRequest {
        config_path: cfg_path,
        target_repo: "customer-api".into(),
        base_rev: "main".into(),
        head_rev: "v2".into(),
    })
    .expect("vue + cross-file type contract analysis should succeed");

    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert!(matches!(
        &finding.change,
        ContractChange::ResponseFieldRemoved { field_path, .. } if field_path == "fullName"
    ));

    let vue_hit = finding
        .consumers
        .iter()
        .find(|c| c.repo == "vue-app")
        .expect("vue-app should be reported");
    assert_eq!(vue_hit.confidence, Confidence::Confirmed);
    assert_eq!(vue_hit.evidence.file, "app/components/CustomerCard.vue");
    assert_eq!(vue_hit.evidence.line, 9);

    let nuxt_hit = finding
        .consumers
        .iter()
        .find(|c| c.repo == "nuxt-portal")
        .expect("nuxt-portal should be reported via cross-file interface");
    assert_eq!(nuxt_hit.confidence, Confidence::Likely);
    assert_eq!(nuxt_hit.evidence.file, "app/types/customer.ts");
    assert_eq!(nuxt_hit.evidence.line, 3);
    assert!(
        nuxt_hit
            .dependency_path
            .iter()
            .any(|p| p.contains("app/services/customer.service.ts:5"))
    );

    let _ = fs::remove_dir_all(&tmp);
}
