use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{
    PolicyConfig, RouteFramework, RouteSourceConfig, ServiceConfig, WorkspaceConfig,
};
use crate::error::{LaineError, Result};
use crate::model::Severity;

/// Registry mapping named workspaces (e.g. `corporativo`, `proyecto-b`) to their
/// `laine.toml` file paths so developers can maintain multiple isolated ecosystems
/// even when repositories live side-by-side in the same parent folder.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceRegistry {
    pub version: u32,
    #[serde(default)]
    pub workspaces: BTreeMap<String, PathBuf>,
}

/// Summary of a local Git repository discovered by `scan_directory`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRepo {
    pub name: String,
    pub path: PathBuf,
    pub framework: Option<RouteFramework>,
    pub route_files: Vec<PathBuf>,
    pub route_prefix: Option<String>,
    pub openapi: Option<PathBuf>,
    pub detected_kind: String,
}

/// Resolved `(config_path, repo_name)` context for `laine check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedContext {
    pub config_path: PathBuf,
    pub repo_name: String,
    pub workspace_name: Option<String>,
}

impl WorkspaceRegistry {
    pub fn empty() -> Self {
        Self {
            version: 1,
            workspaces: BTreeMap::new(),
        }
    }

    /// Returns the default registry path (`$LAINE_HOME/workspaces.toml` or `~/.laine/workspaces.toml`).
    pub fn default_path() -> Result<PathBuf> {
        if let Ok(custom_home) = env::var("LAINE_HOME")
            && !custom_home.trim().is_empty()
        {
            return Ok(PathBuf::from(custom_home).join("workspaces.toml"));
        }

        let home = env::var("USERPROFILE")
            .or_else(|_| env::var("HOME"))
            .map(PathBuf::from)
            .map_err(|_| {
                LaineError::ConfigInvalid(
                    "could not determine home directory (set LAINE_HOME, HOME, or USERPROFILE)"
                        .into(),
                )
            })?;

        Ok(home.join(".laine").join("workspaces.toml"))
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::empty());
        }
        let raw = fs::read_to_string(path).map_err(|source| LaineError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let reg: WorkspaceRegistry = toml::from_str(&raw)?;
        if reg.version != 1 {
            return Err(LaineError::ConfigInvalid(format!(
                "unsupported workspace registry version {} (expected 1)",
                reg.version
            )));
        }
        Ok(reg)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|source| LaineError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let serialized = toml::to_string_pretty(self)
            .map_err(|err| LaineError::ConfigInvalid(err.to_string()))?;
        fs::write(path, serialized).map_err(|source| LaineError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn register(&mut self, name: impl Into<String>, config_path: PathBuf) {
        let normalized = normalize_absolute_path(&config_path);
        self.workspaces.insert(name.into(), normalized);
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.workspaces.remove(name).is_some()
    }
}

/// Scans immediate subdirectories of `root` for local Git repositories and detects
/// their backend route framework, OpenAPI specs, and consumer stack without executing code.
pub fn scan_directory(root: &Path) -> Result<Vec<DiscoveredRepo>> {
    if !root.exists() {
        return Err(LaineError::ConfigInvalid(format!(
            "scan directory does not exist: {}",
            root.display()
        )));
    }

    let entries = fs::read_dir(root).map_err(|source| LaineError::Io {
        path: root.to_path_buf(),
        source,
    })?;

    let mut repos = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| LaineError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name_os) = path.file_name() else {
            continue;
        };
        let name = name_os.to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if !path.join(".git").exists() {
            continue;
        }

        repos.push(inspect_git_repo(&name, &path));
    }

    repos.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(repos)
}

