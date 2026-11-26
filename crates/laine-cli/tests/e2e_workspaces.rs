use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use laine_core::config::WorkspaceConfig;

// Reproduce the shared-callee and renamed-service failures through the managed workspace CLI.
#[test]
fn workspace_scan_scopes_clients_and_edit_preserves_renamed_services() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-client-scope-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let projects = tmp.join("projects");
    let home = tmp.join("home");
    let laine = |args: &[&str], cwd: &Path| {
        Command::new(env!("CARGO_BIN_EXE_laine"))
            .current_dir(cwd)
            .env("LAINE_HOME", &home)
            .args(args)
            .output()
            .unwrap()
    };

    let spec = |path: &str| {
        format!(
            "openapi: 3.0.3\ninfo: {{ title: API, version: '1' }}\npaths:\n  {path}:\n    get:\n      responses:\n        '200': {{ description: ok }}\n"
        )
    };
    for (backend, frontend, endpoint) in [
        ("billing-api", "web-a", "/invoices"),
        ("users-svc", "web-b", "/users"),
    ] {
        let producer = projects.join(backend);
        let consumer = projects.join(frontend);
        init_repo(&producer);
        init_repo(&consumer);
        fs::write(producer.join("openapi.yaml"), spec(endpoint)).unwrap();
        fs::write(
            consumer.join("calls.ts"),
            format!("export const call = () => apiClient.get('{endpoint}');\n"),
        )
        .unwrap();
        for repo in [&producer, &consumer] {
            git(repo, &["add", "."]);
            git(repo, &["commit", "-m", "initialize repository"]);
        }
    }

    let output = laine(
        &["init", "--scan", "--all", "--workspace", "demo"],
        &projects,
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("web-a: `apiClient` -> billing-api"),
        "{stdout}"
    );
    assert!(
        stdout.contains("web-b: `apiClient` -> users-svc"),
        "{stdout}"
    );

    let path = home.join("configs/demo.toml");
    let mut config = WorkspaceConfig::load_from_path(&path).unwrap();
    assert_eq!(config.clients.len(), 2);
    let billing = config
        .services
        .iter_mut()
        .find(|service| service.name == "billing-api")
        .unwrap();
    billing.name = "billing".into();
    billing.base_urls = vec!["https://billing.example".into()];
    let mapping = config
        .clients
        .iter_mut()
        .find(|mapping| mapping.target_service == "billing-api")
        .unwrap();
    mapping.target_service = "billing".into();
    mapping.path_prefix = Some("/manual".into());
    config.save_to_path(&path).unwrap();

    let output = laine(&["workspace", "edit", "demo", "--all"], &projects);
    assert!(output.status.success(), "{output:?}");
    let edited = WorkspaceConfig::load_from_path(&path).unwrap();
    assert!(edited.find_service("billing-api").is_none());
    assert_eq!(
        edited.find_service("billing").unwrap().base_urls,
        ["https://billing.example"]
    );
    assert_eq!(edited.clients, config.clients);

    // Remove the endpoint from each producer and verify extraction uses the correct mapping.
    for (backend, frontend, endpoint) in [
        ("billing-api", "web-a", "/manual/invoices"),
        ("users-svc", "web-b", "/users"),
    ] {
        let producer = projects.join(backend);
        fs::write(producer.join("openapi.yaml"), spec(endpoint)).unwrap();
        git(&producer, &["add", "."]);
        git(
            &producer,
            &["commit", "--allow-empty", "-m", "store endpoint"],
        );
        fs::write(
            producer.join("openapi.yaml"),
            "openapi: 3.0.3\ninfo: { title: API, version: '1' }\npaths: {}\n",
        )
        .unwrap();
        let output = laine(&["check"], &producer);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(
            stdout.contains(&format!("{frontend}/calls.ts:1")),
            "{stdout}"
        );
    }

    let _ = fs::remove_dir_all(&tmp);
}

