use std::fs;
use std::path::Path;
use std::process::Command;

use laine_cli::engine::{AnalyzeRequest, run_analysis, run_analysis_with_options};
use laine_cli::report::{render_json, render_markdown, render_terminal};
use laine_core::model::{
    Confidence, ContractChange, ExtractionFailure, ExtractionStage, FileCoverage, HttpMethod,
    Severity,
};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-02-01T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-02-01T12:00:00Z")
        .output()
        .expect("git command should execute");
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

// Verifica que un archivo malformado reste cobertura sin simular una eliminación de rutas.
#[test]
fn reports_partial_extraction_without_asserting_route_removal() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp =
        std::env::temp_dir().join(format!("laine-e2e-partial-{}-{nanos}", std::process::id()));
    fs::create_dir(&tmp).unwrap();
    let api = tmp.join("api");
    let web = tmp.join("web");
    init_repo(&api);
    init_repo(&web);
    fs::write(
        api.join("app.ts"),
        "const app = express(); app.get('/users', handler);\n",
    )
    .unwrap();
    git(&api, &["add", "."]);
    git(&api, &["commit", "-m", "test: add api"]);
    fs::write(api.join("app.ts"), "const app = {\n").unwrap();
    fs::write(web.join("good.ts"), "fetch('http://api/users');\n").unwrap();
    fs::write(web.join("bad.ts"), "const invalid = {\n").unwrap();
    git(&web, &["add", "."]);
    git(&web, &["commit", "-m", "test: add consumer sources"]);

    let config_path = tmp.join("laine.toml");
    fs::write(
        &config_path,
        r#"version = 1
[[services]]
name = "api"
path = "./api"
base_urls = ["http://api"]
[services.routes]
framework = "express"
files = ["./app.ts"]
[[services]]
name = "web"
path = "./web"
"#,
    )
    .unwrap();
    let request = AnalyzeRequest {
        config_path,
        target_repo: "api".into(),
        base_rev: "HEAD".into(),
        head_rev: "WORKTREE".into(),
    };
    let result = run_analysis(&request).unwrap();
    assert!(result.findings.is_empty());
    let coverage = result.coverage.as_ref().unwrap();
    assert!(!coverage.complete);
    assert_eq!(
        coverage.producer_base,
        FileCoverage {
            discovered: 1,
            read: 1,
            analyzed: 1,
            failed: 0
        }
    );
    assert_eq!(
        coverage.producer_head,
        FileCoverage {
            discovered: 1,
            read: 1,
            analyzed: 0,
            failed: 1
        }
    );
    assert_eq!(
        coverage.consumers,
        FileCoverage {
            discovered: 2,
            read: 2,
            analyzed: 1,
            failed: 1
        }
    );
    assert_eq!(result.diagnostics.len(), 2);
    assert_eq!(result.diagnostics[0].file, "app.ts");
    assert_eq!(result.diagnostics[0].stage, ExtractionStage::ProducerHead);
    assert_eq!(result.diagnostics[0].revision, "WORKTREE");
    assert_eq!(result.diagnostics[1].file, "bad.ts");
    assert!(
        result
            .diagnostics
            .iter()
            .all(|item| item.code == ExtractionFailure::FileParseFailed)
    );
    assert!(render_terminal(&result, 0).contains("Coverage: INCOMPLETE"));
    assert!(render_markdown(&result).contains("**Coverage:** INCOMPLETE"));
    for bypass in [false, true] {
        assert_eq!(
            render_json(&result).unwrap(),
            render_json(&run_analysis_with_options(&request, bypass).unwrap()).unwrap()
        );
    }

    // Quitar de verdad el archivo del checkout sí produce un hallazgo del catálogo base.
    fs::remove_file(api.join("app.ts")).unwrap();
    let removed = run_analysis(&request).unwrap();
    assert!(matches!(
        removed.findings[0].change,
        ContractChange::EndpointRemoved { .. }
    ));
    assert_eq!(removed.coverage.unwrap().producer_head.failed, 0);
    fs::remove_dir_all(&tmp).unwrap();
}

