use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;

use crate::config::{CustomClientConfig, WorkspaceConfig};
use crate::error::{LaineError, Result};

// Version both the repository and per-blob extraction payloads.
pub const CACHE_SCHEMA_VERSION: u32 = 17;
const PHYSICAL_SCHEMA_VERSION: u32 = 2;

const FNV1A_64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Persistent SQLite cache for per-repository extracted consumer facts keyed by
/// `(repo, commit_sha, config_hash, schema_version)`.
pub struct IndexCache {
    conn: Connection,
    retention: RetentionPolicy,
}

/// Bounds derived cache rows that this version of Laine can maintain.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    pub max_age_secs: u32,
    pub max_repo_snapshots_per_bucket: u32,
    pub max_total_repo_snapshots: u32,
    pub max_total_blob_entries: u32,
}

impl Default for RetentionPolicy {
    /// Keeps recent snapshots and reusable blob facts within bounded storage.
    fn default() -> Self {
        Self {
            max_age_secs: 30 * 86_400,
            max_repo_snapshots_per_bucket: 16,
            max_total_repo_snapshots: 256,
            max_total_blob_entries: 20_000,
        }
    }
}

/// Identifies an extraction stage and the exact inputs that affect its output.
#[derive(Clone, Copy)]
pub struct BlobCacheKey<'a> {
    pub stage: &'a str,
    pub file_path: &'a str,
    pub blob_id: &'a str,
    pub deps_hash: &'a str,
}

impl IndexCache {
    /// Opens the cache with the default retention limits.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_retention(path, RetentionPolicy::default())
    }

    /// Migrates the database and prunes old entries under the supplied limits.
    pub fn open_with_retention(path: &Path, retention: RetentionPolicy) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|source| LaineError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let conn = Connection::open(path).map_err(cache_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(cache_error)?;
        conn.pragma_update(None, "secure_delete", true)
            .map_err(cache_error)?;
        initialize_schema(&conn)?;
        if conn
            .pragma_query_value(None, "auto_vacuum", |row| row.get::<_, u8>(0))
            .map_err(cache_error)?
            != 2
        {
            // SQLite needs VACUUM to add pointer maps to an existing database.
            conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
                .map_err(cache_error)?;
            conn.execute_batch("VACUUM").map_err(cache_error)?;
        }
        let cache = Self { conn, retention };
        cache.prune().map_err(cache_error)?;
        Ok(cache)
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
                "UPDATE repo_index_cache SET last_accessed_at = MAX(
                     CAST(unixepoch('subsec') * 1000 AS INTEGER),
                     (SELECT COALESCE(MAX(last_accessed_at), 0) + 1 FROM repo_index_cache))
                 WHERE repo = ?1
                   AND commit_sha = ?2
                   AND config_hash = ?3
                   AND schema_version = ?4
                 RETURNING payload_json",
                params![repo, commit_sha, config_hash, CACHE_SCHEMA_VERSION],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(cache_error)
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
        let tx = self.write_transaction().map_err(cache_error)?;
        let timestamp = next_access(&tx, "repo_index_cache").map_err(cache_error)?;
        tx.execute(
            "INSERT OR REPLACE INTO repo_index_cache
                    (repo, commit_sha, config_hash, schema_version, payload_json, last_accessed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                repo,
                commit_sha,
                config_hash,
                CACHE_SCHEMA_VERSION,
                payload_json,
                timestamp,
            ],
        )
        .map_err(cache_error)?;
        self.finish_write(tx).map_err(cache_error)
    }

    /// Gets one result per key and refreshes all hit timestamps in one transaction.
    pub fn get_blob_entries_batch(
        &self,
        repo: &str,
        config_hash: &str,
        keys: &[BlobCacheKey<'_>],
    ) -> Result<Vec<Option<String>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self.write_transaction().map_err(cache_error)?;
        let timestamp = next_access(&tx, "blob_extraction_cache").map_err(cache_error)?;
        let entries = {
            let mut stmt = tx
                .prepare_cached(
                    "UPDATE blob_extraction_cache SET last_accessed_at = ?8
                 WHERE repo = ?1 AND stage = ?2 AND file_path = ?3 AND blob_id = ?4
                   AND config_hash = ?5 AND deps_hash = ?6 AND schema_version = ?7
                 RETURNING payload_json",
                )
                .map_err(cache_error)?;
            keys.iter()
                .map(|key| {
                    stmt.query_row(
                        params![
                            repo,
                            key.stage,
                            key.file_path,
                            key.blob_id,
                            config_hash,
                            key.deps_hash,
                            CACHE_SCHEMA_VERSION,
                            timestamp
                        ],
                        |row| row.get(0),
                    )
                    .optional()
                })
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(cache_error)?
        };
        tx.commit().map_err(cache_error)?;
        Ok(entries)
    }

    /// Stores a batch of sanitized extraction payloads and enforces retention.
    pub fn put_blob_entries_batch(
        &self,
        repo: &str,
        config_hash: &str,
        entries: &[(BlobCacheKey<'_>, String)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let tx = self.write_transaction().map_err(cache_error)?;
        let timestamp = next_access(&tx, "blob_extraction_cache").map_err(cache_error)?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO blob_extraction_cache
                    (repo, stage, file_path, blob_id, config_hash, deps_hash, schema_version,
                     last_accessed_at, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .map_err(cache_error)?;
            for (key, payload) in entries {
                stmt.execute(params![
                    repo,
                    key.stage,
                    key.file_path,
                    key.blob_id,
                    config_hash,
                    key.deps_hash,
                    CACHE_SCHEMA_VERSION,
                    timestamp,
                    payload
                ])
                .map_err(cache_error)?;
            }
        }
        self.finish_write(tx).map_err(cache_error)
    }

    /// Acquires the writer lock before computing recency and updating rows.
    fn write_transaction(&self) -> rusqlite::Result<Transaction<'_>> {
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
    }

    /// Applies retention on open, including when the configured limits change.
    fn prune(&self) -> rusqlite::Result<()> {
        self.finish_write(self.write_transaction()?)
    }

    /// Commits the write and reclaims a bounded number of pages after eviction.
    fn finish_write(&self, tx: Transaction<'_>) -> rusqlite::Result<()> {
        let evicted = prune_entries(&tx, self.retention)?;
        tx.commit()?;
        if evicted {
            self.conn.execute_batch("PRAGMA incremental_vacuum(64)")?;
        }
        Ok(())
    }
}