/// Builds a `WorkspaceConfig` from a user-selected subset of discovered repositories.
pub fn build_workspace_config(
    repos: &[DiscoveredRepo],
    config_dir: &Path,
    use_relative_paths: bool,
) -> WorkspaceConfig {
    let config_dir_abs = normalize_absolute_path(config_dir);

    let services = repos
        .iter()
        .map(|repo| {
            let repo_abs = normalize_absolute_path(&repo.path);
            let stored_path = if use_relative_paths {
                repo_abs
                    .strip_prefix(&config_dir_abs)
                    .map(|rel| PathBuf::from(".").join(rel))
                    .unwrap_or(repo_abs)
            } else {
                repo_abs
            };

            let routes = repo.framework.map(|framework| RouteSourceConfig {
                framework,
                files: repo.route_files.clone(),
                prefix: repo.route_prefix.clone(),
            });

            let base_urls = if repo.openapi.is_some() || routes.is_some() {
                vec![format!("http://{}", repo.name)]
            } else {
                Vec::new()
            };

            ServiceConfig {
                name: repo.name.clone(),
                path: stored_path,
                openapi: repo.openapi.clone(),
                routes,
                base_urls,
            }
        })
        .collect();

    WorkspaceConfig {
        version: 1,
        services,
        clients: Vec::new(),
        env: BTreeMap::new(),
        policy: PolicyConfig {
            fail_on: Severity::High,
            ignore_endpoints: Vec::new(),
            fail_on_unresolved: false,
        },
        cache_path: None,
    }
}

/// Resolves the target workspace configuration file and service repository name from `cwd`,
/// checking explicit flags, ancestor `laine.toml` files, and the named workspace registry.
pub fn resolve_context_from_cwd(
    cwd: &Path,
    explicit_config: Option<&Path>,
    explicit_workspace: Option<&str>,
    explicit_repo: Option<&str>,
    registry_path: &Path,
) -> Result<ResolvedContext> {
    let cwd_abs = normalize_absolute_path(cwd);

    // 1. Explicit `--config`
    if let Some(cfg_path) = explicit_config {
        let cfg = WorkspaceConfig::load_from_path(cfg_path)?;
        let repo_name = resolve_repo_in_config(cfg_path, &cfg, &cwd_abs, explicit_repo)?;
        return Ok(ResolvedContext {
            config_path: normalize_absolute_path(cfg_path),
            repo_name,
            workspace_name: None,
        });
    }

    // 2. Explicit `--workspace <name>`
    if let Some(ws_name) = explicit_workspace {
        let registry = WorkspaceRegistry::load_from_path(registry_path)?;
        let Some(cfg_path) = registry.workspaces.get(ws_name) else {
            let known: Vec<&str> = registry.workspaces.keys().map(String::as_str).collect();
            let hint = if known.is_empty() {
                "no workspaces are registered yet (run `laine init --scan <dir> --workspace <name>`)"
                    .to_string()
            } else {
                format!("available workspaces: {}", known.join(", "))
            };
            return Err(LaineError::ConfigInvalid(format!(
                "workspace '{ws_name}' not found in {} ({hint})",
                registry_path.display()
            )));
        };
        let cfg = WorkspaceConfig::load_from_path(cfg_path)?;
        let repo_name = resolve_repo_in_config(cfg_path, &cfg, &cwd_abs, explicit_repo)?;
        return Ok(ResolvedContext {
            config_path: cfg_path.clone(),
            repo_name,
            workspace_name: Some(ws_name.to_string()),
        });
    }

    // 3. Walk up ancestors looking for `laine.toml` that owns `cwd`
    let mut nearest_ancestor_cfg: Option<PathBuf> = None;
    for ancestor in cwd_abs.ancestors() {
        let candidate = ancestor.join("laine.toml");
        if candidate.is_file() {
            if nearest_ancestor_cfg.is_none() {
                nearest_ancestor_cfg = Some(candidate.clone());
            }
            if let Ok(cfg) = WorkspaceConfig::load_from_path(&candidate)
                && let Ok(repo_name) =
                    resolve_repo_in_config(&candidate, &cfg, &cwd_abs, explicit_repo)
            {
                return Ok(ResolvedContext {
                    config_path: candidate,
                    repo_name,
                    workspace_name: None,
                });
            }
        }
    }

    // 4. Match `cwd` against all registered workspaces in `~/.laine/workspaces.toml`
    let registry = WorkspaceRegistry::load_from_path(registry_path)?;
    let mut matches: Vec<ResolvedContext> = Vec::new();
    for (ws_name, cfg_path) in &registry.workspaces {
        if !cfg_path.is_file() {
            continue;
        }
        let Ok(cfg) = WorkspaceConfig::load_from_path(cfg_path) else {
            continue;
        };
        if let Ok(repo_name) = resolve_repo_in_config(cfg_path, &cfg, &cwd_abs, explicit_repo) {
            matches.push(ResolvedContext {
                config_path: cfg_path.clone(),
                repo_name,
                workspace_name: Some(ws_name.clone()),
            });
        }
    }

    if matches.len() == 1 {
        return Ok(matches.remove(0));
    }
    if matches.len() > 1 {
        let ws_names: Vec<&str> = matches
            .iter()
            .filter_map(|m| m.workspace_name.as_deref())
            .collect();
        return Err(LaineError::ConfigInvalid(format!(
            "current directory belongs to multiple workspaces ({}); disambiguate with `--workspace <name>`",
            ws_names.join(", ")
        )));
    }

    // 5. Helpful fallback error
    if let Some(cfg_path) = nearest_ancestor_cfg {
        let cfg = WorkspaceConfig::load_from_path(&cfg_path)?;
        let services: Vec<&str> = cfg.services.iter().map(|s| s.name.as_str()).collect();
        return Err(LaineError::ConfigInvalid(format!(
            "found {} but current directory {} does not match any configured service (available: {}; pass `--repo <name>`)",
            cfg_path.display(),
            cwd_abs.display(),
            services.join(", ")
        )));
    }

    Err(LaineError::ConfigInvalid(format!(
        "could not find a Laine workspace for {}. Run `laine init --scan <projects-dir> --workspace <name>` or `laine workspace add <name> <laine.toml>`",
        cwd_abs.display()
    )))
}

