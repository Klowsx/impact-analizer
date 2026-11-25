use std::fs;
use std::path::Path;
use std::process::Command;

use laine_cli::engine::{AnalyzeRequest, run_analysis_with_options};
use laine_core::model::{Confidence, ContractChange};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-03-05T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-03-05T12:00:00Z")
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
fn detects_return_type_contracts_zod_schemas_and_vue_template_reads() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-ret-zod-vue-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Producer: OpenAPI service removing `segundoApellido`
    let api_dir = tmp.join("users-api");
    init_repo(&api_dir);
    fs::write(
        api_dir.join("openapi.yaml"),
        r#"openapi: 3.0.3
paths:
  /users/{id}:
    get:
      responses:
        '200':
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: { type: integer }
                  nombre: { type: string }
                  segundoApellido: { type: string }
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
  /users/{id}:
    get:
      responses:
        '200':
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: { type: integer }
                  nombre: { type: string }
"#,
    )
    .unwrap();
    git(&api_dir, &["add", "."]);
    git(&api_dir, &["commit", "-m", "v2: drop segundoApellido"]);
    git(&api_dir, &["checkout", "main"]);

    // Consumer 1: Next.js service using `: Promise<UserResponse>` return type (NO generic on fetch!)
    let next_ret = tmp.join("next-return-type");
    init_repo(&next_ret);
    fs::create_dir_all(next_ret.join("src/types")).unwrap();
    fs::create_dir_all(next_ret.join("src/services")).unwrap();
    fs::write(
        next_ret.join("src/types/user.ts"),
        r#"export interface UserResponse {
  id: number;
  nombre: string;
  segundoApellido: string;
}
"#,
    )
    .unwrap();
    fs::write(
        next_ret.join("src/services/user.ts"),
        r#"import type { UserResponse } from "../types/user";

export async function getUser(id: string): Promise<UserResponse> {
  const res = await fetch(`http://users-api/users/${id}`);
  return res.json();
}
"#,
    )
    .unwrap();
    git(&next_ret, &["add", "."]);
    git(
        &next_ret,
        &["commit", "-m", "add next return-type consumer"],
    );

    // Consumer 2: Zod runtime validation (`UserSchema.parse(await res.json())`) in separate schema file
    let next_zod = tmp.join("next-zod");
    init_repo(&next_zod);
    fs::create_dir_all(next_zod.join("src/schemas")).unwrap();
    fs::create_dir_all(next_zod.join("src/services")).unwrap();
    fs::write(
        next_zod.join("src/schemas/user.schema.ts"),
        r#"import { z } from "zod";

export const UserSchema = z.object({
  id: z.number(),
  nombre: z.string(),
  segundoApellido: z.string(),
});

export type User = z.infer<typeof UserSchema>;
"#,
    )
    .unwrap();
    fs::write(
        next_zod.join("src/services/user.ts"),
        r#"import { UserSchema } from "../schemas/user.schema";

export async function loadValidatedUser(id: string) {
  const res = await fetch(`http://users-api/users/${id}`);
  return UserSchema.parse(await res.json());
}
"#,
    )
    .unwrap();
    git(&next_zod, &["add", "."]);
    git(&next_zod, &["commit", "-m", "add zod consumer"]);

    // Consumer 3: Vue SFC where `segundoApellido` is ONLY read inside `<template>`, never in `<script setup>`!
    let vue_tpl = tmp.join("vue-template");
    init_repo(&vue_tpl);
    fs::create_dir_all(vue_tpl.join("app/components")).unwrap();
    fs::write(
        vue_tpl.join("app/components/UserBadge.vue"),
        r#"<template>
  <section>
    <h2>{{ user.nombre }} {{ user.segundoApellido }}</h2>
  </section>
</template>

<script setup lang="ts">
import { ref } from "vue";

const user = ref<any>(null);

async function load(id: string) {
  const res = await fetch(`http://users-api/users/${id}`);
  user.value = await res.json();
}
</script>
"#,
    )
    .unwrap();
    git(&vue_tpl, &["add", "."]);
    git(&vue_tpl, &["commit", "-m", "add vue template consumer"]);

    let cfg_path = tmp.join("laine.toml");
    fs::write(
        &cfg_path,
        r#"version = 1

[[services]]
name = "users-api"
path = "./users-api"
openapi = "openapi.yaml"
base_urls = ["http://users-api"]

[[services]]
name = "next-return-type"
path = "./next-return-type"

[[services]]
name = "next-zod"
path = "./next-zod"

[[services]]
name = "vue-template"
path = "./vue-template"
"#,
    )
    .unwrap();

    let report = run_analysis_with_options(
        &AnalyzeRequest {
            config_path: cfg_path,
            target_repo: "users-api".into(),
            base_rev: "main".into(),
            head_rev: "v2".into(),
        },
        true,
    )
    .expect("return-type + zod + vue-template analysis should succeed");

    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert!(matches!(
        &finding.change,
        ContractChange::ResponseFieldRemoved { field_path, .. } if field_path == "segundoApellido"
    ));

    // 1. Return-type annotation contract (`: Promise<UserResponse>`)
    let ret_hit = finding
        .consumers
        .iter()
        .find(|c| c.repo == "next-return-type")
        .expect("next-return-type should be detected via function return type");
    assert_eq!(ret_hit.confidence, Confidence::Likely);
    assert_eq!(ret_hit.evidence.file, "src/types/user.ts");
    assert_eq!(ret_hit.evidence.line, 4);

    // 2. Zod schema contract (`UserSchema.parse(...)`)
    let zod_hit = finding
        .consumers
        .iter()
        .find(|c| c.repo == "next-zod")
        .expect("next-zod should be detected via Zod schema parse");
    assert_eq!(zod_hit.confidence, Confidence::Likely);
    assert_eq!(zod_hit.evidence.file, "src/schemas/user.schema.ts");
    assert_eq!(zod_hit.evidence.line, 6);

    // 3. Vue `<template>` mustache expression reading `user.segundoApellido` on line 3!
    let vue_hit = finding
        .consumers
        .iter()
        .find(|c| c.repo == "vue-template")
        .expect("vue-template should be detected directly inside <template>");
    assert_eq!(vue_hit.confidence, Confidence::Confirmed);
    assert_eq!(vue_hit.evidence.file, "app/components/UserBadge.vue");
    assert_eq!(vue_hit.evidence.line, 3);

    let _ = fs::remove_dir_all(&tmp);
}