// Infer custom object names from all four framework catalogs and surface a positive tie.
#[test]
fn workspace_scan_uses_framework_routes_and_reports_ambiguous_wrappers() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-client-tie-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let projects = tmp.join("projects");
    for (name, manifest_file, manifest, route_file, routes) in [
        (
            "express-api",
            "package.json",
            r#"{"dependencies":{"express":"5"}}"#,
            "server.ts",
            "import express from 'express'; const app = express(); app.get('/express', handler); app.get('/shared', handler);",
        ),
        (
            "fastapi-api",
            "requirements.txt",
            "fastapi",
            "app.py",
            "from fastapi import FastAPI\napp = FastAPI()\n@app.get('/fastapi')\ndef item():\n    return {}\n",
        ),
        (
            "flask-api",
            "requirements.txt",
            "flask",
            "app.py",
            "from flask import Flask\napp = Flask(__name__)\n@app.get('/flask')\n@app.get('/shared')\ndef item():\n    return {}\n",
        ),
        (
            "laravel-api",
            "artisan",
            "#!/usr/bin/env php",
            "routes/api.php",
            "<?php\nRoute::get('/laravel', 'ItemController@index');",
        ),
    ] {
        let producer = projects.join(name);
        init_repo(&producer);
        fs::write(producer.join(manifest_file), manifest).unwrap();
        let route_path = producer.join(route_file);
        fs::create_dir_all(route_path.parent().unwrap()).unwrap();
        fs::write(route_path, routes).unwrap();
    }
    let portal = projects.join("portal");
    init_repo(&portal);
    fs::write(
        portal.join("calls.ts"),
        "nodeTransport.get('/express');\nfastTransport.get('/fastapi');\nflaskTransport.get('/flask');\nphpTransport.get('/api/laravel');\napiClient.get('/shared');\nhttpClient.get('/unknown');\n",
    )
    .unwrap();

    let config_path = tmp.join("laine.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_laine"))
        .current_dir(&projects)
        .args(["init", "--scan", "--all", "--path"])
        .arg(&config_path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("`apiClient` in 'portal': candidates: express-api, flask-api"),
        "{stderr}"
    );
    let config = WorkspaceConfig::load_from_path(&config_path).unwrap();
    let mappings: Vec<_> = config
        .clients
        .iter()
        .map(|mapping| {
            assert_eq!(mapping.consumer_service.as_deref(), Some("portal"));
            assert_eq!(mapping.path_prefix, None);
            (mapping.callee.as_str(), mapping.target_service.as_str())
        })
        .collect();
    assert_eq!(
        mappings,
        [
            ("fastTransport", "fastapi-api"),
            ("flaskTransport", "flask-api"),
            ("nodeTransport", "express-api"),
            ("phpTransport", "laravel-api"),
        ]
    );

    let _ = fs::remove_dir_all(&tmp);
}