fn resolve_repo_in_config(
    config_path: &Path,
    config: &WorkspaceConfig,
    cwd_abs: &Path,
    explicit_repo: Option<&str>,
) -> Result<String> {
    if let Some(repo) = explicit_repo {
        if config.find_service(repo).is_some() {
            return Ok(repo.to_string());
        }
        return Err(LaineError::ConfigInvalid(format!(
            "service '{repo}' not found in {}",
            config_path.display()
        )));
    }

    let config_dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut best_match: Option<(usize, String)> = None;
    for service in &config.services {
        let raw_service_path = if service.path.is_absolute() {
            service.path.clone()
        } else {
            config_dir.join(&service.path)
        };
        let service_abs = normalize_absolute_path(&raw_service_path);
        if path_starts_with_ci(cwd_abs, &service_abs) {
            let depth = service_abs.components().count();
            if best_match.as_ref().is_none_or(|(best, _)| depth > *best) {
                best_match = Some((depth, service.name.clone()));
            }
        }
    }

    best_match.map(|(_, name)| name).ok_or_else(|| {
        LaineError::ConfigInvalid(format!(
            "current directory {} is not inside any service declared in {}",
            cwd_abs.display(),
            config_path.display()
        ))
    })
}

fn inspect_git_repo(name: &str, path: &Path) -> DiscoveredRepo {
    let openapi = detect_openapi_file(path);
    let (framework, route_files, route_prefix) = detect_backend_framework(path);
    let detected_kind = describe_detected_kind(path, framework, openapi.is_some());

    DiscoveredRepo {
        name: name.to_string(),
        path: normalize_absolute_path(path),
        framework,
        route_files,
        route_prefix,
        openapi,
        detected_kind,
    }
}

