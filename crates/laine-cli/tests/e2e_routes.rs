use std::fs;
use std::path::Path;
use std::process::Command;

use laine_cli::engine::{AnalyzeRequest, run_analysis};
use laine_core::model::{Confidence, ContractChange, HttpMethod, Severity};

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
