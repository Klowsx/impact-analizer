use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{RouteFramework, RouteSourceConfig, ServiceConfig, WorkspaceConfig};
use crate::error::{LaineError, Result};
use crate::git::{GitRepo, WORKTREE_REV, to_git_posix_path};
use crate::model::{ExtractionFailure, HttpMethod};
use crate::openapi::OpenApiSnapshot;
use crate::url::{is_absolute_http_url, matching_http_url};
use crate::workspace::{
    WorkspaceRegistry, normalize_absolute_path, same_repository_path, validate_workspace_name,
    workspace_service_path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ValidationSeverity {
    Error,
    Warning,
}

use ValidationSeverity::{Error, Warning};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub severity: ValidationSeverity,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
    pub remediation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceValidationEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub config_path: String,
    pub valid: bool,
    pub services_checked: usize,
    pub errors: usize,
    pub warnings: usize,
    pub issues: Vec<ValidationIssue>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceValidationSummary {
    pub workspaces_checked: usize,
    pub services_checked: usize,
    pub errors: usize,
    pub warnings: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceValidationReport {
    pub schema_version: u32,
    pub valid: bool,
    pub strict: bool,
    pub exit_code: u8,
    pub summary: WorkspaceValidationSummary,
    pub workspaces: Vec<WorkspaceValidationEntry>,
}

/// Return portable route diagnostics for the selected snapshot.
pub type RouteHealthCheck<'a> = dyn Fn(&GitRepo, &str, &str, &RouteSourceConfig) -> Result<Vec<(String, ExtractionFailure)>>
    + 'a;

impl WorkspaceValidationEntry {
    // Initialize counters and retain the target even when its config cannot be loaded.
    fn new(config_path: &Path, workspace: Option<&str>) -> Self {
        Self {
            workspace: workspace.map(str::to_owned),
            config_path: config_path.display().to_string(),
            valid: true,
            services_checked: 0,
            errors: 0,
            warnings: 0,
            issues: Vec::new(),
        }
    }

    // Collect an actionable issue without changing its severity in strict mode.
    fn issue(
        &mut self,
        severity: ValidationSeverity,
        code: &str,
        service: Option<&str>,
        field: Option<&str>,
        message: impl Into<String>,
        remediation: &str,
    ) {
        self.issues.push(ValidationIssue {
            severity,
            code: code.to_owned(),
            service: service.map(str::to_owned),
            field: field.map(str::to_owned),
            message: message.into(),
            remediation: remediation.to_owned(),
        });
    }
}

impl WorkspaceValidationReport {
    // Sort issues by severity, code and context before deriving the totals.
    fn new(
        strict: bool,
        workspaces_checked: usize,
        mut workspaces: Vec<WorkspaceValidationEntry>,
    ) -> Self {
        let mut summary = WorkspaceValidationSummary {
            workspaces_checked,
            ..WorkspaceValidationSummary::default()
        };
        for entry in &mut workspaces {
            entry.issues.sort();
            entry.issues.dedup();
            entry.errors = entry
                .issues
                .iter()
                .filter(|issue| issue.severity == Error)
                .count();
            entry.warnings = entry.issues.len() - entry.errors;
            entry.valid = entry.errors == 0 && (!strict || entry.warnings == 0);
            summary.errors += entry.errors;
            summary.warnings += entry.warnings;
            summary.services_checked += entry.services_checked;
        }
        let valid = summary.errors == 0 && (!strict || summary.warnings == 0);
        Self {
            schema_version: 1,
            valid,
            strict,
            exit_code: u8::from(!valid),
            summary,
            workspaces,
        }
    }

    /// Report an error that prevented selection of any workspace config.
    pub fn selection_error(
        path: &Path,
        workspace: Option<&str>,
        strict: bool,
        code: &str,
        message: impl Into<String>,
        remediation: &str,
    ) -> Self {
        let mut entry = WorkspaceValidationEntry::new(path, workspace);
        entry.issue(Error, code, None, None, message, remediation);
        Self::new(strict, 0, vec![entry])
    }
}

/// Validate a single workspace TOML and its configured repositories.
pub fn validate_workspace_config_with(
    config_path: &Path,
    workspace: Option<&str>,
    strict: bool,
    route_check: Option<&RouteHealthCheck<'_>>,
) -> WorkspaceValidationReport {
    WorkspaceValidationReport::new(
        strict,
        1,
        vec![validate_config_entry(
            WorkspaceValidationEntry::new(config_path, workspace),
            config_path,
            route_check,
        )],
    )
}

/// Validate the registry and either a named workspace or all registered workspaces.
pub fn validate_registry_with(
    registry_path: &Path,
    name: Option<&str>,
    strict: bool,
    route_check: Option<&RouteHealthCheck<'_>>,
) -> WorkspaceValidationReport {
    let registry = if registry_path.is_file() {
        WorkspaceRegistry::read_from_path(registry_path).ok()
    } else {
        None
    };
    let Some(registry) = registry else {
        return WorkspaceValidationReport::selection_error(
            registry_path,
            name,
            strict,
            "invalid_registry",
            "The workspace registry must be a readable TOML file with version = 1.",
            "Check the registry path and repair workspaces.toml.",
        );
    };

    if let Some(name) = name
        && !registry.workspaces.contains_key(name)
    {
        let (code, message) = if validate_workspace_name(name).is_err() {
            (
                "invalid_workspace_name",
                "Workspace names must be portable filename components.",
            )
        } else {
            (
                "invalid_workspace_config",
                "The selected workspace is not registered.",
            )
        };
        return WorkspaceValidationReport::selection_error(
            registry_path,
            Some(name),
            strict,
            code,
            message,
            "Choose a name from laine workspace list or register it with laine workspace add.",
        );
    }

    // The registry is a BTreeMap, so names and their corresponding entries are in order.
    let mut entries = Vec::new();
    for (workspace, path) in &registry.workspaces {
        if name.is_some_and(|name| name != workspace) {
            continue;
        }
        let mut entry = WorkspaceValidationEntry::new(path, Some(workspace));
        if validate_workspace_name(workspace).is_err() {
            entry.issue(
                Error,
                "invalid_workspace_name",
                None,
                Some("workspaces"),
                "Workspace names must be portable filename components.",
                "Use a non-empty name portable across operating systems.",
            );
        }
        if path.is_file() {
            entry = validate_config_entry(entry, path, route_check);
        } else {
            entry.issue(
                Error,
                "missing_workspace_config",
                None,
                Some("workspaces"),
                "The registered workspace config does not exist or is not a file.",
                "Update the registration to point to a laine.toml file.",
            );
        }
        entries.push(entry);
    }
    WorkspaceValidationReport::new(strict, entries.len(), entries)
}

// Load the config with the same rules as analysis and report syntax errors separately.
fn validate_config_entry(
    mut entry: WorkspaceValidationEntry,
    path: &Path,
    route_check: Option<&RouteHealthCheck<'_>>,
) -> WorkspaceValidationEntry {
    let input = path
        .is_file()
        .then(|| fs::read_to_string(path).ok())
        .flatten();
    let config = match input.as_deref().map(WorkspaceConfig::parse_toml) {
        Some(Ok(config)) => config,
        failure => {
            let (code, message) = match failure {
                None => ("invalid_workspace_config", "The workspace config must be a readable file.".to_owned()),
                Some(Err(LaineError::ConfigInvalid(message))) => ("invalid_workspace_config", message),
                _ if input.as_deref().is_some_and(|raw| toml::from_str::<toml::Table>(raw).is_err()) => (
                    "invalid_config_syntax",
                    "The workspace file contains invalid TOML syntax.".to_owned(),
                ),
                _ => (
                    "invalid_workspace_config",
                    "The workspace config has missing fields, invalid types, or an unsupported value (check policy.fail_on and routes.framework).".to_owned(),
                ),
            };
            entry.issue(
                Error,
                code,
                None,
                None,
                message,
                "Repair the workspace TOML file.",
            );
            return entry;
        }
    };

    let config_dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    entry.services_checked = config.services.len();
    if config.services.is_empty() {
        entry.issue(
            Warning,
            "empty_services",
            None,
            Some("services"),
            "The workspace does not list any services.",
            "Add at least one Git checkout with laine repo add.",
        );
    }
    validate_services(&mut entry, config_dir, &config.services, route_check);
    validate_clients_and_settings(&mut entry, config_dir, &config);
    entry
}

// Check service paths and URL ownership in name order for consistent duplicate diagnostics.
fn validate_services(
    entry: &mut WorkspaceValidationEntry,
    config_dir: &Path,
    services: &[ServiceConfig],
    route_check: Option<&RouteHealthCheck<'_>>,
) {
    let mut services: Vec<_> = services.iter().collect();
    services.sort_by_key(|service| &service.name);
    let mut roots: Vec<(PathBuf, &str)> = Vec::new();
    let mut claims = BTreeMap::new();
    for service in services {
        let name = Some(service.name.as_str());
        if service.name.trim().is_empty() {
            entry.issue(
                Error,
                "empty_service_name",
                name,
                Some("name"),
                "The service name is empty.",
                "Choose a non-empty, unique service name.",
            );
        }
        let path = workspace_service_path(config_dir, service);
        let git = if path.is_dir() {
            match GitRepo::discover(&path) {
                Ok(git) => Some(git),
                Err(_) => {
                    entry.issue(
                        Error,
                        "invalid_git_repository",
                        name,
                        Some("path"),
                        "The configured directory is not a Git checkout.",
                        "Set the service path to an accessible Git checkout.",
                    );
                    None
                }
            }
        } else {
            entry.issue(
                Error,
                "missing_service_path",
                name,
                Some("path"),
                "The configured service directory does not exist or is not a directory.",
                "Correct the service path or clone the repository.",
            );
            None
        };

        let root = git
            .as_ref()
            .map_or(path, |git| normalize_absolute_path(git.root()));
        if let Some((_, first)) = roots
            .iter()
            .find(|(other, _)| same_repository_path(other, &root))
        {
            entry.issue(
                Error,
                "duplicate_service_path",
                name,
                Some("path"),
                format!("The service shares a Git checkout with '{first}'."),
                "Keep one service entry per physical checkout.",
            );
        } else {
            roots.push((root, &service.name));
        }
        validate_service_urls(entry, service, &mut claims);
        validate_service_sources(entry, git.as_ref(), service, route_check);
    }
    for owners in claims.values().filter(|owners| owners.len() > 1) {
        let (first, index) = owners[0];
        let names: Vec<_> = owners.iter().map(|(name, _)| *name).collect();
        entry.issue(
            Error,
            "ambiguous_base_url",
            Some(first),
            Some(&format!("base_urls[{index}]")),
            format!(
                "Services {} claim the same normalized base URL.",
                names.join(", ")
            ),
            "Assign a distinct origin or path prefix to each service.",
        );
    }
}

// Normalize URL claims using the same matcher as consumer discovery without echoing URL values.
fn validate_service_urls<'a>(
    entry: &mut WorkspaceValidationEntry,
    service: &'a ServiceConfig,
    claims: &mut BTreeMap<String, Vec<(&'a str, usize)>>,
) {
    let name = Some(service.name.as_str());
    let mut seen = BTreeSet::new();
    for (index, url) in service.base_urls.iter().enumerate() {
        let field = format!("base_urls[{index}]");
        let Some(normalized) = matching_http_url(url) else {
            entry.issue(
                Error,
                "invalid_base_url",
                name,
                Some(&field),
                "The base URL is not a valid absolute HTTP(S) URL.",
                "Provide an http:// or https:// URL with a host.",
            );
            continue;
        };
        if has_http_credentials(url) || url.contains(['?', '#']) {
            entry.issue(
                Warning,
                "unsanitized_base_url",
                name,
                Some(&field),
                "The base URL contains credentials, a query, or a fragment.",
                "Keep only the scheme, host, optional port, and path.",
            );
        }
        let normalized = normalized.trim_end_matches('/');
        if seen.insert(normalized.to_owned()) {
            claims
                .entry(normalized.to_owned())
                .or_default()
                .push((&service.name, index));
        } else {
            entry.issue(
                Warning,
                "duplicate_base_url",
                name,
                Some(&field),
                "This base URL matches another entry for the same service.",
                "Keep one entry for each normalized base URL.",
            );
        }
    }
}

// Identify userinfo in the authority without treating @ in paths as credentials.
fn has_http_credentials(value: &str) -> bool {
    is_absolute_http_url(value)
        && value.split_once("://").is_some_and(|(_, rest)| {
            rest.split(['/', '?', '#'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        })
}

// Normalize a relative snapshot path while disallowing traversal above the checkout.
fn relative_snapshot_path(path: &Path) -> Option<String> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir if relative.pop() => {}
            _ => return None,
        }
    }
    (!relative.as_os_str().is_empty()).then(|| to_git_posix_path(&relative))
}

// Resolve HEAD once and prefer its files, falling back to disk for uncommitted files.
fn validate_service_sources(
    entry: &mut WorkspaceValidationEntry,
    git: Option<&GitRepo>,
    service: &ServiceConfig,
    route_check: Option<&RouteHealthCheck<'_>>,
) {
    let name = Some(service.name.as_str());
    let openapi = service.openapi.as_deref().and_then(|path| {
        let normalized = relative_snapshot_path(path);
        if normalized.is_none() {
            entry.issue(
                Error,
                "invalid_openapi_path",
                name,
                Some("openapi"),
                "The OpenAPI path must point to a file within the checkout.",
                "Use a repository-relative OpenAPI path.",
            );
        }
        normalized
    });
    let Some(git) = git else { return };
    let head = git.resolve_revision("HEAD").ok();
    if head.is_none() {
        entry.issue(
            Warning,
            "unborn_head_revision",
            name,
            Some("path"),
            "HEAD does not point to a commit.",
            "Create an initial commit or restore a valid HEAD revision.",
        );
    }
    if openapi.is_none() && service.routes.is_none() {
        return;
    }
    let head_files = match head.as_deref().map(|rev| git.list_files(rev)).transpose() {
        Ok(files) => files.unwrap_or_default(),
        Err(_) => {
            entry.issue(
                Error,
                "invalid_git_repository",
                name,
                Some("path"),
                "Git could not read the HEAD snapshot.",
                "Check the repository and restore the HEAD commit tree.",
            );
            return;
        }
    };
    let revision_for = |file: &str| {
        if head_files
            .binary_search_by(|candidate| candidate.as_str().cmp(file))
            .is_ok()
        {
            head.as_deref()
        } else if git.root().join(file).is_file() {
            Some(WORKTREE_REV)
        } else {
            None
        }
    };
    if let Some(file) = openapi.as_deref() {
        validate_openapi(entry, git, &service.name, file, revision_for(file));
    }
    if let Some(routes) = &service.routes {
        validate_routes(
            entry,
            git,
            &service.name,
            routes,
            head.as_deref(),
            revision_for,
            route_check,
        );
    }
}

// Read the spec and its references from one snapshot and keep locations for ref failures.
fn validate_openapi(
    entry: &mut WorkspaceValidationEntry,
    git: &GitRepo,
    service: &str,
    file: &str,
    revision: Option<&str>,
) {
    let name = Some(service);
    let Some(revision) = revision else {
        entry.issue(
            Error,
            "missing_openapi_file",
            name,
            Some("openapi"),
            format!("OpenAPI file '{file}' is missing from HEAD and the worktree."),
            "Fix the configured path or add the OpenAPI file to the repository.",
        );
        return;
    };
    if revision == WORKTREE_REV {
        entry.issue(
            Warning,
            "uncommitted_openapi_file",
            name,
            Some("openapi"),
            format!("OpenAPI file '{file}' exists only in the worktree."),
            "Commit the OpenAPI file so HEAD-based analyses can read it.",
        );
    }
    let extracted = OpenApiSnapshot::read(git, revision, file)
        .and_then(|snapshot| snapshot.extract(git, revision, service));
    match extracted {
        Ok(extracted) => {
            for (source, failure) in extracted.diagnostics {
                entry.issue(
                    Warning,
                    "openapi_reference_diagnostic",
                    name,
                    Some("openapi"),
                    format!(
                        "{} at {}#{}.",
                        failure.message(),
                        source.file,
                        source.pointer
                    ),
                    failure.remediation(),
                );
            }
        }
        Err(_) => entry.issue(
            Error,
            "invalid_openapi_document",
            name,
            Some("openapi"),
            format!("OpenAPI file '{file}' could not be read or extracted."),
            "Use a readable UTF-8 OpenAPI YAML or JSON document.",
        ),
    }
}

// Check explicit paths and Laravel defaults before asking the framework extractor for diagnostics.
fn validate_routes<'a>(
    entry: &mut WorkspaceValidationEntry,
    git: &GitRepo,
    service: &str,
    routes: &RouteSourceConfig,
    head: Option<&'a str>,
    revision_for: impl Fn(&str) -> Option<&'a str>,
    route_check: Option<&RouteHealthCheck<'_>>,
) {
    let name = Some(service);
    let uses_defaults = routes.files.is_empty() && routes.framework == RouteFramework::Laravel;
    let mut revisions = BTreeSet::new();
    for file in routes.snapshot_files() {
        match relative_snapshot_path(Path::new(&file))
            .as_deref()
            .and_then(&revision_for)
        {
            Some(revision) => {
                revisions.insert(revision);
                if revision == WORKTREE_REV && !uses_defaults {
                    entry.issue(
                        Warning,
                        "uncommitted_route_file",
                        name,
                        Some("routes.files"),
                        format!("Route file '{file}' exists only in the worktree."),
                        "Commit the route file so HEAD-based analyses can read it.",
                    );
                }
            }
            None if !uses_defaults => entry.issue(
                Error,
                "missing_route_file",
                name,
                Some("routes.files"),
                format!("Route file '{file}' is missing from HEAD and the worktree."),
                "Set routes.files to repository-relative files that exist.",
            ),
            None => {}
        }
    }
    if uses_defaults && revisions.is_empty() {
        entry.issue(
            Warning,
            "missing_default_laravel_routes",
            name,
            Some("routes.files"),
            "Neither routes/api.php nor routes/web.php exists.",
            "List the Laravel route files explicitly or add a conventional route file.",
        );
    } else if routes.files.is_empty() && !uses_defaults {
        revisions.insert(head.unwrap_or(WORKTREE_REV));
    }
    if let Some(check) = route_check {
        for revision in revisions {
            match check(git, service, revision, routes) {
                Ok(failures) => {
                    for (file, failure) in failures {
                        entry.issue(
                            Warning,
                            "incomplete_route_extraction",
                            name,
                            Some("routes"),
                            format!("{} in '{file}'.", failure.message()),
                            failure.remediation(),
                        );
                    }
                }
                Err(_) => entry.issue(
                    Warning,
                    "incomplete_route_extraction",
                    name,
                    Some("routes"),
                    "The route extractor could not read the repository snapshot.",
                    "Verify that Git and the configured route files are readable.",
                ),
            }
        }
    }
}

