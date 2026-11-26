use std::fs;
use std::path::Path;
use std::process::Command;

use laine_cli::engine::{AnalyzeRequest, run_analysis_with_options};
use laine_core::model::{Confidence, ContractChange};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-03-06T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-03-06T12:00:00Z")
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

/// Verifies that imported Axios calls contribute consumption and unresolved-call evidence.
#[test]
fn tracks_exported_axios_instances_even_when_responses_are_discarded() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-axios-imports-{}", std::process::id()));
    let api = tmp.join("producer");
    let frontend = tmp.join("frontend");
    init_repo(&api);
    init_repo(&frontend);

    let original = "openapi: 3.0.3\npaths:\n  /alias:\n    get:\n      responses: {}\n";
    fs::write(api.join("openapi.yaml"), original).unwrap();
    git(&api, &["add", "."]);
    git(&api, &["commit", "-m", "v1"]);
    fs::write(api.join("openapi.yaml"), "openapi: 3.0.3\npaths: {}\n").unwrap();
    git(&api, &["commit", "-am", "remove the endpoint"]);

    for (file, source) in [
        (
            "client.ts",
            "import axios from 'axios';\nconst base = 'http://producer';\nexport const api = axios.create({ baseURL: base });\n",
        ),
        (
            "default-client.ts",
            "import axios from 'axios';\nconst client = axios.create({ baseURL: 'http://producer' });\nexport default client;\n",
        ),
        ("index.ts", "export { api as forwarded } from './client';\n"),
        (
            "named.ts",
            "import { api as named } from './client';\nnamed.get('/alias');\n",
        ),
        (
            "default.ts",
            "import api from './default-client';\nawait api.get('/alias');\n",
        ),
        (
            "barrel.ts",
            "import { forwarded as api } from './index';\napi.get('/alias');\n",
        ),
        (
            "dynamic-client.ts",
            "import axios from 'axios';\nexport default axios.create({ baseURL: process.env.API });\n",
        ),
        (
            "dynamic.ts",
            "import api from './dynamic-client';\napi.get('/alias');\n",
        ),
        (
            "unrelated.ts",
            "import { api } from './client';\nfunction read(api: Map<string, string>) { api.get('/alias'); }\n",
        ),
    ] {
        fs::write(frontend.join(file), source).unwrap();
    }
    git(&frontend, &["add", "."]);
    git(&frontend, &["commit", "-m", "add Axios consumers"]);

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

    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert!(matches!(
        finding.change,
        ContractChange::EndpointRemoved { .. }
    ));
    let mut files: Vec<_> = finding
        .consumers
        .iter()
        .map(|hit| {
            assert_eq!(hit.confidence, Confidence::Confirmed);
            assert_eq!(hit.evidence.line, 2);
            hit.evidence.file.as_str()
        })
        .collect();
    files.sort_unstable();
    assert_eq!(files, ["barrel.ts", "default.ts", "named.ts"]);
    assert_eq!(report.unresolved_calls.len(), 1);
    let unresolved = &report.unresolved_calls[0];
    assert_eq!(unresolved.evidence.file, "dynamic.ts");
    assert_eq!(unresolved.evidence.line, 2);
    assert!(unresolved.reason.contains("base URL"));

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn tracks_untyped_js_multi_file_chain_jsx_props_and_vue_nuxt_aliases_end_to_end() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-crossfile-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Producer: OpenAPI backend removing `segundoApellido` from GET /users/{id}
    let api_dir = tmp.join("spring-users-api");
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

    // Consumer 1: PURE UNTYPED JAVASCRIPT (zero types!) 3-file chain with `@/` alias + React Query
    let js_front = tmp.join("next-untyped-js");
    init_repo(&js_front);
    fs::write(
        js_front.join("jsconfig.json"),
        r#"{
  // Next.js path aliases
  "compilerOptions": {
    "baseUrl": ".",
    "paths": {
      "@/*": ["src/*"],
    },
  }
}
"#,
    )
    .unwrap();
    fs::create_dir_all(js_front.join("src/services")).unwrap();
    fs::create_dir_all(js_front.join("src/hooks")).unwrap();
    fs::create_dir_all(js_front.join("src/components")).unwrap();
    fs::write(
        js_front.join("src/services/userService.js"),
        r#"export async function getUser(id) {
  const res = await fetch(`http://spring-users-api/users/${id}`);
  return res.json();
}
"#,
    )
    .unwrap();
    fs::write(
        js_front.join("src/hooks/useUser.js"),
        r#"import { useQuery } from "@tanstack/react-query";
import { getUser } from "@/services/userService";

export function useUser(id) {
  return useQuery({
    queryKey: ["user", id],
    queryFn: () => getUser(id),
  });
}
"#,
    )
    .unwrap();
    fs::write(
        js_front.join("src/components/UserCard.jsx"),
        r#"import { useUser } from "@/hooks/useUser";

export function UserCard({ id }) {
  const { data: user } = useUser(id);
  return <p>{user?.segundoApellido}</p>;
}
"#,
    )
    .unwrap();
    git(&js_front, &["add", "."]);
    git(&js_front, &["commit", "-m", "add untyped JS 3-file chain"]);

    // Consumer 2: 3-file JSX Prop Propagation (`service -> Page -> <UserBadge user={user} />`)
    let react_props = tmp.join("react-jsx-props");
    init_repo(&react_props);
    fs::write(
        react_props.join("tsconfig.json"),
        r#"{
  "compilerOptions": {
    "baseUrl": ".",
    "paths": { "@/*": ["src/*"] }
  }
}
"#,
    )
    .unwrap();
    fs::create_dir_all(react_props.join("src/services")).unwrap();
    fs::create_dir_all(react_props.join("src/pages")).unwrap();
    fs::create_dir_all(react_props.join("src/components")).unwrap();
    fs::write(
        react_props.join("src/services/api.ts"),
        r#"export async function loadProfile(id: string) {
  const res = await fetch(`http://spring-users-api/users/${id}`);
  return res.json();
}
"#,
    )
    .unwrap();
    fs::write(
        react_props.join("src/components/UserBadge.tsx"),
        r#"export function UserBadge({ user }: { user: any }) {
  return <span>{user.segundoApellido}</span>;
}
"#,
    )
    .unwrap();
    fs::write(
        react_props.join("src/pages/Profile.tsx"),
        r#"import { loadProfile } from "@/services/api";
import { UserBadge } from "@/components/UserBadge";

export async function ProfilePage({ id }: { id: string }) {
  const user = await loadProfile(id);
  return <UserBadge user={user} />;
}
"#,
    )
    .unwrap();
    git(&react_props, &["add", "."]);
    git(&react_props, &["commit", "-m", "add JSX prop propagation"]);

    // Consumer 3: Nuxt/Vue SFC `<template>` consuming an imported service via `~/` alias
    let nuxt_app = tmp.join("nuxt-cross-file");
    init_repo(&nuxt_app);
    fs::create_dir_all(nuxt_app.join("app/services")).unwrap();
    fs::create_dir_all(nuxt_app.join("app/components")).unwrap();
    fs::write(
        nuxt_app.join("app/services/users.ts"),
        r#"export const userService = {
  async findById(id: string) {
    const res = await fetch(`http://spring-users-api/users/${id}`);
    return res.json();
  },
};
"#,
    )
    .unwrap();
    fs::write(
        nuxt_app.join("app/components/UserHeader.vue"),
        r#"<template>
  <h1>{{ user.segundoApellido }}</h1>
</template>

<script setup lang="ts">
import { ref } from "vue";
import { userService } from "~/services/users";

const user = ref<any>(null);
async function init(id: string) {
  user.value = await userService.findById(id);
}
</script>
"#,
    )
    .unwrap();
    git(&nuxt_app, &["add", "."]);
    git(
        &nuxt_app,
        &["commit", "-m", "add nuxt cross-file template consumer"],
    );

    let cfg_path = tmp.join("laine.toml");
    fs::write(
        &cfg_path,
        r#"version = 1

[[services]]
name = "spring-users-api"
path = "./spring-users-api"
openapi = "openapi.yaml"
base_urls = ["http://spring-users-api"]

[[services]]
name = "next-untyped-js"
path = "./next-untyped-js"

[[services]]
name = "react-jsx-props"
path = "./react-jsx-props"

[[services]]
name = "nuxt-cross-file"
path = "./nuxt-cross-file"
"#,
    )
    .unwrap();

    let report = run_analysis_with_options(
        &AnalyzeRequest {
            config_path: cfg_path,
            target_repo: "spring-users-api".into(),
            base_rev: "main".into(),
            head_rev: "v2".into(),
        },
        true,
    )
    .expect("cross-file interprocedural analysis should succeed");

    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert!(matches!(
        &finding.change,
        ContractChange::ResponseFieldRemoved { field_path, .. } if field_path == "segundoApellido"
    ));

    // 1. Pure untyped JS 3-file chain caught via React Query (`LIKELY`)
    let js_hit = finding
        .consumers
        .iter()
        .find(|c| c.repo == "next-untyped-js")
        .expect("next-untyped-js should be caught even with ZERO types");
    assert_eq!(js_hit.confidence, Confidence::Likely);
    assert_eq!(js_hit.evidence.file, "src/components/UserCard.jsx");
    assert_eq!(js_hit.evidence.line, 5);
    assert!(
        js_hit
            .dependency_path
            .iter()
            .any(|s| s.contains("userService.js"))
    );

    // 2. JSX prop propagation caught in `UserBadge.tsx:2` (`CONFIRMED`)
    let props_hit = finding
        .consumers
        .iter()
        .find(|c| c.repo == "react-jsx-props")
        .expect("react-jsx-props should be caught via <UserBadge user={user} />");
    assert_eq!(props_hit.confidence, Confidence::Confirmed);
    assert_eq!(props_hit.evidence.file, "src/components/UserBadge.tsx");
    assert_eq!(props_hit.evidence.line, 2);

    // 3. Nuxt/Vue `<template>` consuming imported service via `~/` alias (`CONFIRMED`)
    let nuxt_hit = finding
        .consumers
        .iter()
        .find(|c| c.repo == "nuxt-cross-file")
        .expect("nuxt-cross-file should be caught inside <template>");
    assert_eq!(nuxt_hit.confidence, Confidence::Confirmed);
    assert_eq!(nuxt_hit.evidence.file, "app/components/UserHeader.vue");
    assert_eq!(nuxt_hit.evidence.line, 2);

    let _ = fs::remove_dir_all(&tmp);
}
