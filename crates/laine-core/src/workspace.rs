use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{
    CustomClientConfig, PolicyConfig, RouteFramework, RouteSourceConfig, ServiceConfig,
    WorkspaceConfig,
};
use crate::error::{LaineError, Result};
use crate::git::GitRepo;
use crate::graph::routes_match;
use crate::laravel::extract_laravel_routes;
use crate::model::Severity;
use crate::openapi::{canonical_route_key, extract_contract};

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

    /// Returns the base directory for Laine user state (`$LAINE_HOME` or `~/.laine`).
    pub fn default_home_dir() -> Result<PathBuf> {
        if let Ok(custom_home) = env::var("LAINE_HOME")
            && !custom_home.trim().is_empty()
        {
            return Ok(PathBuf::from(custom_home));
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

        Ok(home.join(".laine"))
    }

    /// Returns the default registry path (`$LAINE_HOME/workspaces.toml` or `~/.laine/workspaces.toml`).
    pub fn default_path() -> Result<PathBuf> {
        Ok(Self::default_home_dir()?.join("workspaces.toml"))
    }

    /// Returns the default managed config path for a named workspace
    /// (`$LAINE_HOME/configs/<name>.toml` or `~/.laine/configs/<name>.toml`).
    pub fn default_config_path(workspace_name: &str) -> Result<PathBuf> {
        Ok(Self::default_home_dir()?
            .join("configs")
            .join(format!("{workspace_name}.toml")))
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

    let mut env_vars = BTreeMap::new();
    for repo in repos {
        for (key, value) in detect_env_urls_in_repo(&repo.path) {
            env_vars.entry(key).or_insert(value);
        }
    }

    let clients = detect_custom_clients(repos);

    WorkspaceConfig {
        version: 1,
        services,
        clients,
        env: env_vars,
        policy: PolicyConfig {
            fail_on: Severity::High,
            ignore_endpoints: Vec::new(),
            fail_on_unresolved: false,
        },
        cache_path: None,
    }
}

/// Infers the common parent directory containing a workspace's repositories so
/// `laine workspace edit <name>` can rescan sibling repos automatically.
pub fn infer_workspace_scan_dir(config: &WorkspaceConfig, config_path: &Path) -> Option<PathBuf> {
    let config_dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut parents: Vec<PathBuf> = Vec::new();
    for service in &config.services {
        let raw = if service.path.is_absolute() {
            service.path.clone()
        } else {
            config_dir.join(&service.path)
        };
        let abs = normalize_absolute_path(&raw);
        if let Some(parent) = abs.parent() {
            parents.push(parent.to_path_buf());
        }
    }

    let first = parents.first()?.clone();
    let mut common: Vec<_> = first.components().collect();
    for parent in &parents[1..] {
        let comps: Vec<_> = parent.components().collect();
        let mut shared = 0usize;
        while shared < common.len() && shared < comps.len() {
            let same = if cfg!(windows) {
                common[shared]
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&comps[shared].as_os_str().to_string_lossy())
            } else {
                common[shared] == comps[shared]
            };
            if same {
                shared += 1;
            } else {
                break;
            }
        }
        common.truncate(shared);
    }

    if common.is_empty() {
        None
    } else {
        Some(common.into_iter().collect())
    }
}

/// Inspects a single repository directory into a `DiscoveredRepo` descriptor.
pub fn inspect_repo_path(path: &Path) -> DiscoveredRepo {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "service".to_string());
    inspect_git_repo(&name, path)
}

