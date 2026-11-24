use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{LaineError, Result};

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
    /// Optional explicit list of route files relative to the service repository root.
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
    pub fn load_from_path(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).map_err(|source| LaineError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse_toml(&raw)
    }

    pub fn parse_toml(raw: &str) -> Result<Self> {
        let cfg: WorkspaceConfig = toml::from_str(raw)?;
        if cfg.version != 1 {
            return Err(LaineError::ConfigInvalid(format!(
                "unsupported config version {} (expected 1)",
                cfg.version
            )));
        }
        Ok(cfg)
    }

    pub fn find_service(&self, name: &str) -> Option<&ServiceConfig> {
        self.services.iter().find(|svc| svc.name == name)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        let serialized = toml::to_string_pretty(self)
            .map_err(|err| LaineError::ConfigInvalid(err.to_string()))?;
        fs::write(path, serialized).map_err(|source| LaineError::Io {
            path: path.to_path_buf(),
            source,
        })
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
        assert_eq!(cfg.clients[0].callee, "getClient().fetch");
        assert_eq!(cfg.clients[0].target_service, "customer-service");
        assert_eq!(cfg.clients[0].path_prefix.as_deref(), Some("/api"));
    }
}