// Converts database diagnostics to the public cache error.
fn cache_error(err: rusqlite::Error) -> LaineError {
    LaineError::Cache(err.to_string())
}

// Allocates a timestamp strictly newer than every entry, including hits in the same millisecond.
fn next_access(conn: &Connection, table: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        &format!(
            "SELECT MAX(CAST(unixepoch('subsec') * 1000 AS INTEGER),
                     COALESCE(MAX(last_accessed_at), 0) + 1) FROM {table}"
        ),
        [],
        |row| row.get(0),
    )
}

// Rebuilds incompatible derived tables in one transaction, preserving external tables.
fn initialize_schema(conn: &Connection) -> Result<()> {
    let tx =
        Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(cache_error)?;
    let version_table_exists = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'laine_cache_schema' AND type = 'table')",
        [],
        |row| row.get::<_, bool>(0),
    ).map_err(cache_error)?;
    let version = if version_table_exists {
        tx.query_row(
            "SELECT version FROM laine_cache_schema WHERE id = 1",
            [],
            |row| row.get::<_, u32>(0),
        )
        .optional()
        .map_err(cache_error)?
    } else {
        None
    };
    if version.is_some_and(|version| version > PHYSICAL_SCHEMA_VERSION) {
        return Err(LaineError::Cache(
            "database uses a newer Laine cache schema".into(),
        ));
    }
    tx.pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .map_err(cache_error)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS laine_cache_schema (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            version INTEGER NOT NULL
        );",
    )
    .map_err(cache_error)?;

    let compatible = version == Some(PHYSICAL_SCHEMA_VERSION)
        && [
            "INSERT INTO repo_index_cache (repo, commit_sha, config_hash, schema_version,
                                           payload_json, created_at, last_accessed_at)
             VALUES (NULL, NULL, NULL, NULL, NULL, NULL, NULL)
             ON CONFLICT (repo, commit_sha, config_hash, schema_version) DO NOTHING",
            "INSERT INTO blob_extraction_cache (repo, stage, file_path, blob_id, config_hash,
                                                deps_hash, schema_version, payload_json, created_at, last_accessed_at)
             VALUES (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)
             ON CONFLICT (repo, stage, file_path, blob_id, config_hash, deps_hash, schema_version) DO NOTHING",
            "INSERT INTO cache_meta (key, value) VALUES (NULL, NULL) ON CONFLICT (key) DO NOTHING",
        ]
        .iter()
        .all(|sql| tx.prepare(sql).is_ok());
    if !compatible {
        let future_tables = [
            ("repo_index_cache", "repo, commit_sha, config_hash, schema_version, payload_json"),
            ("blob_extraction_cache", "repo, stage, file_path, blob_id, config_hash, deps_hash, schema_version, payload_json"),
        ].into_iter().filter_map(|(table, columns)| {
            save_future_rows(&tx, table, columns).transpose()
        }).collect::<rusqlite::Result<Vec<_>>>().map_err(cache_error)?;
        tx.execute_batch(
            "DROP TABLE IF EXISTS repo_index_cache;
             DROP TABLE IF EXISTS blob_extraction_cache;
             DROP TABLE IF EXISTS cache_meta;
             CREATE TABLE repo_index_cache (
                repo TEXT NOT NULL,
                commit_sha TEXT NOT NULL,
                config_hash TEXT NOT NULL,
                schema_version INTEGER NOT NULL,
                payload_json TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                last_accessed_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (repo, commit_sha, config_hash, schema_version)
             );
             CREATE TABLE blob_extraction_cache (
                repo TEXT NOT NULL,
                stage TEXT NOT NULL,
                file_path TEXT NOT NULL,
                blob_id TEXT NOT NULL,
                config_hash TEXT NOT NULL,
                deps_hash TEXT NOT NULL,
                schema_version INTEGER NOT NULL,
                payload_json TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                last_accessed_at INTEGER NOT NULL,
                PRIMARY KEY (repo, stage, file_path, blob_id, config_hash, deps_hash, schema_version)
             );
             CREATE TABLE cache_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        ).map_err(cache_error)?;
        for (table, columns) in future_tables {
            tx.execute_batch(&format!(
                "INSERT OR REPLACE INTO {table} ({columns}, created_at, last_accessed_at)
                 SELECT * FROM temp.future_{table};
                 DROP TABLE temp.future_{table};"
            ))
            .map_err(cache_error)?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO laine_cache_schema (id, version) VALUES (1, ?1)",
            [PHYSICAL_SCHEMA_VERSION],
        )
        .map_err(cache_error)?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS repo_index_cache_bucket_lru
             ON repo_index_cache (repo, config_hash, last_accessed_at);
         CREATE INDEX IF NOT EXISTS repo_index_cache_lru ON repo_index_cache (last_accessed_at);
         CREATE INDEX IF NOT EXISTS blob_extraction_cache_lru ON blob_extraction_cache (last_accessed_at);
         INSERT OR IGNORE INTO cache_meta (key, value) VALUES ('created_at', CURRENT_TIMESTAMP);",
    ).map_err(cache_error)?;
    tx.execute(
        "INSERT OR REPLACE INTO cache_meta (key, value) VALUES
         ('physical_schema_version', ?1), ('logical_schema_version', ?2), ('laine_version', ?3)",
        params![
            PHYSICAL_SCHEMA_VERSION,
            CACHE_SCHEMA_VERSION,
            env!("CARGO_PKG_VERSION")
        ],
    )
    .map_err(cache_error)?;
    tx.commit().map_err(cache_error)
}