/// Updates an existing `WorkspaceConfig` with a new repository selection while preserving
/// existing policy, cache path, manual env/client entries, and per-service overrides.
pub fn merge_workspace_config(
    existing: &WorkspaceConfig,
    selected: &[DiscoveredRepo],
    config_dir: &Path,
) -> WorkspaceConfig {
    let mut merged = build_workspace_config(selected, config_dir, false);
    merged.policy = existing.policy.clone();
    merged.cache_path = existing.cache_path.clone();

    // Preserve per-service overrides (`openapi`, `routes`, `base_urls`) for kept repositories.
    for service in &mut merged.services {
        let svc_abs = normalize_absolute_path(&service.path);
        let prev = existing.services.iter().find(|old| {
            let old_raw = if old.path.is_absolute() {
                old.path.clone()
            } else {
                config_dir.join(&old.path)
            };
            old.name == service.name || path_equals_ci(&normalize_absolute_path(&old_raw), &svc_abs)
        });

        if let Some(old) = prev {
            if old.openapi.is_some() {
                service.openapi = old.openapi.clone();
            }
            if old.routes.is_some() {
                service.routes = old.routes.clone();
            }
            if !old.base_urls.is_empty() {
                service.base_urls = old.base_urls.clone();
            }
        }
    }

    // Preserve manual `[env]` overrides on top of newly auto-detected URLs.
    for (k, v) in &existing.env {
        merged.env.insert(k.clone(), v.clone());
    }

    // Preserve existing `[[clients]]` whose `target_service` is still selected,
    // without duplicating a `callee` already auto-detected by `build_workspace_config`.
    let kept_service_names: BTreeSet<&str> =
        merged.services.iter().map(|s| s.name.as_str()).collect();
    let mut existing_callees: BTreeSet<String> =
        merged.clients.iter().map(|c| c.callee.clone()).collect();

    for old_client in &existing.clients {
        if !kept_service_names.contains(old_client.target_service.as_str()) {
            continue;
        }
        if existing_callees.insert(old_client.callee.clone()) {
            merged.clients.push(old_client.clone());
        }
    }

    merged
        .clients
        .sort_by(|a, b| (&a.callee, &a.target_service).cmp(&(&b.callee, &b.target_service)));

    merged
}

fn path_equals_ci(a: &Path, b: &Path) -> bool {
    path_starts_with_ci(a, b) && path_starts_with_ci(b, a)
}

fn detect_env_urls_in_repo(repo_root: &Path) -> BTreeMap<String, String> {
    let mut env_map = BTreeMap::new();
    for filename in [".env.example", ".env.development"] {
        let path = repo_root.join(filename);
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((raw_key, raw_val)) = trimmed.split_once('=') else {
                continue;
            };
            let key = raw_key.trim().trim_start_matches("export ").trim();
            if key.is_empty() {
                continue;
            }
            let key_upper = key.to_ascii_uppercase();
            if key_upper.contains("SECRET")
                || key_upper.contains("PASSWORD")
                || key_upper.contains("TOKEN")
                || key_upper.contains("KEY")
            {
                continue;
            }
            let val = raw_val.trim().trim_matches('"').trim_matches('\'').trim();
            if val.starts_with("http://") || val.starts_with("https://") {
                env_map
                    .entry(key.to_string())
                    .or_insert_with(|| val.to_string());
            }
        }
    }
    env_map
}

struct ProducerCatalog {
    name: String,
    prefix: Option<String>,
    canonical_routes: Vec<String>,
}

fn detect_custom_clients(repos: &[DiscoveredRepo]) -> Vec<CustomClientConfig> {
    let producers: Vec<ProducerCatalog> = repos
        .iter()
        .filter(|r| r.openapi.is_some() || r.framework.is_some())
        .map(build_producer_catalog)
        .collect();

    if producers.is_empty() {
        return Vec::new();
    }

    let mut observed_by_callee: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for repo in repos {
        if repo.openapi.is_some() || repo.framework.is_some() {
            continue;
        }
        collect_consumer_wrapper_paths(&repo.path, 0, &mut observed_by_callee);
    }

    let mut clients = Vec::new();
    for (callee, paths) in observed_by_callee {
        if paths.is_empty() {
            continue;
        }
        if let Some(best) = select_best_producer(&producers, &paths) {
            clients.push(CustomClientConfig {
                callee,
                target_service: best.name.clone(),
                path_prefix: best.prefix.clone(),
            });
        }
    }

    clients
}

