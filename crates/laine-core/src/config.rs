use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{LaineError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub version: u32,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub openapi: Option<PathBuf>,
    #[serde(default)]
    pub base_urls: Vec<String>,
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
}