// Copies identifiable future rows before rebuilding a legacy or damaged cache table.
fn save_future_rows<'a>(
    tx: &Transaction<'_>,
    table: &'a str,
    columns: &'a str,
) -> rusqlite::Result<Option<(&'a str, &'a str)>> {
    if tx
        .prepare(&format!("SELECT {columns} FROM {table}"))
        .is_err()
    {
        return Ok(None);
    }
    let access = if tx
        .prepare(&format!("SELECT last_accessed_at FROM {table}"))
        .is_ok()
    {
        "COALESCE(last_accessed_at, 0)"
    } else {
        "0"
    };
    let created = if tx
        .prepare(&format!("SELECT created_at FROM {table}"))
        .is_ok()
    {
        "COALESCE(created_at, CURRENT_TIMESTAMP)"
    } else {
        "CURRENT_TIMESTAMP"
    };
    tx.execute(
        &format!(
            "CREATE TEMP TABLE future_{table} AS
                  SELECT {columns}, {created}, {access} FROM {table} WHERE schema_version > ?1"
        ),
        [CACHE_SCHEMA_VERSION],
    )?;
    Ok(Some((table, columns)))
}

// Removes stale and least-recently-used rows while preserving newer logical versions.
fn prune_entries(tx: &Transaction<'_>, retention: RetentionPolicy) -> rusqlite::Result<bool> {
    let mut deleted = 0;
    for (table, limit) in [
        ("repo_index_cache", retention.max_total_repo_snapshots),
        ("blob_extraction_cache", retention.max_total_blob_entries),
    ] {
        deleted += tx.execute(
            &format!(
                "DELETE FROM {table} WHERE schema_version < ?1 OR
                     (schema_version = ?1 AND created_at < datetime('now', ?2))"
            ),
            params![
                CACHE_SCHEMA_VERSION,
                format!("-{} seconds", retention.max_age_secs)
            ],
        )?;
        if table == "repo_index_cache" {
            deleted += tx.execute(
                "DELETE FROM repo_index_cache WHERE rowid IN (
                    SELECT rowid FROM (
                        SELECT rowid, ROW_NUMBER() OVER (
                            PARTITION BY repo, config_hash ORDER BY last_accessed_at DESC, rowid DESC
                        ) AS lru_rank FROM repo_index_cache WHERE schema_version = ?1
                    ) WHERE lru_rank > ?2
                )",
                params![CACHE_SCHEMA_VERSION, retention.max_repo_snapshots_per_bucket],
            )?;
        }
        deleted += tx.execute(
            &format!(
                "DELETE FROM {table} WHERE rowid IN (
                         SELECT rowid FROM {table} WHERE schema_version = ?1
                         ORDER BY last_accessed_at DESC, rowid DESC LIMIT -1 OFFSET ?2
                     )"
            ),
            params![CACHE_SCHEMA_VERSION, limit],
        )?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO cache_meta (key, value)
         VALUES ('last_pruned_at', CURRENT_TIMESTAMP)",
        [],
    )?;
    Ok(deleted > 0)
}