fn read_repo_file_preferring_head(repo_root: &Path, rel: &Path) -> Option<String> {
    let git = GitRepo::new(repo_root);
    if let Ok(committed) = git.read_file("HEAD", rel) {
        return Some(committed);
    }
    fs::read(repo_root.join(rel))
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn build_producer_catalog(repo: &DiscoveredRepo) -> ProducerCatalog {
    let mut routes = BTreeSet::new();

    if let Some(openapi_rel) = repo.openapi.as_deref()
        && let Some(raw) = read_repo_file_preferring_head(&repo.path, openapi_rel)
        && let Ok(contract) = extract_contract(&repo.name, &raw)
    {
        for ep in contract.endpoints.keys() {
            routes.insert(canonical_route_key(&ep.normalized_path));
        }
    }

    if repo.framework == Some(RouteFramework::Laravel) {
        for rel in &repo.route_files {
            if let Some(raw) = read_repo_file_preferring_head(&repo.path, rel) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                for extracted in
                    extract_laravel_routes(&repo.name, &rel_str, &raw, repo.route_prefix.as_deref())
                {
                    routes.insert(canonical_route_key(&extracted.endpoint.normalized_path));
                }
            }
        }
    }

    ProducerCatalog {
        name: repo.name.clone(),
        prefix: repo.route_prefix.clone(),
        canonical_routes: routes.into_iter().collect(),
    }
}

fn select_best_producer<'a>(
    producers: &'a [ProducerCatalog],
    consumer_paths: &BTreeSet<String>,
) -> Option<&'a ProducerCatalog> {
    let mut best: Option<(usize, &'a ProducerCatalog)> = None;

    for producer in producers {
        let mut hits = 0usize;
        for raw_path in consumer_paths {
            let prefixed = match producer.prefix.as_deref() {
                Some(prefix) if !raw_path.starts_with(prefix) => {
                    format!(
                        "{}/{}",
                        prefix.trim_end_matches('/'),
                        raw_path.trim_start_matches('/')
                    )
                }
                _ => raw_path.clone(),
            };
            let canonical_consumer = canonical_route_key(&prefixed);
            if producer
                .canonical_routes
                .iter()
                .any(|prod_key| routes_match(prod_key, &canonical_consumer))
            {
                hits += 1;
            }
        }

        if hits > 0 && best.as_ref().is_none_or(|(best_hits, _)| hits > *best_hits) {
            best = Some((hits, producer));
        }
    }

    if let Some((_, producer)) = best {
        return Some(producer);
    }

    // If there is only one producer and it has no statically scanned routes yet (e.g. Express),
    // map the custom client to it as the single backend in this workspace.
    if producers.len() == 1 {
        return Some(&producers[0]);
    }

    None
}

fn collect_consumer_wrapper_paths(
    dir: &Path,
    depth: usize,
    out: &mut BTreeMap<String, BTreeSet<String>>,
) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name_os) = path.file_name() else {
            continue;
        };
        let name = name_os.to_string_lossy();
        if path.is_dir() {
            if name.starts_with('.')
                || matches!(
                    name.as_ref(),
                    "node_modules" | "dist" | "build" | "coverage" | "vendor" | "target"
                )
            {
                continue;
            }
            collect_consumer_wrapper_paths(&path, depth + 1, out);
        } else if is_scannable_ts_source(&name)
            && let Ok(bytes) = fs::read(&path)
        {
            let source = String::from_utf8_lossy(&bytes);
            extract_wrapper_calls_from_source(&source, out);
        }
    }
}

fn is_scannable_ts_source(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".d.ts") {
        return false;
    }
    lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".vue")
        || lower.ends_with(".mjs")
}

fn extract_wrapper_calls_from_source(source: &str, out: &mut BTreeMap<String, BTreeSet<String>>) {
    const PATTERNS: &[(&str, &str)] = &[
        ("getClient().fetch", "getClient().fetch"),
        ("$fetch", "$fetch"),
        ("apiClient.get", "apiClient"),
        ("apiClient.post", "apiClient"),
        ("apiClient.put", "apiClient"),
        ("apiClient.patch", "apiClient"),
        ("apiClient.delete", "apiClient"),
        ("apiClient.fetch", "apiClient"),
        ("httpClient.get", "httpClient"),
        ("httpClient.post", "httpClient"),
        ("httpClient.put", "httpClient"),
        ("httpClient.patch", "httpClient"),
        ("httpClient.delete", "httpClient"),
        ("httpClient.fetch", "httpClient"),
    ];

    for (needle, callee) in PATTERNS {
        let mut search_from = 0usize;
        while let Some(rel_idx) = source[search_from..].find(needle) {
            let start = search_from + rel_idx;
            let after = start + needle.len();
            search_from = after;

            // Avoid matching inside a longer identifier (e.g. `my$fetch` or `xapiClient.get`)
            if start > 0 {
                let prev = source.as_bytes()[start - 1];
                if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'$' {
                    continue;
                }
            }

            if let Some(path) = extract_first_relative_path_arg(&source[after..]) {
                out.entry((*callee).to_string()).or_default().insert(path);
            }
        }
    }
}

