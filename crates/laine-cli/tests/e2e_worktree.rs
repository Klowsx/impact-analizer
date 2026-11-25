use std::fs;
use std::path::Path;
use std::process::Command;

use laine_core::config::{
    CustomClientConfig, RouteFramework, RouteSourceConfig, ServiceConfig, WorkspaceConfig,
};

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

// Verifica la revisión explícita de consumidores, incluida la app del propio productor.
#[test]
fn selects_consumer_snapshots_and_invalidates_mutable_cache_by_content() {
    let tmp = std::env::temp_dir().join(format!(
        "laine-e2e-consumer-snapshots-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir(&tmp).unwrap();
    let api = tmp.join("api");
    let web = tmp.join("web");
    init_repo(&api);
    init_repo(&web);
    fs::create_dir(api.join("routes")).unwrap();
    fs::write(
        api.join("routes/api.php"),
        "<?php\nRoute::get('/users', 'Users@index');\n",
    )
    .unwrap();
    fs::write(api.join("local.ts"), "export {};\n").unwrap();
    fs::write(web.join("staged.ts"), "export {};\n").unwrap();
    for repo in [&api, &web] {
        git(repo, &["add", "."]);
        git(repo, &["commit", "-m", "test: establish base"]);
    }
    let call = "fetch('http://api/users');\n";
    for (repo, file) in [(&api, "local.ts"), (&web, "staged.ts")] {
        fs::write(repo.join(file), call).unwrap();
        git(repo, &["add", file]);
        fs::write(repo.join(file), "export {};\n").unwrap();
    }
    fs::write(api.join("routes/api.php"), "<?php\n").unwrap();
    fs::write(web.join("worktree.ts"), call).unwrap();

    let config = tmp.join("laine.toml");
    fs::write(
        &config,
        r#"version = 1
[[services]]
name = "api"
path = "./api"
base_urls = ["http://api"]
[services.routes]
framework = "laravel"
[[services]]
name = "web"
path = "./web"
"#,
    )
    .unwrap();

    let analyze = |snapshot: &str, disable_cache: bool| {
        let mut process = Command::new(env!("CARGO_BIN_EXE_laine"));
        process.args(["analyze", "--config"]).arg(&config).args([
            "--repo",
            "api",
            "--head",
            "WORKTREE",
            "--format=json",
            "--consumer-revision",
            snapshot,
        ]);
        if disable_cache {
            process.arg("--no-cache");
        }
        let output = process.output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let head = analyze("head", false);
    assert_eq!(
        head["consumer_revisions"]["web"].as_str().unwrap().len(),
        40
    );
    assert_eq!(
        head["findings"][0]["consumers"].as_array().unwrap().len(),
        0
    );

    let staged = analyze("staged", false);
    assert_eq!(staged["consumer_revisions"]["api"], "STAGED");
    assert_eq!(staged["consumer_revisions"]["web"], "STAGED");
    let consumers = staged["findings"][0]["consumers"].as_array().unwrap();
    assert_eq!(consumers.len(), 2);
    assert_eq!(consumers[0]["repo"], "api");
    assert_eq!(consumers[0]["evidence"]["file"], "local.ts");
    assert_eq!(consumers[1]["evidence"]["file"], "staged.ts");
    assert_eq!(staged, analyze("staged", true));

    let worktree = analyze("worktree", false);
    let consumers = worktree["findings"][0]["consumers"].as_array().unwrap();
    assert_eq!(consumers.len(), 1);
    assert_eq!(consumers[0]["evidence"]["file"], "worktree.ts");
    assert_eq!(consumers[0]["evidence"]["revision"], "WORKTREE");
    assert_eq!(worktree, analyze("worktree", true));

    fs::write(web.join("worktree.ts"), "export {};\n").unwrap();
    let changed_worktree = analyze("worktree", false);
    assert!(
        changed_worktree["findings"][0]["consumers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(changed_worktree, analyze("worktree", true));
    assert_eq!(staged, analyze("staged", false));
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn cli_analyze_defaults_to_head_vs_worktree_for_uncommitted_changes() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-worktree-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Producer: Laravel API
    let backend = tmp.join("laravel-api");
    init_repo(&backend);
    fs::create_dir_all(backend.join("routes")).unwrap();
    fs::write(
        backend.join("routes/api.php"),
        r#"<?php
use Illuminate\Support\Facades\Route;

Route::post('/reset_password', 'AuthController@changePassword');
Route::get('/health', 'HealthController@check');
"#,
    )
    .unwrap();
    git(&backend, &["add", "."]);
    git(&backend, &["commit", "-m", "initial routes"]);

    // Consumer: TS frontend calling `/reset_password` via custom client
    let frontend = tmp.join("web-portal");
    init_repo(&frontend);
    fs::create_dir_all(frontend.join("src")).unwrap();
    fs::write(
        frontend.join("src/auth.ts"),
        r#"export async function resetPassword(email: string) {
  return getClient().fetch('/reset_password', { method: 'POST', body: { email } });
}
"#,
    )
    .unwrap();
    git(&frontend, &["add", "."]);
    git(&frontend, &["commit", "-m", "init frontend"]);

    let cfg_path = tmp.join("laine.toml");
    let cfg = WorkspaceConfig {
        version: 1,
        services: vec![
            ServiceConfig {
                name: "laravel-api".into(),
                path: "./laravel-api".into(),
                openapi: None,
                routes: Some(RouteSourceConfig {
                    framework: RouteFramework::Laravel,
                    files: vec!["routes/api.php".into()],
                    prefix: Some("/api".into()),
                }),
                base_urls: vec!["https://api.local".into()],
            },
            ServiceConfig {
                name: "web-portal".into(),
                path: "./web-portal".into(),
                openapi: None,
                routes: None,
                base_urls: Vec::new(),
            },
        ],
        clients: vec![CustomClientConfig {
            callee: "getClient().fetch".into(),
            target_service: "laravel-api".into(),
            path_prefix: Some("/api".into()),
        }],
        env: Default::default(),
        policy: Default::default(),
        cache_path: None,
    };
    cfg.save_to_path(&cfg_path).unwrap();

    // Modify routes/api.php locally WITHOUT committing (remove /reset_password)
    fs::write(
        backend.join("routes/api.php"),
        r#"<?php
use Illuminate\Support\Facades\Route;

Route::get('/health', 'HealthController@check');
"#,
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_laine");

    // 1. Omitting --base and --head should default to HEAD -> WORKTREE and fail with exit code 2
    let output = Command::new(bin)
        .args([
            "analyze",
            "--config",
            cfg_path.to_str().unwrap(),
            "--repo",
            "laravel-api",
        ])
        .output()
        .expect("laine binary should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit code 2 on uncommitted breaking change.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("-> WORKTREE)"),
        "expected WORKTREE in header, got:\n{stdout}"
    );
    assert!(
        stdout.contains("/api/reset_password"),
        "expected removed endpoint in report, got:\n{stdout}"
    );
    assert!(
        stdout.contains("web-portal/src/auth.ts"),
        "expected affected consumer file in report, got:\n{stdout}"
    );

    // 2. Comparing HEAD -> STAGED should report COMPATIBLE (exit 0) because the deletion is unstaged
    let staged_out = Command::new(bin)
        .args([
            "analyze",
            "--config",
            cfg_path.to_str().unwrap(),
            "--repo",
            "laravel-api",
            "--base",
            "HEAD",
            "--head",
            "STAGED",
        ])
        .output()
        .expect("laine binary should run for STAGED");
    let staged_stdout = String::from_utf8_lossy(&staged_out.stdout);
    assert_eq!(
        staged_out.status.code(),
        Some(0),
        "expected exit code 0 when breaking change is only unstaged.\nstdout:\n{staged_stdout}"
    );
    assert!(
        staged_stdout.contains("-> STAGED)"),
        "expected STAGED in header, got:\n{staged_stdout}"
    );

    let _ = fs::remove_dir_all(&tmp);
}