fn detect_openapi_file(repo_root: &Path) -> Option<PathBuf> {
    const CANDIDATES: &[&str] = &[
        "openapi.yaml",
        "openapi.yml",
        "openapi.json",
        "swagger.yaml",
        "swagger.yml",
        "swagger.json",
        "docs/openapi.yaml",
        "docs/openapi.yml",
        "docs/openapi.json",
    ];
    for rel in CANDIDATES {
        if repo_root.join(rel).is_file() {
            return Some(PathBuf::from(rel));
        }
    }
    None
}

fn detect_backend_framework(
    repo_root: &Path,
) -> (Option<RouteFramework>, Vec<PathBuf>, Option<String>) {
    // 1. Laravel (PHP)
    let has_api_routes = repo_root.join("routes/api.php").is_file();
    let has_web_routes = repo_root.join("routes/web.php").is_file();
    if repo_root.join("artisan").is_file() || has_api_routes {
        if has_api_routes {
            return (
                Some(RouteFramework::Laravel),
                vec![PathBuf::from("routes/api.php")],
                Some("/api".to_string()),
            );
        }
        if has_web_routes {
            return (
                Some(RouteFramework::Laravel),
                vec![PathBuf::from("routes/web.php")],
                None,
            );
        }
        return (Some(RouteFramework::Laravel), Vec::new(), None);
    }

    // 2. Express (Node/TS)
    if let Ok(raw_pkg) = fs::read_to_string(repo_root.join("package.json"))
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw_pkg)
    {
        let has_express = json
            .get("dependencies")
            .and_then(|d| d.get("express"))
            .is_some()
            || json
                .get("devDependencies")
                .and_then(|d| d.get("express"))
                .is_some();
        if has_express {
            return (Some(RouteFramework::Express), Vec::new(), None);
        }
    }

    // 3. Python (FastAPI / Flask)
    let mut py_manifest = String::new();
    if let Ok(s) = fs::read_to_string(repo_root.join("pyproject.toml")) {
        py_manifest.push_str(&s);
        py_manifest.push('\n');
    }
    if let Ok(s) = fs::read_to_string(repo_root.join("requirements.txt")) {
        py_manifest.push_str(&s);
    }
    if !py_manifest.is_empty() {
        let lower = py_manifest.to_ascii_lowercase();
        if lower.contains("fastapi") {
            return (Some(RouteFramework::FastApi), Vec::new(), None);
        }
        if lower.contains("flask") {
            return (Some(RouteFramework::Flask), Vec::new(), None);
        }
    }

    (None, Vec::new(), None)
}

fn describe_detected_kind(
    repo_root: &Path,
    framework: Option<RouteFramework>,
    has_openapi: bool,
) -> String {
    let mut parts = Vec::new();
    if has_openapi {
        parts.push("OpenAPI");
    }
    if let Some(fw) = framework {
        parts.push(match fw {
            RouteFramework::Laravel => "Laravel API",
            RouteFramework::Express => "Express API",
            RouteFramework::FastApi => "FastAPI",
            RouteFramework::Flask => "Flask API",
        });
    }
    if !parts.is_empty() {
        return parts.join(" + ");
    }

    if repo_root.join("nuxt.config.ts").is_file() || repo_root.join("nuxt.config.js").is_file() {
        return "Nuxt/TS consumer".to_string();
    }
    if repo_root.join("package.json").is_file() {
        return "TS/JS consumer".to_string();
    }
    if repo_root.join("pyproject.toml").is_file() || repo_root.join("requirements.txt").is_file() {
        return "Python consumer".to_string();
    }
    "Git repository".to_string()
}

pub fn normalize_absolute_path(path: &Path) -> PathBuf {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    });
    strip_windows_verbatim_prefix(&resolved)
}

fn strip_windows_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(stripped) = raw.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path.to_path_buf()
    }
}