// Comprueba que la evidencia y las excepciones conservan la identidad de las rutas de base.
#[test]
fn keeps_literal_calls_and_policy_ignores_separate_from_removed_templates() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!(
        "laine-e2e-route-identity-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir(&tmp).unwrap();

    let api = tmp.join("api");
    init_repo(&api);
    fs::create_dir(api.join("routes")).unwrap();
    let routes = api.join("routes/api.php");
    fs::write(
        &routes,
        "<?php\nRoute::get('/users/me', 'Users@me');\nRoute::get('/users/{id}', 'Users@show');\n",
    )
    .unwrap();
    git(&api, &["add", "."]);
    git(&api, &["commit", "-m", "feat: add user routes"]);

    fs::write(
        &routes,
        "<?php\nRoute::get('/users/me', 'Users@me');\nRoute::get('/users/42', 'Users@example');\n",
    )
    .unwrap();

    let web = tmp.join("web");
    init_repo(&web);
    fs::write(
        web.join("users.ts"),
        "fetch('http://api/users/me');\nfetch('http://api/users/42');\nexport const userById = (id: string) => fetch(`http://api/users/${id}`);\n",
    ).unwrap();
    git(&web, &["add", "."]);
    git(&web, &["commit", "-m", "feat: call user routes"]);

    let config_path = tmp.join("laine.toml");
    fs::write(
        &config_path,
        r#"version = 1

[policy]
ignore_endpoints = ["GET /users/me"]

[[services]]
name = "api"
path = "./api"
base_urls = ["http://api"]
[services.routes]
framework = "laravel"
files = ["routes/api.php"]

[[services]]
name = "web"
path = "./web"
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

    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert!(matches!(
        &finding.change,
        ContractChange::EndpointRemoved { endpoint } if endpoint.normalized_path == "/users/:id"
    ));
    let calls: Vec<_> = finding
        .consumers
        .iter()
        .map(|consumer| (consumer.evidence.line, consumer.confidence))
        .collect();
    assert_eq!(
        calls,
        [(2, Confidence::Confirmed), (3, Confidence::Possible)]
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn detects_removed_laravel_route_consumed_via_custom_ts_wrapper_without_openapi() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-laravel-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // 1. Producer: Laravel repo (NO openapi.yaml)
    let api_dir = tmp.join("laravel-api");
    init_repo(&api_dir);
    fs::create_dir_all(api_dir.join("routes")).unwrap();
    fs::write(
        api_dir.join("routes/api.php"),
        r#"<?php
use Illuminate\Support\Facades\Route;

Route::prefix('corp')->group(function () {
    Route::post('/listBooking', 'BookingController@list');
    Route::post('/listBilling', 'BillingController@list');
});
"#,
    )
    .unwrap();
    git(&api_dir, &["add", "."]);
    git(&api_dir, &["commit", "-m", "feat: initial routes"]);

    git(&api_dir, &["checkout", "-b", "refactor/drop-booking"]);
    fs::write(
        api_dir.join("routes/api.php"),
        r#"<?php
use Illuminate\Support\Facades\Route;

Route::prefix('corp')->group(function () {
    Route::post('/listBilling', 'BillingController@list');
});
"#,
    )
    .unwrap();
    git(&api_dir, &["add", "."]);
    git(&api_dir, &["commit", "-m", "refactor!: remove listBooking"]);
    git(&api_dir, &["checkout", "main"]);

    // 2. Consumer: TypeScript frontend using custom wrapper `getClient().fetch`
    let web_dir = tmp.join("portal-web");
    init_repo(&web_dir);
    fs::create_dir_all(web_dir.join("app/services")).unwrap();
    fs::write(
        web_dir.join("app/services/portal.services.ts"),
        r#"export const portalServices = {
  reserveList(payload: unknown) {
    return getClient().fetch<ReserveResponse>('/corp/listBooking', {
      method: 'POST',
      body: payload,
    });
  },
  billingList(payload: unknown) {
    return getClient().fetch<BillingResponse>('/corp/listBilling', {
      method: 'POST',
      body: payload,
    });
  },
};
"#,
    )
    .unwrap();
    git(&web_dir, &["add", "."]);
    git(&web_dir, &["commit", "-m", "feat: portal services"]);

    // 3. laine.toml with Laravel routes + custom HTTP client wrapper
    let cfg_path = tmp.join("laine.toml");
    fs::write(
        &cfg_path,
        r#"version = 1

[[services]]
name = "laravel-api"
path = "./laravel-api"
[services.routes]
framework = "laravel"
files = ["routes/api.php"]

[[services]]
name = "portal-web"
path = "./portal-web"

[[clients]]
callee = "getClient().fetch"
target_service = "laravel-api"
"#,
    )
    .unwrap();

    let report = run_analysis(&AnalyzeRequest {
        config_path: cfg_path,
        target_repo: "laravel-api".into(),
        base_rev: "main".into(),
        head_rev: "refactor/drop-booking".into(),
    })
    .expect("Laravel + custom TS wrapper analysis should succeed");

    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(finding.confidence, Confidence::Confirmed);
    assert_eq!(finding.severity, Severity::Critical);
    assert!(matches!(
        &finding.change,
        ContractChange::EndpointRemoved { endpoint }
            if endpoint.method == HttpMethod::Post
                && endpoint.normalized_path == "/corp/listBooking"
    ));

    assert_eq!(finding.consumers.len(), 1);
    assert_eq!(finding.consumers[0].repo, "portal-web");
    assert_eq!(
        finding.consumers[0].evidence.file,
        "app/services/portal.services.ts"
    );
    assert_eq!(finding.consumers[0].evidence.line, 3);

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn matches_laravel_optional_route_params_and_reports_exact_route_line() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-laravel-opt-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Producer: Laravel wildcard route with optional `{param?}` defined on line 6
    let api_dir = tmp.join("laravel-api");
    init_repo(&api_dir);
    fs::create_dir_all(api_dir.join("routes")).unwrap();
    fs::write(
        api_dir.join("routes/api.php"),
        "<?php\n\
use Illuminate\\Support\\Facades\\Route;\n\
\n\
Route::get('/health', 'HealthController@check');\n\
// Corporate catch-all route:\n\
Route::post('corp/{accion}/{param?}', [Corporativo::class, 'accion']);\n",
    )
    .unwrap();
    git(&api_dir, &["add", "."]);
    git(
        &api_dir,
        &["commit", "-m", "feat: add corp optional param route"],
    );

    // Consumer calls both 2-segment (`/corp/loginCorp`) and 3-segment (`/corp/booking/123`) URLs
    let web_dir = tmp.join("portal-web");
    init_repo(&web_dir);
    fs::create_dir_all(web_dir.join("app/services")).unwrap();
    fs::write(
        web_dir.join("app/services/auth.ts"),
        r#"export function login() {
  return getClient().fetch('/corp/loginCorp', { method: 'POST' });
}
export function updateBooking(id: string) {
  return getClient().fetch(`/corp/booking/${id}`, { method: 'POST' });
}
"#,
    )
    .unwrap();
    git(&web_dir, &["add", "."]);
    git(&web_dir, &["commit", "-m", "feat: call corp endpoints"]);

    let cfg_path = tmp.join("laine.toml");
    fs::write(
        &cfg_path,
        r#"version = 1

[[services]]
name = "laravel-api"
path = "./laravel-api"
[services.routes]
framework = "laravel"
files = ["routes/api.php"]
prefix = "/api"

[[services]]
name = "portal-web"
path = "./portal-web"

[[clients]]
callee = "getClient().fetch"
target_service = "laravel-api"
path_prefix = "/api"
"#,
    )
    .unwrap();

    // Remove the optional-param route locally in WORKTREE (no commit)
    fs::write(
        api_dir.join("routes/api.php"),
        "<?php\n\
use Illuminate\\Support\\Facades\\Route;\n\
\n\
Route::get('/health', 'HealthController@check');\n",
    )
    .unwrap();

    let report = run_analysis(&AnalyzeRequest {
        config_path: cfg_path,
        target_repo: "laravel-api".into(),
        base_rev: "HEAD".into(),
        head_rev: "WORKTREE".into(),
    })
    .expect("analysis should succeed");

    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(finding.severity, Severity::Critical);
    assert_eq!(finding.producer_evidence.file, "routes/api.php");
    assert_eq!(
        finding.producer_evidence.line,
        Some(6),
        "expected exact route definition line 6"
    );
    assert_eq!(
        finding.consumers.len(),
        2,
        "expected both 2-segment and 3-segment calls to match optional param route, got: {:?}",
        finding.consumers
    );

    let _ = fs::remove_dir_all(&tmp);
}