fn extract_first_relative_path_arg(rest: &str) -> Option<String> {
    let bytes = rest.as_bytes();
    let mut i = 0usize;

    // Skip whitespace
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }

    // Optional TypeScript generic type argument `<...>` before `(`
    if bytes.get(i) == Some(&b'<') {
        let mut depth = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b'<' => depth += 1,
                b'>' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }

    if bytes.get(i) != Some(&b'(') {
        return None;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }

    let quote = *bytes.get(i)?;
    if quote != b'\'' && quote != b'"' && quote != b'`' {
        return None;
    }
    i += 1;
    let start = i;
    while i < bytes.len() && bytes[i] != quote {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == b'\n' {
            return None;
        }
        i += 1;
    }
    if bytes.get(i) != Some(&quote) {
        return None;
    }

    let raw = &rest[start..i];
    if !raw.starts_with('/') || raw.starts_with("//") {
        return None;
    }

    // Strip query string and replace template interpolations `${...}` with `:param`
    let without_query = raw.split('?').next().unwrap_or(raw);
    let mut normalized = String::new();
    let mut cursor = 0usize;
    while let Some(pos) = without_query[cursor..].find("${") {
        normalized.push_str(&without_query[cursor..cursor + pos]);
        normalized.push_str(":param");
        let after_open = cursor + pos + 2;
        let close = without_query[after_open..].find('}')?;
        cursor = after_open + close + 1;
    }
    normalized.push_str(&without_query[cursor..]);
    Some(normalized)
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

    #[test]
    fn build_workspace_config_detects_env_urls_and_maps_custom_clients_to_matching_producer() {
        let tmp =
            std::env::temp_dir().join(format!("laine-ws-autodetect-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Producer 1: unrelated Laravel API
        let other_api = tmp.join("other-api");
        init_git_repo(&other_api);
        fs::write(other_api.join("artisan"), "#!/usr/bin/env php\n").unwrap();
        fs::create_dir_all(other_api.join("routes")).unwrap();
        fs::write(
            other_api.join("routes/api.php"),
            "<?php\nRoute::get('/other/ping', 'PingController@ping');\n",
        )
        .unwrap();

        // Producer 2: target Laravel API with optional param route
        let cerebro_web = tmp.join("cerebro-web");
        init_git_repo(&cerebro_web);
        fs::write(cerebro_web.join("artisan"), "#!/usr/bin/env php\n").unwrap();
        fs::create_dir_all(cerebro_web.join("routes")).unwrap();
        fs::write(
            cerebro_web.join("routes/api.php"),
            "<?php\nRoute::post('corp/{accion}/{param?}', [Corp::class, 'accion']);\nRoute::post('/reset_password', 'Auth@reset');\n",
        )
        .unwrap();

        // Consumer: Nuxt/TS portal with .env.example URL + custom wrapper `getClient().fetch`
        let portal = tmp.join("portal-web");
        init_git_repo(&portal);
        fs::write(portal.join("nuxt.config.ts"), "export default {}\n").unwrap();
        fs::write(
            portal.join(".env.example"),
            "# comment\nNUXT_PUBLIC_API=https://api.cerebro.local/api\nRECAPTCHA_KEY=secret-should-be-ignored\nEMPTY_URL=\n",
        )
        .unwrap();
        fs::create_dir_all(portal.join("app/services")).unwrap();
        fs::write(
            portal.join("app/services/auth.ts"),
            r#"export function login() {
  return getClient().fetch<LoginResp>('/corp/loginCorp', { method: 'POST' });
}
export function reset() {
  return getClient().fetch('/reset_password', { method: 'POST' });
}
"#,
        )
        .unwrap();

        let repos = scan_directory(&tmp).expect("scan should succeed");
        let cfg = build_workspace_config(&repos, &tmp, false);

        assert_eq!(
            cfg.env.get("NUXT_PUBLIC_API").map(String::as_str),
            Some("https://api.cerebro.local/api")
        );
        assert!(!cfg.env.contains_key("RECAPTCHA_KEY"));

        assert_eq!(
            cfg.clients.len(),
            1,
            "expected 1 auto-detected client: {:?}",
            cfg.clients
        );
        assert_eq!(cfg.clients[0].callee, "getClient().fetch");
        assert_eq!(cfg.clients[0].target_service, "cerebro-web");
        assert_eq!(cfg.clients[0].path_prefix.as_deref(), Some("/api"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn infers_common_parent_dir_and_merges_workspace_edits_preserving_customizations() {
        let tmp = std::env::temp_dir().join(format!("laine-ws-merge-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let projects = tmp.join("Projects");
        fs::create_dir_all(&projects).unwrap();

        let backend = projects.join("cerebro-web");
        init_git_repo(&backend);
        fs::write(backend.join("artisan"), "#!/usr/bin/env php\n").unwrap();
        fs::create_dir_all(backend.join("routes")).unwrap();
        fs::write(
            backend.join("routes/api.php"),
            "<?php\nRoute::post('corp/{accion}/{param?}', [Corp::class, 'accion']);\n",
        )
        .unwrap();

        let old_fe = projects.join("portal-v1");
        init_git_repo(&old_fe);

        let new_fe = projects.join("portal-v2");
        init_git_repo(&new_fe);
        fs::write(new_fe.join("nuxt.config.ts"), "export default {}\n").unwrap();
        fs::create_dir_all(new_fe.join("app/services")).unwrap();
        fs::write(
            new_fe.join("app/services/auth.ts"),
            "export const login = () => getClient().fetch('/corp/loginCorp', { method: 'POST' });\n",
        )
        .unwrap();

        let cfg_path = tmp.join("configs").join("corp.toml");
        let existing = WorkspaceConfig {
            version: 1,
            services: vec![
                ServiceConfig {
                    name: "cerebro-web".into(),
                    path: backend.clone(),
                    openapi: Some("openapi.yaml".into()),
                    routes: Some(RouteSourceConfig {
                        framework: RouteFramework::Laravel,
                        files: vec!["routes/api.php".into()],
                        prefix: Some("/api".into()),
                    }),
                    base_urls: vec!["https://custom.internal".into()],
                },
                ServiceConfig {
                    name: "portal-v1".into(),
                    path: old_fe.clone(),
                    openapi: None,
                    routes: None,
                    base_urls: Vec::new(),
                },
            ],
            clients: vec![CustomClientConfig {
                callee: "legacyClient".into(),
                target_service: "portal-v1".into(),
                path_prefix: None,
            }],
            env: BTreeMap::from([("MANUAL_URL".into(), "https://manual.local".into())]),
            policy: PolicyConfig {
                fail_on: Severity::Critical,
                ignore_endpoints: vec!["/health".into()],
                fail_on_unresolved: true,
            },
            cache_path: Some(".cache/custom.sqlite".into()),
        };

        let inferred =
            infer_workspace_scan_dir(&existing, &cfg_path).expect("should infer parent dir");
        assert_eq!(
            normalize_absolute_path(&inferred),
            normalize_absolute_path(&projects)
        );

        let discovered = scan_directory(&projects).unwrap();
        let selected: Vec<DiscoveredRepo> = discovered
            .into_iter()
            .filter(|r| r.name == "cerebro-web" || r.name == "portal-v2")
            .collect();

        let merged = merge_workspace_config(&existing, &selected, tmp.join("configs").as_path());
        assert_eq!(merged.policy.fail_on, Severity::Critical);
        assert_eq!(merged.policy.ignore_endpoints, vec!["/health".to_string()]);
        assert_eq!(merged.cache_path, Some(".cache/custom.sqlite".into()));
        assert_eq!(
            merged.env.get("MANUAL_URL").map(String::as_str),
            Some("https://manual.local")
        );

        let kept_backend = merged.find_service("cerebro-web").unwrap();
        assert_eq!(kept_backend.openapi, Some("openapi.yaml".into()));
        assert_eq!(kept_backend.base_urls, vec!["https://custom.internal"]);
        assert!(merged.find_service("portal-v1").is_none());
        assert!(merged.find_service("portal-v2").is_some());

        // Stale client pointing to removed portal-v1 should be pruned, while new portal-v2 client is auto-detected
        assert!(!merged.clients.iter().any(|c| c.callee == "legacyClient"));
        assert!(merged.clients.iter().any(|c| {
            c.callee == "getClient().fetch"
                && c.target_service == "cerebro-web"
                && c.path_prefix.as_deref() == Some("/api")
        }));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn autodetects_correct_producer_from_head_even_when_worktree_renames_route() {
        let tmp =
            std::env::temp_dir().join(format!("laine-ws-head-catalog-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let commit_all = |dir: &Path| {
            for args in [
                vec!["config", "user.name", "Laine Test"],
                vec!["config", "user.email", "test@laine.local"],
                vec!["add", "."],
                vec!["commit", "-m", "init"],
            ] {
                let status = Command::new("git")
                    .current_dir(dir)
                    .args(&args)
                    .status()
                    .expect("git should run");
                assert!(status.success());
            }
        };

        // Producer 1 (competing backend): also exposes `corp/{accion}`
        let servicios = tmp.join("cerebro-servicios");
        init_git_repo(&servicios);
        fs::write(servicios.join("artisan"), "#!/usr/bin/env php\n").unwrap();
        fs::create_dir_all(servicios.join("routes")).unwrap();
        fs::write(
            servicios.join("routes/api.php"),
            "<?php\nRoute::post('corp/{accion}', [Servicios::class, 'accion']);\n",
        )
        .unwrap();
        commit_all(&servicios);

        // Producer 2 (true backend): exposes `corp/{accion}/{param?}`, `web/{accion}/{param?}`, `/reset_password`
        let web = tmp.join("cerebro-web");
        init_git_repo(&web);
        fs::write(web.join("artisan"), "#!/usr/bin/env php\n").unwrap();
        fs::create_dir_all(web.join("routes")).unwrap();
        fs::write(
            web.join("routes/api.php"),
            "<?php\nRoute::post('corp/{accion}/{param?}', [Corp::class, 'accion']);\nRoute::post('web/{accion}/{param?}', [Web::class, 'accion']);\nRoute::post('/reset_password', 'Auth@reset');\n",
        )
        .unwrap();
        commit_all(&web);

        // Simulate uncommitted breaking rename in cerebro-web's working tree
        fs::write(
            web.join("routes/api.php"),
            "<?php\nRoute::post('corp2/{accion}/{param?}', [Corp::class, 'accion']);\nRoute::post('web/{accion}/{param?}', [Web::class, 'accion']);\nRoute::post('/reset_password', 'Auth@reset');\n",
        )
        .unwrap();

        // Consumer calls 3 `/corp/*` routes + `/web/getRateCars` + `/reset_password`
        let portal = tmp.join("portal");
        init_git_repo(&portal);
        fs::write(portal.join("nuxt.config.ts"), "export default {}\n").unwrap();
        fs::create_dir_all(portal.join("app/services")).unwrap();
        fs::write(
            portal.join("app/services/api.ts"),
            r#"export const api = {
  login: () => getClient().fetch('/corp/loginCorp', { method: 'POST' }),
  me: () => getClient().fetch('/corp/me', { method: 'POST' }),
  list: () => getClient().fetch('/corp/listBooking', { method: 'POST' }),
  rates: () => getClient().fetch('/web/getRateCars', { method: 'POST' }),
  reset: () => getClient().fetch('/reset_password', { method: 'POST' }),
};
"#,
        )
        .unwrap();
        commit_all(&portal);

        let repos = scan_directory(&tmp).unwrap();
        let cfg = build_workspace_config(&repos, &tmp, false);

        assert_eq!(cfg.clients.len(), 1);
        assert_eq!(cfg.clients[0].callee, "getClient().fetch");
        assert_eq!(
            cfg.clients[0].target_service, "cerebro-web",
            "must match committed HEAD routes (5 hits) instead of losing to cerebro-servicios (3 hits) due to uncommitted worktree rename"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