fn path_starts_with_ci(path: &Path, prefix: &Path) -> bool {
    if cfg!(windows) {
        let p_comps: Vec<String> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect();
        let pre_comps: Vec<String> = prefix
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect();
        p_comps.len() >= pre_comps.len() && p_comps[..pre_comps.len()] == pre_comps[..]
    } else {
        path.starts_with(prefix)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::*;
    use crate::config::ServiceConfig;

    fn init_git_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        let status = Command::new("git")
            .current_dir(dir)
            .args(["init", "-b", "main"])
            .status()
            .expect("git init should run");
        assert!(status.success());
    }

    #[test]
    fn scans_directory_and_detects_laravel_openapi_express_fastapi_and_nuxt_repos() {
        let tmp = std::env::temp_dir().join(format!("laine-ws-scan-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // 1. Laravel repo
        let laravel = tmp.join("cerebro-web");
        init_git_repo(&laravel);
        fs::write(laravel.join("artisan"), "#!/usr/bin/env php\n").unwrap();
        fs::create_dir_all(laravel.join("routes")).unwrap();
        fs::write(laravel.join("routes/api.php"), "<?php\n").unwrap();

        // 2. Nuxt / TS consumer repo
        let nuxt = tmp.join("portal-corporativo");
        init_git_repo(&nuxt);
        fs::write(nuxt.join("nuxt.config.ts"), "export default {}\n").unwrap();
        fs::write(nuxt.join("package.json"), r#"{"name": "portal"}"#).unwrap();

        // 3. OpenAPI + Express repo
        let express = tmp.join("billing-api");
        init_git_repo(&express);
        fs::write(
            express.join("package.json"),
            r#"{"dependencies": {"express": "^4.19.0"}}"#,
        )
        .unwrap();
        fs::write(express.join("openapi.yaml"), "openapi: 3.0.3\npaths: {}\n").unwrap();

        // 4. Python FastAPI repo
        let fastapi = tmp.join("ml-service");
        init_git_repo(&fastapi);
        fs::write(
            fastapi.join("pyproject.toml"),
            "[project]\ndependencies = [\"fastapi\"]\n",
        )
        .unwrap();

        // 5. Non-git directory should be ignored
        fs::create_dir_all(tmp.join("random-notes")).unwrap();

        let repos = scan_directory(&tmp).expect("scan_directory should succeed");
        let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "billing-api",
                "cerebro-web",
                "ml-service",
                "portal-corporativo"
            ]
        );

        let lar = repos.iter().find(|r| r.name == "cerebro-web").unwrap();
        assert_eq!(lar.framework, Some(RouteFramework::Laravel));
        assert_eq!(lar.route_files, vec![PathBuf::from("routes/api.php")]);
        assert_eq!(lar.route_prefix.as_deref(), Some("/api"));

        let exp = repos.iter().find(|r| r.name == "billing-api").unwrap();
        assert_eq!(exp.framework, Some(RouteFramework::Express));
        assert_eq!(exp.openapi, Some(PathBuf::from("openapi.yaml")));

        let py = repos.iter().find(|r| r.name == "ml-service").unwrap();
        assert_eq!(py.framework, Some(RouteFramework::FastApi));

        let fe = repos
            .iter()
            .find(|r| r.name == "portal-corporativo")
            .unwrap();
        assert_eq!(fe.framework, None);
        assert_eq!(fe.openapi, None);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolves_isolated_workspaces_from_shared_projects_folder() {
        let tmp =
            std::env::temp_dir().join(format!("laine-ws-resolve-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Two unrelated ecosystems living in the exact same parent directory
        let projects = tmp.join("Projects");
        let eco_a_api = projects.join("eco-a-api");
        let eco_a_web = projects.join("eco-a-web");
        let eco_b_api = projects.join("eco-b-api");
        let eco_b_web = projects.join("eco-b-web");
        for dir in [&eco_a_api, &eco_a_web, &eco_b_api, &eco_b_web] {
            init_git_repo(dir);
        }
        let nested_dir = eco_a_api.join("routes");
        fs::create_dir_all(&nested_dir).unwrap();

        let cfg_a_path = tmp.join("laine.eco-a.toml");
        WorkspaceConfig {
            version: 1,
            services: vec![
                ServiceConfig {
                    name: "eco-a-api".into(),
                    path: eco_a_api.clone(),
                    openapi: None,
                    routes: None,
                    base_urls: Vec::new(),
                },
                ServiceConfig {
                    name: "eco-a-web".into(),
                    path: eco_a_web.clone(),
                    openapi: None,
                    routes: None,
                    base_urls: Vec::new(),
                },
            ],
            clients: Vec::new(),
            env: Default::default(),
            policy: Default::default(),
            cache_path: None,
        }
        .save_to_path(&cfg_a_path)
        .unwrap();

        let cfg_b_path = tmp.join("laine.eco-b.toml");
        WorkspaceConfig {
            version: 1,
            services: vec![
                ServiceConfig {
                    name: "eco-b-api".into(),
                    path: eco_b_api.clone(),
                    openapi: None,
                    routes: None,
                    base_urls: Vec::new(),
                },
                ServiceConfig {
                    name: "eco-b-web".into(),
                    path: eco_b_web.clone(),
                    openapi: None,
                    routes: None,
                    base_urls: Vec::new(),
                },
            ],
            clients: Vec::new(),
            env: Default::default(),
            policy: Default::default(),
            cache_path: None,
        }
        .save_to_path(&cfg_b_path)
        .unwrap();

        let registry_path = tmp.join("home").join("workspaces.toml");
        let mut reg = WorkspaceRegistry::empty();
        reg.register("eco-a", cfg_a_path.clone());
        reg.register("eco-b", cfg_b_path.clone());
        reg.save_to_path(&registry_path).unwrap();

        // Running from a subdirectory inside eco-a-api resolves eco-a + eco-a-api automatically
        let ctx_a =
            resolve_context_from_cwd(&nested_dir, None, None, None, &registry_path).unwrap();
        assert_eq!(ctx_a.repo_name, "eco-a-api");
        assert_eq!(ctx_a.workspace_name.as_deref(), Some("eco-a"));

        // Running from eco-b-api resolves eco-b + eco-b-api automatically
        let ctx_b = resolve_context_from_cwd(&eco_b_api, None, None, None, &registry_path).unwrap();
        assert_eq!(ctx_b.repo_name, "eco-b-api");
        assert_eq!(ctx_b.workspace_name.as_deref(), Some("eco-b"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reports_ambiguity_when_repo_belongs_to_multiple_workspaces_and_allows_explicit_selection() {
        let tmp = std::env::temp_dir().join(format!("laine-ws-ambig-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let shared_api = tmp.join("shared-api");
        init_git_repo(&shared_api);

        let cfg1 = tmp.join("ws1.toml");
        let cfg2 = tmp.join("ws2.toml");
        for cfg_path in [&cfg1, &cfg2] {
            WorkspaceConfig {
                version: 1,
                services: vec![ServiceConfig {
                    name: "shared-api".into(),
                    path: shared_api.clone(),
                    openapi: None,
                    routes: None,
                    base_urls: Vec::new(),
                }],
                clients: Vec::new(),
                env: Default::default(),
                policy: Default::default(),
                cache_path: None,
            }
            .save_to_path(cfg_path)
            .unwrap();
        }

        let registry_path = tmp.join("workspaces.toml");
        let mut reg = WorkspaceRegistry::empty();
        reg.register("ws1", cfg1.clone());
        reg.register("ws2", cfg2.clone());
        reg.save_to_path(&registry_path).unwrap();

        let err = resolve_context_from_cwd(&shared_api, None, None, None, &registry_path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ws1") && err.contains("ws2"));

        let explicit =
            resolve_context_from_cwd(&shared_api, None, Some("ws2"), None, &registry_path).unwrap();
        assert_eq!(explicit.workspace_name.as_deref(), Some("ws2"));
        assert_eq!(explicit.repo_name, "shared-api");

        let _ = fs::remove_dir_all(&tmp);
    }
}