#[derive(Serialize)]
struct ConfigFingerprint<'a> {
    service_base_urls: BTreeMap<&'a str, &'a [String]>,
    clients: &'a [CustomClientConfig],
    env: &'a BTreeMap<String, String>,
}

/// Calcula la huella de las opciones que pueden cambiar la extracción.
/// Uses a deterministic 64-bit FNV-1a hex fingerprint over the extraction-relevant
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
    stable_fingerprint(canonical_json.as_bytes())
}

/// Generates a deterministic hexadecimal identifier from bytes.
pub fn stable_fingerprint(bytes: &[u8]) -> String {
    let mut hash = FNV1A_64_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV1A_64_PRIME);
    }
    format!("{hash:016x}")
}

/// Fingerprints a structured value whose JSON representation has deterministic ordering.
pub fn fingerprint_json(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|err| LaineError::Cache(err.to_string()))?;
    Ok(stable_fingerprint(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PolicyConfig, ServiceConfig};
    use crate::model::Severity;

    // Rebuilds the legacy schema, preserves external tables, and expires snapshots.
    #[test]
    fn rebuilds_legacy_schema_and_purges_expired_snapshots() {
        let tmp =
            std::env::temp_dir().join(format!("laine-cache-migration-{}.db", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let original = Connection::open(&tmp).unwrap();
        original
            .execute_batch(
                "CREATE TABLE laine_cache_schema (id INTEGER PRIMARY KEY, version INTEGER NOT NULL);
            INSERT INTO laine_cache_schema VALUES (1, 1);
            CREATE TABLE repo_index_cache (
                repo TEXT, commit_sha TEXT, config_hash TEXT,
                schema_version INTEGER, payload_json TEXT, created_at TEXT
            );
            INSERT INTO repo_index_cache VALUES ('repo', 'legacy', 'config', 16, 'old snapshot', CURRENT_TIMESTAMP);
            CREATE TABLE external_data (value TEXT);
            INSERT INTO external_data VALUES ('keep');",
            )
            .unwrap();
        drop(original);

        let cache = IndexCache::open(&tmp).unwrap();
        let metadata = cache
            .conn
            .prepare("SELECT key, value FROM cache_meta ORDER BY key")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()
            .unwrap();
        assert_eq!(metadata.len(), 5);
        assert_eq!(metadata["physical_schema_version"], "2");
        assert_eq!(metadata["logical_schema_version"], "17");
        assert_eq!(metadata["laine_version"], env!("CARGO_PKG_VERSION"));
        assert!(!metadata["created_at"].is_empty());
        assert!(!metadata["last_pruned_at"].is_empty());
        assert_eq!(
            cache
                .conn
                .pragma_query_value(None, "auto_vacuum", |row| row.get::<_, u8>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            cache.get_repo_snapshot("repo", "legacy", "config").unwrap(),
            None
        );
        cache
            .put_repo_snapshot("repo", "old", "config", "{}")
            .unwrap();
        cache
            .put_repo_snapshot("repo", "new", "config", "{}")
            .unwrap();
        cache.conn.execute("UPDATE repo_index_cache SET created_at = datetime('now', '-31 days') WHERE commit_sha = 'old'", []).unwrap();
        drop(cache);

        let cache = IndexCache::open(&tmp).unwrap();
        assert_eq!(
            cache.get_repo_snapshot("repo", "old", "config").unwrap(),
            None
        );
        assert_eq!(
            cache
                .get_repo_snapshot("repo", "new", "config")
                .unwrap()
                .as_deref(),
            Some("{}")
        );
        let external: String = cache
            .conn
            .query_row("SELECT value FROM external_data", [], |row| row.get(0))
            .unwrap();
        assert_eq!(external, "keep");
        drop(cache);
        fs::remove_file(&tmp).unwrap();
    }

    // Verifies that touching rows changes eviction order and that old blobs expire on open.
    #[test]
    fn bounds_snapshot_and_blob_retention_by_age_and_recency() {
        let tmp =
            std::env::temp_dir().join(format!("laine-cache-retention-{}.db", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let policy = RetentionPolicy {
            max_age_secs: 60,
            max_repo_snapshots_per_bucket: 2,
            max_total_repo_snapshots: 3,
            max_total_blob_entries: 2,
        };
        let cache = IndexCache::open_with_retention(&tmp, policy).unwrap();
        for revision in ["a", "b"] {
            cache
                .put_repo_snapshot("repo", revision, "config", "{}")
                .unwrap();
        }
        assert_eq!(
            cache
                .get_repo_snapshot("repo", "a", "config")
                .unwrap()
                .as_deref(),
            Some("{}")
        );
        cache
            .put_repo_snapshot("repo", "c", "config", "{}")
            .unwrap();
        assert!(
            cache
                .get_repo_snapshot("repo", "b", "config")
                .unwrap()
                .is_none()
        );
        cache
            .put_repo_snapshot("repo2", "d", "config", "{}")
            .unwrap();
        assert!(
            cache
                .get_repo_snapshot("repo", "a", "config")
                .unwrap()
                .is_some()
        );
        cache
            .put_repo_snapshot("repo", "e", "other config", "{}")
            .unwrap();
        assert!(
            cache
                .get_repo_snapshot("repo", "c", "config")
                .unwrap()
                .is_none()
        );
        assert!(
            cache
                .get_repo_snapshot("repo2", "d", "config")
                .unwrap()
                .is_some()
        );

        let key = |path| BlobCacheKey {
            stage: "ts_l1",
            file_path: path,
            blob_id: "oid",
            deps_hash: "",
        };
        let a = key("a.ts");
        let b = key("b.ts");
        let c = key("c.ts");
        cache
            .put_blob_entries_batch("repo", "config", &[(a, "{}".into()), (b, "{}".into())])
            .unwrap();
        assert_eq!(
            cache
                .get_blob_entries_batch("repo", "config", &[a])
                .unwrap(),
            [Some("{}".into())]
        );
        cache
            .put_blob_entries_batch("repo2", "config", &[(c, "{}".into())])
            .unwrap();
        assert_eq!(
            cache
                .get_blob_entries_batch("repo", "config", &[b, a])
                .unwrap(),
            [None, Some("{}".into())]
        );
        cache.conn.execute("UPDATE blob_extraction_cache SET created_at = datetime('now', '-61 seconds') WHERE file_path = 'a.ts'", []).unwrap();
        drop(cache);

        let cache = IndexCache::open_with_retention(&tmp, policy).unwrap();
        assert_eq!(
            cache
                .get_blob_entries_batch("repo", "config", &[a])
                .unwrap(),
            [None]
        );
        assert_eq!(
            cache
                .get_blob_entries_batch("repo2", "config", &[c])
                .unwrap(),
            [Some("{}".into())]
        );
        drop(cache);
        fs::remove_file(tmp).unwrap();
    }

    // Refuses a newer physical schema and rebuilds an incomplete current schema.
    #[test]
    fn checks_physical_version_and_table_structure() {
        let tmp =
            std::env::temp_dir().join(format!("laine-cache-physical-{}.db", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let cache = IndexCache::open(&tmp).unwrap();
        cache
            .conn
            .execute("UPDATE laine_cache_schema SET version = 3", [])
            .unwrap();
        drop(cache);
        assert!(matches!(
            IndexCache::open(&tmp),
            Err(LaineError::Cache(message)) if message.contains("newer Laine cache schema")
        ));
        let conn = Connection::open(&tmp).unwrap();
        conn.execute_batch(
            "UPDATE laine_cache_schema SET version = 2;
            DROP TABLE blob_extraction_cache; CREATE TABLE blob_extraction_cache (legacy TEXT);",
        )
        .unwrap();
        drop(conn);
        let cache = IndexCache::open(&tmp).unwrap();
        assert!(
            cache
                .conn
                .prepare("SELECT last_accessed_at FROM blob_extraction_cache")
                .is_ok()
        );
        drop(cache);
        fs::remove_file(tmp).unwrap();
    }

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
                consumer_service: None,
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
    // Verifica la separación de snapshots y la retirada de evidencias de versiones antiguas.
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

        cache
            .conn
            .execute(
                "INSERT INTO repo_index_cache
                    (repo, commit_sha, config_hash, schema_version, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "billing-service",
                    "sha000",
                    "cfg1",
                    3,
                    r#"{"snippet":"FICTITIOUS_LEGACY_SNIPPET"}"#
                ],
            )
            .unwrap();
        drop(cache);

        let cache = IndexCache::open(&tmp).expect("cache should reopen");
        let mut query = cache
            .conn
            .prepare("SELECT schema_version FROM repo_index_cache ORDER BY schema_version")
            .unwrap();
        let versions = query
            .query_map([], |row| row.get::<_, u32>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(versions, [CACHE_SCHEMA_VERSION, CACHE_SCHEMA_VERSION + 1]);
        assert!(
            !String::from_utf8_lossy(&fs::read(&tmp).unwrap())
                .contains("FICTITIOUS_LEGACY_SNIPPET")
        );
        drop(query);
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