/// Verifica que el selector rechaza la entrada no interactiva aunque la tubería siga abierta.
fn assert_selector_rejects_open_stdin(command: &mut Command) {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("laine should start");
    let _open_stdin = child.stdin.take().expect("laine stdin should be piped");
    let deadline = Instant::now() + Duration::from_secs(5);

    // Si el proceso intenta leer de la tubería, lo detenemos y recogemos su salida.
    while child
        .try_wait()
        .expect("laine status should be readable")
        .is_none()
    {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("laine should stop");
            panic!(
                "laine did not exit with stdin open:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }

    let output = child
        .wait_with_output()
        .expect("laine output should be readable");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(output.stdout.is_empty(), "selector must not print a prompt");
    assert!(stderr.contains("interactive terminal"), "stderr:\n{stderr}");
    assert!(stderr.contains("--select"), "stderr:\n{stderr}");
    assert!(stderr.contains("--all"), "stderr:\n{stderr}");
}

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

// Comprueba los checks desde un productor y las opciones cuando el cwd es un consumidor.
#[test]
fn init_scan_registers_isolated_workspaces_and_laine_check_runs_zero_flag_from_repo_cwd() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-workspaces-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let laine_home = tmp.join("laine-home");
    let projects = tmp.join("Projects");
    fs::create_dir_all(&projects).unwrap();

    // Ecosystem A: laravel-a + portal-a
    let laravel_a = projects.join("laravel-a");
    init_repo(&laravel_a);
    fs::write(laravel_a.join("artisan"), "#!/usr/bin/env php\n").unwrap();
    fs::create_dir_all(laravel_a.join("routes")).unwrap();
    fs::write(
        laravel_a.join("routes/api.php"),
        "<?php\nRoute::post('/orders/create', 'OrderController@create');\n",
    )
    .unwrap();
    git(&laravel_a, &["add", "."]);
    git(&laravel_a, &["commit", "-m", "init laravel-a"]);

    let portal_a = projects.join("portal-a");
    init_repo(&portal_a);
    fs::write(portal_a.join("package.json"), r#"{"name": "portal-a"}"#).unwrap();
    fs::create_dir_all(portal_a.join("src")).unwrap();
    fs::write(
        portal_a.join("src/orders.ts"),
        "export function createOrder() {\n  return fetch('http://laravel-a/api/orders/create', { method: 'POST' });\n}\n",
    )
    .unwrap();
    git(&portal_a, &["add", "."]);
    git(&portal_a, &["commit", "-m", "init portal-a"]);

    // Ecosystem B (unrelated repos living in the same Projects/ folder): laravel-b + portal-b
    let laravel_b = projects.join("laravel-b");
    init_repo(&laravel_b);
    fs::write(laravel_b.join("artisan"), "#!/usr/bin/env php\n").unwrap();
    fs::create_dir_all(laravel_b.join("routes")).unwrap();
    fs::write(
        laravel_b.join("routes/api.php"),
        "<?php\nRoute::get('/status', 'StatusController@index');\n",
    )
    .unwrap();
    git(&laravel_b, &["add", "."]);
    git(&laravel_b, &["commit", "-m", "init laravel-b"]);

    let portal_b = projects.join("portal-b");
    init_repo(&portal_b);
    fs::write(portal_b.join("package.json"), r#"{"name": "portal-b"}"#).unwrap();
    fs::create_dir_all(portal_b.join("src")).unwrap();
    fs::write(
        portal_b.join("src/status.ts"),
        "export function checkStatus() {\n  return fetch('http://laravel-b/api/status');\n}\n",
    )
    .unwrap();
    git(&portal_b, &["add", "."]);
    git(&portal_b, &["commit", "-m", "init portal-b"]);

    let bin = env!("CARGO_BIN_EXE_laine");
    let cfg_a = tmp.join("laine.eco-a.toml");
    let cfg_b = tmp.join("laine.eco-b.toml");

    // 1. Scan Projects/ and create two separate named workspaces selecting only their respective repos
    let scan_a = Command::new(bin)
        .env("LAINE_HOME", &laine_home)
        .args([
            "init",
            "--scan",
            projects.to_str().unwrap(),
            "--select",
            "laravel-a,portal-a",
            "--workspace",
            "eco-a",
            "--path",
            cfg_a.to_str().unwrap(),
        ])
        .output()
        .expect("laine init --scan should run for eco-a");
    assert!(
        scan_a.status.success(),
        "eco-a init --scan failed: {}",
        String::from_utf8_lossy(&scan_a.stderr)
    );

    let scan_b = Command::new(bin)
        .env("LAINE_HOME", &laine_home)
        .args([
            "init",
            "--scan",
            projects.to_str().unwrap(),
            "--select",
            "laravel-b,portal-b",
            "--workspace",
            "eco-b",
            "--path",
            cfg_b.to_str().unwrap(),
        ])
        .output()
        .expect("laine init --scan should run for eco-b");
    assert!(
        scan_b.status.success(),
        "eco-b init --scan failed: {}",
        String::from_utf8_lossy(&scan_b.stderr)
    );

    // 2. Verify `laine workspace list` shows both workspaces
    let list_out = Command::new(bin)
        .env("LAINE_HOME", &laine_home)
        .args(["workspace", "list"])
        .output()
        .expect("laine workspace list should run");
    assert!(list_out.status.success());
    let list_stdout = String::from_utf8_lossy(&list_out.stdout);
    assert!(list_stdout.contains("eco-a"), "got:\n{list_stdout}");
    assert!(list_stdout.contains("eco-b"), "got:\n{list_stdout}");

    // 3. Remove `/orders/create` locally in laravel-a (uncommitted WORKTREE)
    fs::write(laravel_a.join("routes/api.php"), "<?php\n").unwrap();

    // 4. Run `laine check` with ZERO flags from inside laravel-a!
    let check_out = Command::new(bin)
        .current_dir(&laravel_a)
        .env("LAINE_HOME", &laine_home)
        .args(["check"])
        .output()
        .expect("laine check should run");
    let check_stdout = String::from_utf8_lossy(&check_out.stdout);
    let check_stderr = String::from_utf8_lossy(&check_out.stderr);
    assert_eq!(
        check_out.status.code(),
        Some(2),
        "expected exit code 2 from zero-flag `laine check`.\nstdout:\n{check_stdout}\nstderr:\n{check_stderr}"
    );
    assert!(
        check_stdout.contains("Analyzed: 2 repos"),
        "eco-a should only analyze its 2 repos (never eco-b), got:\n{check_stdout}"
    );
    assert!(
        check_stdout.contains("portal-a/src/orders.ts:2"),
        "expected portal-a consumer hit, got:\n{check_stdout}"
    );
    assert!(
        !check_stdout.contains("portal-b") && !check_stdout.contains("laravel-b"),
        "eco-b repos must not leak into eco-a check, got:\n{check_stdout}"
    );

    let consumer_check = Command::new(bin)
        .current_dir(&portal_a)
        .env("LAINE_HOME", &laine_home)
        .arg("check")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&consumer_check.stderr);
    assert_eq!(consumer_check.status.code(), Some(1));
    assert!(stderr.contains("laine check --config"), "{stderr}");
    assert!(stderr.contains("--repo \"laravel-a\""), "{stderr}");
    assert!(!stderr.contains("laravel-b"), "{stderr}");

    let producer_override = Command::new(bin)
        .current_dir(&portal_a)
        .env("LAINE_HOME", &laine_home)
        .args(["check", "--config"])
        .arg(&cfg_a)
        .args(["--repo", "laravel-a"])
        .output()
        .unwrap();
    assert_eq!(producer_override.status.code(), Some(2));

    // 5. `laine check --staged` should pass (exit 0) because the deletion is unstaged
    let staged_out = Command::new(bin)
        .current_dir(&laravel_a)
        .env("LAINE_HOME", &laine_home)
        .args(["check", "--staged"])
        .output()
        .expect("laine check --staged should run");
    assert_eq!(
        staged_out.status.code(),
        Some(0),
        "expected exit code 0 for --staged, got:\n{}",
        String::from_utf8_lossy(&staged_out.stdout)
    );

    let _ = fs::remove_dir_all(&tmp);
}

/// Verifica el error sin terminal y la inicialización explícita con autodetección.
#[test]
fn init_scan_defaults_to_cwd_saves_in_laine_home_configs_and_autodetects_custom_client() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-scan-ux-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let laine_home = tmp.join("laine-home");
    let projects = tmp.join("Projects");
    fs::create_dir_all(&projects).unwrap();

    // Backend Laravel repo
    let backend = projects.join("cerebro-web");
    init_repo(&backend);
    fs::write(backend.join("artisan"), "#!/usr/bin/env php\n").unwrap();
    fs::create_dir_all(backend.join("routes")).unwrap();
    fs::write(
        backend.join("routes/api.php"),
        "<?php\nRoute::post('corp/{accion}/{param?}', [Corporativo::class, 'accion']);\n",
    )
    .unwrap();
    git(&backend, &["add", "."]);
    git(&backend, &["commit", "-m", "init backend"]);

    // Frontend Nuxt repo using custom wrapper `getClient().fetch('/corp/loginCorp')`
    let frontend = projects.join("portal-web");
    init_repo(&frontend);
    fs::write(frontend.join("nuxt.config.ts"), "export default {}\n").unwrap();
    fs::create_dir_all(frontend.join("app/services")).unwrap();
    fs::write(
        frontend.join("app/services/auth.ts"),
        "export function login() {\n  return getClient().fetch('/corp/loginCorp', { method: 'POST' });\n}\n",
    )
    .unwrap();
    git(&frontend, &["add", "."]);
    git(&frontend, &["commit", "-m", "init frontend"]);

    let bin = env!("CARGO_BIN_EXE_laine");

    let expected_cfg = laine_home.join("configs").join("corp.toml");
    assert_selector_rejects_open_stdin(
        Command::new(bin)
            .current_dir(&projects)
            .env("LAINE_HOME", &laine_home)
            .args(["init", "--scan", "--workspace", "corp"]),
    );
    assert!(!expected_cfg.exists(), "init should not write a config");

    // 1. Run `laine init --scan --all --workspace corp` from inside `Projects/`
    //    WITHOUT passing a path after `--scan` and WITHOUT passing `--path`!
    let init_out = Command::new(bin)
        .current_dir(&projects)
        .env("LAINE_HOME", &laine_home)
        .args(["init", "--scan", "--all", "--workspace", "corp"])
        .output()
        .expect("laine init --scan should run");
    assert!(
        init_out.status.success(),
        "laine init --scan failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&init_out.stdout),
        String::from_utf8_lossy(&init_out.stderr)
    );

    // Config must be stored cleanly inside `$LAINE_HOME/configs/corp.toml`, NOT in `Projects/`
    assert!(
        expected_cfg.is_file(),
        "expected config at {}",
        expected_cfg.display()
    );
    assert!(
        !projects.join("laine.corp.toml").exists(),
        "should not clutter cwd with laine.corp.toml"
    );

    // 2. Remove the route locally (uncommitted WORKTREE) and run zero-flag `laine check`
    //    WITHOUT manually editing the generated config!
    fs::write(backend.join("routes/api.php"), "<?php\n").unwrap();

    let check_out = Command::new(bin)
        .current_dir(&backend)
        .env("LAINE_HOME", &laine_home)
        .args(["check"])
        .output()
        .expect("laine check should run");
    let stdout = String::from_utf8_lossy(&check_out.stdout);
    let stderr = String::from_utf8_lossy(&check_out.stderr);
    assert_eq!(
        check_out.status.code(),
        Some(2),
        "expected exit code 2 via auto-detected custom client.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("portal-web/app/services/auth.ts:2"),
        "expected auto-detected custom client consumer hit, got:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&tmp);
}

/// Verifica el error sin terminal y los cambios de repositorio pedidos con flags explícitos.
#[test]
fn workspace_edit_and_repo_add_remove_update_registered_workspace() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-ws-edit-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let laine_home = tmp.join("laine-home");
    let projects = tmp.join("Projects");
    fs::create_dir_all(&projects).unwrap();

    let backend = projects.join("cerebro-web");
    init_repo(&backend);
    fs::write(backend.join("artisan"), "#!/usr/bin/env php\n").unwrap();
    fs::create_dir_all(backend.join("routes")).unwrap();
    fs::write(
        backend.join("routes/api.php"),
        "<?php\nRoute::post('corp/{accion}/{param?}', [Corp::class, 'accion']);\n",
    )
    .unwrap();
    git(&backend, &["add", "."]);
    git(&backend, &["commit", "-m", "init backend"]);

    let v1 = projects.join("portal-v1");
    init_repo(&v1);
    fs::write(v1.join("package.json"), r#"{"name": "v1"}"#).unwrap();
    git(&v1, &["add", "."]);
    git(&v1, &["commit", "-m", "init v1"]);

    let v2 = projects.join("portal-v2");
    init_repo(&v2);
    fs::write(v2.join("nuxt.config.ts"), "export default {}\n").unwrap();
    fs::create_dir_all(v2.join("app/services")).unwrap();
    fs::write(
        v2.join("app/services/auth.ts"),
        "export const login = () => getClient().fetch('/corp/loginCorp', { method: 'POST' });\n",
    )
    .unwrap();
    git(&v2, &["add", "."]);
    git(&v2, &["commit", "-m", "init v2"]);

    let bin = env!("CARGO_BIN_EXE_laine");

    // 1. Initialize workspace with backend + v1
    let init = Command::new(bin)
        .current_dir(&projects)
        .env("LAINE_HOME", &laine_home)
        .args([
            "init",
            "--scan",
            "--select",
            "cerebro-web,portal-v1",
            "--workspace",
            "corp",
        ])
        .output()
        .expect("init should run");
    assert!(init.status.success());

    let config = laine_home.join("configs").join("corp.toml");
    let original = fs::read(&config).unwrap();
    assert_selector_rejects_open_stdin(Command::new(bin).env("LAINE_HOME", &laine_home).args([
        "workspace",
        "edit",
        "corp",
    ]));
    assert_eq!(fs::read(&config).unwrap(), original);

    // 2. Edit workspace to swap portal-v1 -> portal-v2
    let edit = Command::new(bin)
        .env("LAINE_HOME", &laine_home)
        .args([
            "workspace",
            "edit",
            "corp",
            "--select",
            "cerebro-web,portal-v2",
        ])
        .output()
        .expect("workspace edit should run");
    assert!(
        edit.status.success(),
        "workspace edit failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&edit.stdout),
        String::from_utf8_lossy(&edit.stderr)
    );

    // 3. Verify `laine check` now detects portal-v2 via auto-detected client
    fs::write(backend.join("routes/api.php"), "<?php\n").unwrap();
    let check = Command::new(bin)
        .current_dir(&backend)
        .env("LAINE_HOME", &laine_home)
        .args(["check"])
        .output()
        .expect("check should run");
    let check_stdout = String::from_utf8_lossy(&check.stdout);
    assert_eq!(check.status.code(), Some(2), "got:\n{check_stdout}");
    assert!(
        check_stdout.contains("portal-v2/app/services/auth.ts:1"),
        "expected portal-v2 hit after workspace edit, got:\n{check_stdout}"
    );

    // Rechaza una ruta inexistente sin modificar la configuración del workspace.
    let config_path = laine_home.join("configs/corp.toml");
    let before = fs::read(&config_path).unwrap();
    let missing = Command::new(bin)
        .current_dir(&projects)
        .env("LAINE_HOME", &laine_home)
        .args(["repo", "add", "./nonexistent", "--workspace", "corp"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(fs::read(&config_path).unwrap(), before);

    // Guarda la raíz real del checkout aunque la entrada sea una subcarpeta relativa.
    fs::create_dir_all(v1.join("app/services")).unwrap();
    let add_out = Command::new(bin)
        .current_dir(&projects)
        .env("LAINE_HOME", &laine_home)
        .args([
            "repo",
            "add",
            "./portal-v1/app/services",
            "--workspace",
            "corp",
        ])
        .output()
        .expect("repo add --workspace should run");
    assert!(
        add_out.status.success(),
        "repo add --workspace failed: {}",
        String::from_utf8_lossy(&add_out.stderr)
    );
    let cfg = WorkspaceConfig::load_from_path(&config_path).unwrap();
    let saved = &cfg.find_service("portal-v1").unwrap().path;
    assert_eq!(
        fs::canonicalize(saved).unwrap(),
        fs::canonicalize(&v1).unwrap()
    );

    let rm_out = Command::new(bin)
        .env("LAINE_HOME", &laine_home)
        .args(["repo", "remove", "portal-v1", "--workspace", "corp"])
        .output()
        .expect("repo remove --workspace should run");
    assert!(
        rm_out.status.success(),
        "repo remove --workspace failed: {}",
        String::from_utf8_lossy(&rm_out.stderr)
    );

    let _ = fs::remove_dir_all(&tmp);
}

// Comprueba que los procesos concurrentes conservan todas las altas y que reemplazar exige una opción.
#[test]
fn serializes_workspace_updates_and_requires_explicit_replacement() {
    let tmp = std::env::temp_dir().join(format!("laine-e2e-lock-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let state = tmp.join("state");
    let config = tmp.join("laine.toml");
    let replacement = tmp.join("replacement.toml");
    let left = tmp.join("left");
    let right = tmp.join("right");
    init_repo(&left);
    init_repo(&right);
    let cli = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_laine"));
        cmd.env("LAINE_HOME", &state).current_dir(&tmp);
        cmd
    };

    let lock = laine_core::config::lock_config_update(&config).unwrap();
    let mut left_process = cli().args(["repo", "add", "left"]).spawn().unwrap();
    let mut right_process = cli().args(["repo", "add", "right"]).spawn().unwrap();
    drop(lock);
    assert!(left_process.wait().unwrap().success());
    assert!(right_process.wait().unwrap().success());
    let stored = WorkspaceConfig::load_from_path(&config).unwrap();
    assert!(stored.find_service("left").is_some());
    assert!(stored.find_service("right").is_some());

    fs::copy(&config, &replacement).unwrap();
    let mut first = cli()
        .args(["workspace", "add", "left", "laine.toml"])
        .spawn()
        .unwrap();
    let mut second = cli()
        .args(["workspace", "add", "right", "laine.toml"])
        .spawn()
        .unwrap();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());

    let duplicate = cli()
        .args(["workspace", "add", "left", "replacement.toml"])
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("--replace"));
    assert!(
        cli()
            .args(["workspace", "add", "left", "replacement.toml", "--replace"])
            .status()
            .unwrap()
            .success()
    );

    let invalid = cli()
        .args(["init", "unused.toml", "--workspace", "../outside"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(!tmp.join("unused.toml").exists());

    let registry =
        laine_core::workspace::WorkspaceRegistry::load_from_path(&state.join("workspaces.toml"))
            .unwrap();
    assert_eq!(
        registry.workspaces["left"],
        laine_core::workspace::normalize_absolute_path(&replacement)
    );
    assert_eq!(
        registry.workspaces["right"],
        laine_core::workspace::normalize_absolute_path(&config)
    );
    fs::remove_dir_all(&tmp).unwrap();
}