// Validate cross-service client mappings and workspace-wide settings.
fn validate_clients_and_settings(
    entry: &mut WorkspaceValidationEntry,
    config_dir: &Path,
    config: &WorkspaceConfig,
) {
    let mut mappings = BTreeSet::new();
    for (index, client) in config.clients.iter().enumerate() {
        let consumer = client.consumer_service.as_deref();
        let callee = client.callee.trim();
        let field = |name| format!("clients[{index}].{name}");
        if callee.is_empty() {
            entry.issue(
                Error,
                "empty_client_callee",
                consumer,
                Some(&field("callee")),
                "The client callee is empty.",
                "Specify the wrapper function or client object name.",
            );
        }
        if config.find_service(&client.target_service).is_none() {
            entry.issue(
                Error,
                "unknown_client_target",
                consumer,
                Some(&field("target_service")),
                "The client target does not match a configured service.",
                "Set target_service to a declared service name.",
            );
        }
        if consumer.is_some_and(|name| config.find_service(name).is_none()) {
            entry.issue(
                Error,
                "unknown_client_consumer",
                consumer,
                Some(&field("consumer_service")),
                "The client consumer does not match a configured service.",
                "Set consumer_service to a declared service name or omit it for a global mapping.",
            );
        }
        if !mappings.insert((consumer, client.callee.as_str())) {
            entry.issue(
                Warning,
                "duplicate_client_mapping",
                consumer,
                Some(&field("callee")),
                "The same callee is mapped more than once for this consumer scope.",
                "Keep one mapping per callee and consumer scope.",
            );
        }
        if client
            .path_prefix
            .as_deref()
            .is_some_and(|prefix| prefix.contains("://") || prefix.contains(['?', '#']))
        {
            entry.issue(
                Warning,
                "suspicious_client_prefix",
                consumer,
                Some(&field("path_prefix")),
                "The client path prefix contains a URL scheme, query, or fragment.",
                "Use only a path prefix such as /api.",
            );
        }
    }

    for (index, pattern) in config.policy.ignore_endpoints.iter().enumerate() {
        let field = format!("policy.ignore_endpoints[{index}]");
        let mut tokens = pattern.split_whitespace();
        match (tokens.next(), tokens.next(), tokens.next()) {
            (None, _, _) => entry.issue(
                Warning,
                "empty_ignore_pattern",
                None,
                Some(&field),
                "This endpoint ignore pattern is empty.",
                "Remove the empty entry.",
            ),
            (first, second, extra) => {
                if extra.is_none()
                    && second.is_some()
                    && first.and_then(HttpMethod::parse).is_none()
                {
                    entry.issue(
                        Error,
                        "invalid_ignore_pattern_method",
                        None,
                        Some(&field),
                        "This endpoint ignore pattern starts with an unknown HTTP method.",
                        "Use a supported HTTP method such as GET or omit the method.",
                    );
                }
                if extra.is_some() || !second.or(first).is_some_and(|path| path.starts_with('/')) {
                    entry.issue(
                        Error,
                        "invalid_ignore_pattern_path",
                        None,
                        Some(&field),
                        "This endpoint ignore pattern has an invalid path or extra tokens.",
                        "Use /path or METHOD /path.",
                    );
                }
            }
        }
    }
    for (key, value) in &config.env {
        if key.trim().is_empty() {
            entry.issue(
                Error,
                "empty_env_key",
                None,
                Some("env"),
                "An environment key is empty.",
                "Use the variable's non-empty name as its key.",
            );
        }
        if has_http_credentials(value) {
            entry.issue(
                Warning,
                "unsanitized_env_url",
                None,
                Some(&format!("env.{key}")),
                "The environment URL contains embedded credentials.",
                "Remove userinfo from the URL stored in the config.",
            );
        }
    }
    if config
        .cache_path
        .as_deref()
        .is_some_and(|path| config_dir.join(path).is_dir())
    {
        entry.issue(
            Error,
            "invalid_cache_path",
            None,
            Some("cache_path"),
            "The configured cache path points to a directory.",
            "Set cache_path to a SQLite file or omit it to use the default cache.",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Keep invalid syntax distinct from a TOML document with an invalid workspace schema.
    #[test]
    fn distinguishes_toml_and_config_errors() {
        let path =
            std::env::temp_dir().join(format!("laine-validation-{}.toml", std::process::id()));
        let cases = [
            ("version = [", "invalid_config_syntax"),
            ("version = 2", "invalid_workspace_config"),
            (
                "version = 1\n[policy]\nfail_on = 'medium-high'",
                "invalid_workspace_config",
            ),
            (
                "version = 1\n[[services]]\nname = 'one'\npath = '.'\n[services.routes]\nframework = 'unknown'",
                "invalid_workspace_config",
            ),
            (
                "version = 1\n[[services]]\nname = 'one'\npath = '.'\n[[services]]\nname = 'one'\npath = '../one'",
                "invalid_workspace_config",
            ),
        ];
        for (raw, code) in cases {
            fs::write(&path, raw).unwrap();
            let report = validate_workspace_config_with(&path, None, false, None);
            assert_eq!(report.exit_code, 1);
            assert_eq!(report.workspaces[0].issues[0].code, code);
        }
        fs::remove_file(path).unwrap();
    }

    // Ensure paths used for Git snapshots cannot escape the repository.
    #[test]
    fn normalizes_only_paths_inside_the_checkout() {
        for invalid in [
            "",
            ".",
            "../openapi.yaml",
            "spec/../../openapi.yaml",
            "/openapi.yaml",
        ] {
            assert!(
                relative_snapshot_path(Path::new(invalid)).is_none(),
                "{invalid}"
            );
        }
        assert_eq!(
            relative_snapshot_path(Path::new("./spec/../openapi.yaml")).as_deref(),
            Some("openapi.yaml")
        );
    }

    // Preserve issue severities and totals when strict mode changes the outcome.
    #[test]
    fn sorts_issues_and_applies_strictness() {
        let mut entry = WorkspaceValidationEntry::new(Path::new("laine.toml"), None);
        entry.issue(Warning, "z", None, None, "Last", "Fix it.");
        entry.issue(Warning, "a", None, None, "First", "Fix it.");
        for strict in [false, true] {
            let report = WorkspaceValidationReport::new(strict, 1, vec![entry.clone()]);
            let workspace = &report.workspaces[0];
            assert_eq!(workspace.issues[0].code, "a");
            assert_eq!(workspace.issues[0].severity, Warning);
            assert_eq!(report.summary.warnings, 2);
            assert_eq!(report.exit_code, u8::from(strict));
            assert_eq!(workspace.valid, !strict);
        }
    }
}
