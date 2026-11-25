use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use laine_core::config::WorkspaceConfig;

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
