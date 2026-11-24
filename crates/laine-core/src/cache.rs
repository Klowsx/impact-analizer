use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::config::{CustomClientConfig, WorkspaceConfig};
use crate::error::{LaineError, Result};

pub const CACHE_SCHEMA_VERSION: u32 = 1;

const FNV1A_64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Persistent SQLite cache for per-repository extracted consumer facts keyed by
/// `(repo, commit_sha, config_hash, schema_version)`.
pub struct IndexCache {
    conn: Connection,
}

impl IndexCache {
    /// Opens (or creates) the SQLite cache database at `path`, ensuring parent directories
    /// and the `repo_index_cache` table exist.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|source| LaineError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let conn = Connection::open(path).map_err(|err| LaineError::Cache(err.to_string()))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|err| LaineError::Cache(err.to_string()))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS repo_index_cache (
                repo TEXT NOT NULL,
                commit_sha TEXT NOT NULL,
                config_hash TEXT NOT NULL,
                schema_version INTEGER NOT NULL,
                payload_json TEXT NOT NULL,
                PRIMARY KEY (repo, commit_sha, config_hash, schema_version)
            );",
        )
        .map_err(|err| LaineError::Cache(err.to_string()))?;

        Ok(Self { conn })
    }

    /// Retrieves a cached JSON payload for `(repo, commit_sha, config_hash)` at the current
    /// `CACHE_SCHEMA_VERSION`, returning `Ok(None)` on cache miss.
    pub fn get_repo_snapshot(
        &self,
        repo: &str,
        commit_sha: &str,
        config_hash: &str,
    ) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT payload_json
                 FROM repo_index_cache
                 WHERE repo = ?1
                   AND commit_sha = ?2
                   AND config_hash = ?3
                   AND schema_version = ?4",
                params![repo, commit_sha, config_hash, CACHE_SCHEMA_VERSION],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|err| LaineError::Cache(err.to_string()))
    }

    /// Stores (or replaces) a cached JSON payload for `(repo, commit_sha, config_hash)` at the
    /// current `CACHE_SCHEMA_VERSION`.
    pub fn put_repo_snapshot(
        &self,
        repo: &str,
        commit_sha: &str,
        config_hash: &str,
        payload_json: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO repo_index_cache
                    (repo, commit_sha, config_hash, schema_version, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    repo,
                    commit_sha,
                    config_hash,
                    CACHE_SCHEMA_VERSION,
                    payload_json
                ],
            )
            .map_err(|err| LaineError::Cache(err.to_string()))?;
        Ok(())
    }
}

#[derive(Serialize)]
struct ConfigFingerprint<'a> {
    service_base_urls: BTreeMap<&'a str, &'a [String]>,
    clients: &'a [CustomClientConfig],
    env: &'a BTreeMap<String, String>,
}

/// Computes a deterministic 64-bit FNV-1a hex fingerprint over the extraction-relevant
/// configuration fields (`services.base_urls`, `clients`, and `env`).
///
/// Any change to service base URLs, custom clients, or environment variables produces a new
/// fingerprint and automatically invalidates cached repository extractions.
pub fn compute_config_hash(config: &WorkspaceConfig) -> String {
    let mut service_base_urls: BTreeMap<&str, &[String]> = BTreeMap::new();
    for service in &config.services {
        service_base_urls.insert(service.name.as_str(), service.base_urls.as_slice());
    }

    let fingerprint = ConfigFingerprint {
        service_base_urls,
        clients: &config.clients,
        env: &config.env,
    };

    let canonical_json = serde_json::to_string(&fingerprint).unwrap_or_default();

    let mut hash = FNV1A_64_OFFSET_BASIS;
    for byte in canonical_json.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV1A_64_PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PolicyConfig, ServiceConfig};
    use crate::model::Severity;

    fn sample_config() -> WorkspaceConfig {
        let mut env = BTreeMap::new();
        env.insert("API_URL".into(), "http://customer-service".into());

        WorkspaceConfig {
            version: 1,
            services: vec![ServiceConfig {
                name: "customer-service".into(),
                path: "./customer-service".into(),
                openapi: Some("openapi.yaml".into()),
                routes: None,
                base_urls: vec!["http://customer-service".into()],
            }],
            clients: vec![CustomClientConfig {
                callee: "apiClient".into(),
                target_service: "customer-service".into(),
                path_prefix: Some("/api".into()),
            }],
            env,
            policy: PolicyConfig::default(),
            cache_path: None,
        }
    }

    #[test]
    fn stores_retrieves_and_invalidates_by_sha_or_config_hash() {
        let tmp = std::env::temp_dir().join(format!("laine-cache-unit-{}.db", std::process::id()));
        let _ = fs::remove_file(&tmp);

        let cache = IndexCache::open(&tmp).expect("cache should open");
        assert_eq!(
            cache
                .get_repo_snapshot("billing-service", "sha111", "cfg1")
                .unwrap(),
            None
        );

        cache
            .put_repo_snapshot(
                "billing-service",
                "sha111",
                "cfg1",
                r#"{"files_analyzed":3}"#,
            )
            .unwrap();

        assert_eq!(
            cache
                .get_repo_snapshot("billing-service", "sha111", "cfg1")
                .unwrap(),
            Some(r#"{"files_analyzed":3}"#.to_string())
        );

        // Different commit SHA -> cache miss
        assert_eq!(
            cache
                .get_repo_snapshot("billing-service", "sha222", "cfg1")
                .unwrap(),
            None
        );

        // Different config hash -> cache miss
        assert_eq!(
            cache
                .get_repo_snapshot("billing-service", "sha111", "cfg2")
                .unwrap(),
            None
        );

        // Different schema version row is isolated from current version
        cache
            .conn
            .execute(
                "INSERT INTO repo_index_cache
                    (repo, commit_sha, config_hash, schema_version, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "billing-service",
                    "sha999",
                    "cfg1",
                    CACHE_SCHEMA_VERSION + 1,
                    "{}"
                ],
            )
            .unwrap();
        assert_eq!(
            cache
                .get_repo_snapshot("billing-service", "sha999", "cfg1")
                .unwrap(),
            None
        );

        drop(cache);
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn config_hash_changes_on_base_urls_clients_env_but_not_policy() {
        let base = sample_config();
        let h_base = compute_config_hash(&base);
        assert_eq!(h_base, compute_config_hash(&base));

        // Changing policy does NOT change extraction hash
        let mut policy_changed = base.clone();
        policy_changed.policy.fail_on = Severity::Critical;
        policy_changed
            .policy
            .ignore_endpoints
            .push("/health".into());
        assert_eq!(h_base, compute_config_hash(&policy_changed));

        // Changing base_urls invalidates hash
        let mut url_changed = base.clone();
        url_changed.services[0]
            .base_urls
            .push("https://api.example.com".into());
        assert_ne!(h_base, compute_config_hash(&url_changed));

        // Changing clients invalidates hash
        let mut client_changed = base.clone();
        client_changed.clients[0].path_prefix = Some("/v2".into());
        assert_ne!(h_base, compute_config_hash(&client_changed));

        // Changing env invalidates hash
        let mut env_changed = base;
        env_changed
            .env
            .insert("API_URL".into(), "https://prod.internal".into());
        assert_ne!(h_base, compute_config_hash(&env_changed));
    }
}
