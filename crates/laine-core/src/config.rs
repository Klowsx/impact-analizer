use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{LaineError, Result};
use crate::model::Severity;
pub use crate::url::{
    ServiceEndpointMatch, is_absolute_http_url, matching_http_url, normalize_relative_path,
    resolve_http_service_url,
};

// Adquiere un bloqueo de escritura compartido por todos los procesos que actualizan un TOML.
pub fn lock_config_update(path: &Path) -> Result<File> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let parent = config_parent(&path);
    let Some(name) = path.file_name() else {
        return Err(LaineError::ConfigInvalid(
            "expected a TOML file path".into(),
        ));
    };
    let mut lock_name = name.to_os_string();
    lock_name.push(".lock");
    let lock_path = parent.join(lock_name);
    let io_error = |source| LaineError::Io {
        path: lock_path.clone(),
        source,
    };
    fs::create_dir_all(parent).map_err(&io_error)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(&io_error)?;
    file.lock().map_err(io_error)?;
    Ok(file)
}

// Escribe un archivo temporal junto al destino y lo renombra para evitar TOML parciales.
pub fn write_config_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let parent = config_parent(&path);
    let io_error = |source| LaineError::Io {
        path: path.clone(),
        source,
    };
    fs::create_dir_all(parent).map_err(&io_error)?;
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_error(error)),
    };
    let (temporary_path, mut file) = loop {
        let seq = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(".laine-{}-{seq}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    };
    let write = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        if let Some(metadata) = metadata {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    let result = write.and_then(|_| fs::rename(&temporary_path, &path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result.map_err(io_error)
}

// Trata los nombres relativos sin directorio padre como archivos del directorio actual.
fn config_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn default_fail_on() -> Severity {
    Severity::Low
}

fn deserialize_severity<'de, D>(deserializer: D) -> std::result::Result<Severity, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => Ok(Severity::Low),
        "medium" => Ok(Severity::Medium),
        "high" => Ok(Severity::High),
        "critical" => Ok(Severity::Critical),
        other => Err(serde::de::Error::custom(format!(
            "invalid severity '{other}' (expected one of: low, medium, high, critical)"
        ))),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyConfig {
    /// Minimum finding severity that causes `laine analyze` to fail (exit code 2).
    #[serde(default = "default_fail_on", deserialize_with = "deserialize_severity")]
    pub fail_on: Severity,
    /// Normalized paths or canonical keys to ignore (e.g. `"/health"`, `"/internal/*"`).
    #[serde(default)]
    pub ignore_endpoints: Vec<String>,
    /// When true, any unresolved (`UNKNOWN`) HTTP call also triggers a policy failure.
    #[serde(default)]
    pub fail_on_unresolved: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            fail_on: Severity::Low,
            ignore_endpoints: Vec::new(),
            fail_on_unresolved: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub version: u32,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
    /// Optional custom HTTP client wrappers (e.g. `getClient().fetch`, `$fetch`, `apiClient`).
    #[serde(default)]
    pub clients: Vec<CustomClientConfig>,
    /// Optional environment variable values for resolving `process.env.X` / `import.meta.env.X`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Policy rules controlling ignored endpoints and failure thresholds.
    #[serde(default)]
    pub policy: PolicyConfig,
    /// Optional path to the SQLite incremental extraction cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteFramework {
    Express,
    Laravel,
    FastApi,
    Flask,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteSourceConfig {
    pub framework: RouteFramework,
    /// Optional list of files that contribute route operations, relative to the service root.
    /// Express and FastAPI also scan other source files in the revision for router mounts.
    /// A listed router without a mount chain or a listed mount exposing an unlisted operation
    /// makes route extraction incomplete.
    #[serde(default)]
    pub files: Vec<PathBuf>,
    /// Optional mount prefix prepended to extracted routes (e.g. `"/api"`).
    #[serde(default)]
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub openapi: Option<PathBuf>,
    #[serde(default)]
    pub routes: Option<RouteSourceConfig>,
    #[serde(default)]
    pub base_urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomClientConfig {
    /// Restrict this mapping to a service name. Omit it for a workspace-wide fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_service: Option<String>,
    /// Callee pattern to recognize:
    /// - Exact call such as `getClient().fetch`, `$fetch`, `fetchApi`
    /// - Or an HTTP client object such as `apiClient` (matches `.get/.post/.put/.patch/.delete/.fetch`)
    pub callee: String,
    /// Service name from `[[services]]` targeted when called with a relative path like `'/corp/listBooking'`.
    pub target_service: String,
    /// Optional path prefix prepended to relative paths (e.g. `"/api"`).
    #[serde(default)]
    pub path_prefix: Option<String>,
}

impl WorkspaceConfig {
    /// Select a consumer's mappings, giving its own entries precedence over global entries.
    pub fn clients_for_consumer(&self, consumer: &str) -> Vec<CustomClientConfig> {
        let scoped: Vec<_> = self
            .clients
            .iter()
            .filter(|client| client.consumer_service.as_deref() == Some(consumer))
            .collect();
        let global = self.clients.iter().filter(|client| {
            client.consumer_service.is_none()
                && !scoped.iter().any(|entry| entry.callee == client.callee)
        });
        scoped.iter().copied().chain(global).cloned().collect()
    }
}

// Sanea cualquier URL HTTP(S) al reescribir el TOML y conserva ajustes de otros tipos.
fn sanitize_config_value(value: &str) -> String {
    if is_absolute_http_url(value) {
        return matching_http_url(value).unwrap_or_default();
    }
    value.to_owned()
}

impl WorkspaceConfig {
    pub fn load_from_path(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).map_err(|source| LaineError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse_toml(&raw)
    }

    // Valida la versión y que cada nombre identifique un único servicio.
    pub fn parse_toml(raw: &str) -> Result<Self> {
        let cfg: WorkspaceConfig = toml::from_str(raw)?;
        if cfg.version != 1 {
            return Err(LaineError::ConfigInvalid(format!(
                "unsupported config version {} (expected 1)",
                cfg.version
            )));
        }
        let mut names = BTreeSet::new();
        for service in &cfg.services {
            if !names.insert(&service.name) {
                return Err(LaineError::ConfigInvalid(format!(
                    "duplicate service name '{}'",
                    service.name
                )));
            }
        }
        Ok(cfg)
    }

    pub fn find_service(&self, name: &str) -> Option<&ServiceConfig> {
        self.services.iter().find(|svc| svc.name == name)
    }

    // Guarda de forma atómica una copia saneada de la configuración del workspace.
    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        let mut config = self.clone();
        for value in config.env.values_mut().chain(
            config
                .services
                .iter_mut()
                .flat_map(|service| service.base_urls.iter_mut()),
        ) {
            *value = sanitize_config_value(value);
        }
        let serialized = toml::to_string_pretty(&config)
            .map_err(|err| LaineError::ConfigInvalid(err.to_string()))?;
        write_config_atomically(path, serialized.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_workspace_config() {
        let raw = r#"
version = 1

[[services]]
name = "customer-service"
path = "./customer-service"
openapi = "openapi.yaml"
base_urls = ["http://customer-service"]

[[services]]
name = "billing-service"
path = "./billing-service"
"#;
        let cfg = WorkspaceConfig::parse_toml(raw).expect("valid config should parse");
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.services.len(), 2);
        let customer = cfg.find_service("customer-service").unwrap();
        assert_eq!(customer.base_urls, vec!["http://customer-service"]);
        assert_eq!(customer.openapi, Some(PathBuf::from("openapi.yaml")));
    }

    #[test]
    fn rejects_unsupported_version() {
        let raw = "version = 99\n";
        let err = WorkspaceConfig::parse_toml(raw).unwrap_err();
        assert!(matches!(err, LaineError::ConfigInvalid(_)));
    }

    // Keep existing TOML entries global when the consumer field is omitted.
    #[test]
    fn parses_custom_clients_and_env_mappings() {
        let raw = r#"
version = 1

[env]
VITE_API_URL = "http://customer-service"

[[services]]
name = "customer-service"
path = "./customer-service"
base_urls = ["http://customer-service"]

[[clients]]
callee = "getClient().fetch"
target_service = "customer-service"
path_prefix = "/api"
"#;
        let cfg = WorkspaceConfig::parse_toml(raw).expect("extended config should parse");
        assert_eq!(
            cfg.env.get("VITE_API_URL").map(String::as_str),
            Some("http://customer-service")
        );
        assert_eq!(cfg.clients.len(), 1);
        assert_eq!(cfg.clients[0].consumer_service, None);
        assert_eq!(cfg.clients[0].callee, "getClient().fetch");
        assert_eq!(cfg.clients[0].target_service, "customer-service");
        assert_eq!(cfg.clients[0].path_prefix.as_deref(), Some("/api"));
        assert_eq!(cfg.clients_for_consumer("legacy-portal"), cfg.clients);
    }

    // Resolve a scoped mapping first and retain the legacy default for other consumers.
    #[test]
    fn scoped_clients_override_global_clients_for_one_consumer() {
        let cfg = WorkspaceConfig::parse_toml(
            r#"
version = 1

[[clients]]
callee = "apiClient"
target_service = "billing"

[[clients]]
callee = "apiClient"
consumer_service = "web-b"
target_service = "users"
"#,
        )
        .unwrap();
        assert_eq!(cfg.clients_for_consumer("web-a"), cfg.clients[..1]);
        assert_eq!(cfg.clients_for_consumer("web-b"), cfg.clients[1..]);
    }

    #[test]
    fn parses_policy_and_cache_path_with_case_insensitive_severity() {
        let raw = r#"
version = 1
cache_path = ".cache/laine.sqlite"

[policy]
fail_on = "critical"
ignore_endpoints = ["/health", "/internal/*"]
fail_on_unresolved = true
"#;
        let cfg = WorkspaceConfig::parse_toml(raw).expect("policy config should parse");
        assert_eq!(cfg.cache_path, Some(PathBuf::from(".cache/laine.sqlite")));
        assert_eq!(cfg.policy.fail_on, Severity::Critical);
        assert_eq!(
            cfg.policy.ignore_endpoints,
            vec!["/health".to_string(), "/internal/*".to_string()]
        );
        assert!(cfg.policy.fail_on_unresolved);

        // Defaults preserve backwards compatibility when `[policy]` is omitted.
        let minimal = WorkspaceConfig::parse_toml("version = 1\n").unwrap();
        assert_eq!(minimal.policy, PolicyConfig::default());
        assert_eq!(minimal.policy.fail_on, Severity::Low);
        assert!(!minimal.policy.fail_on_unresolved);
        assert!(minimal.cache_path.is_none());
    }
}
