//! SQLite access layer (W0/T04).
//!
//! * one pooled writer connection ([`options::MAX_CONNECTIONS`] = 1), `mode=rw`;
//! * documented PRAGMAs on every connection, `journal_mode` asserted (never set);
//! * a fail-closed startup [`guard`] (`schema_version` = 7, `quick_check` = ok,
//!   `delete` journal mode) returning [`SchemaGuardError`] for `/readyz` = 503;
//! * runtime queries only — no compile-time query macros, no migrations, no
//!   runtime DDL.
//!
//! This module is wired into the composition root (`crate::main`) and owns no
//! HTTP policy: the settings/profiles repositories below are consumed by T07/T09.

pub mod backup;
pub mod error;
pub mod guard;
mod options;
#[cfg(test)]
pub(crate) mod test_support;

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

pub use error::DbError;
pub use guard::{GuardInfo, SchemaErrorKind, SchemaGuardError};

use crate::profiles::ProfilesRepo;
use crate::settings::SettingsRepo;

/// Open, guarded handle to the legacy v7 database.
///
/// Cloning is cheap (the pool is an `Arc` internally) and every clone shares the
/// same single connection. Call [`Database::close`] once during graceful shutdown.
#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
    guard: GuardInfo,
}

impl Database {
    /// Opens an existing database read-write and runs the startup guard.
    ///
    /// * `url` must be `sqlite:<absolute path>?mode=rw` — the file is never created;
    /// * `allow_unknown_schema` is the lab-only `ALLOW_UNKNOWN_SCHEMA=1` bypass
    ///   (skips only the `schema_version` equality check, never quick_check/journal).
    ///
    /// On any failure the pool is closed before returning, so a refused database
    /// is left untouched (zero writes).
    pub async fn open(url: &str, allow_unknown_schema: bool) -> Result<Self, SchemaGuardError> {
        let options = options::connect_options(url)?;
        let path = options.get_filename().to_path_buf();
        if !path.is_file() {
            return Err(SchemaGuardError::new(
                SchemaErrorKind::MissingFile,
                "database file does not exist (mode=rw never creates it)",
            ));
        }
        // File-level WAL refusal happens before SQLite opens the file, so no
        // -wal/-shm sidecars can be created while refusing.
        guard::check_persistent_journal_mode(&path)?;

        let pool = SqlitePoolOptions::new()
            .max_connections(options::MAX_CONNECTIONS)
            .min_connections(options::MIN_CONNECTIONS)
            .acquire_timeout(options::ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await
            .map_err(|_| {
                SchemaGuardError::new(SchemaErrorKind::OpenFailed, "pool connect failed")
            })?;

        match guard::inspect(&pool, allow_unknown_schema).await {
            Ok(guard) => Ok(Self { pool, guard }),
            Err(error) => {
                pool.close().await;
                Err(error)
            }
        }
    }

    /// The single-writer pool (exposed for repositories and health checks).
    // `pool()`/`settings()`/`profiles()` are the T04 integration surface: the HTTP
    // API layers (T07/T09) consume them; until then only tests and the guard do.
    #[allow(dead_code)]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Facts proven by the startup guard, reported by `/readyz`.
    pub fn guard_info(&self) -> GuardInfo {
        self.guard
    }

    /// Settings repository (see `crate::settings`).
    #[allow(dead_code)]
    pub fn settings(&self) -> SettingsRepo {
        SettingsRepo::new(self.pool.clone())
    }

    /// Profiles repository (see `crate::profiles`).
    #[allow(dead_code)]
    pub fn profiles(&self) -> ProfilesRepo {
        ProfilesRepo::new(self.pool.clone())
    }

    /// Closes the pool during graceful shutdown.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::error::DbErrorKind;
    use crate::db::options::{MAX_CONNECTIONS, MIN_CONNECTIONS};
    use crate::db::test_support::{file_bytes, TempDb};
    use crate::profiles::ProfileUpdate;
    use sqlx::Row;

    /// Legacy application tables (10) — `sqlite_sequence` is checked separately.
    const APP_TABLES: [&str; 10] = [
        "settings",
        "schema_version",
        "profiles",
        "bookmarks",
        "bookmark_events",
        "timeline",
        "timeline_events",
        "profile_settings",
        "device_access",
        "ip_rate_limits",
    ];

    async fn table_counts(pool: &SqlitePool) -> Vec<(String, i64)> {
        let mut counts = Vec::with_capacity(APP_TABLES.len());
        for table in APP_TABLES {
            // Table names are the fixed list above, never user input.
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(pool)
                .await
                .expect("count table rows");
            counts.push((table.to_owned(), count));
        }
        counts
    }

    async fn sequence_rows(pool: &SqlitePool) -> Vec<(String, i64)> {
        sqlx::query_as::<_, (String, i64)>("SELECT name, seq FROM sqlite_sequence ORDER BY name")
            .fetch_all(pool)
            .await
            .expect("read sqlite_sequence")
    }

    async fn schema_objects(pool: &SqlitePool) -> Vec<(String, String)> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT type || ':' || name, COALESCE(sql, '') FROM sqlite_master \
             ORDER BY type, name",
        )
        .fetch_all(pool)
        .await
        .expect("read sqlite_master")
    }

    #[tokio::test]
    async fn pool_is_a_single_writer_with_the_documented_pragmas() {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");

        assert_eq!(MAX_CONNECTIONS, 1);
        assert_eq!(MIN_CONNECTIONS, 1);
        assert_eq!(database.pool().options().get_max_connections(), 1);
        assert_eq!(database.pool().options().get_min_connections(), 1);

        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(database.pool())
            .await
            .expect("journal_mode");
        assert_eq!(journal_mode, "delete");

        let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(database.pool())
            .await
            .expect("busy_timeout");
        assert_eq!(busy_timeout, 5000);

        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(database.pool())
            .await
            .expect("foreign_keys");
        assert_eq!(
            foreign_keys, 0,
            "legacy schema has no FK; orphans are normal"
        );

        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(database.pool())
            .await
            .expect("synchronous");
        assert_eq!(synchronous, 2, "FULL");

        let cache_size: i64 = sqlx::query_scalar("PRAGMA cache_size")
            .fetch_one(database.pool())
            .await
            .expect("cache_size");
        assert_eq!(cache_size, -2000);

        let temp_store: i64 = sqlx::query_scalar("PRAGMA temp_store")
            .fetch_one(database.pool())
            .await
            .expect("temp_store");
        assert_eq!(temp_store, 1, "FILE");

        assert_eq!(database.pool().size(), 1, "exactly one pooled connection");
        database.close().await;
    }

    #[tokio::test]
    async fn hundred_cycle_crud_keeps_integrity_counts_sequence_and_schema() {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        let settings = database.settings();
        let profiles = database.profiles();

        // `GET all creates Main when empty`, which also seeds sqlite_sequence(profiles).
        let main = profiles
            .list()
            .await
            .expect("list profiles")
            .into_iter()
            .next()
            .expect("Main profile must be created");

        // One pre-existing row in each non-AUTOINCREMENT table we exercise.
        sqlx::query(
            "INSERT INTO timeline(profile_id, hash, data, updated_at) \
             VALUES(?, 'pre-existing', '{}', 1)",
        )
        .bind(main.id)
        .execute(database.pool())
        .await
        .expect("seed timeline");
        sqlx::query(
            "INSERT INTO profile_settings(profile_id, key, value, updated_at) \
             VALUES(?, 'pre-existing', '1', 1)",
        )
        .bind(main.id)
        .execute(database.pool())
        .await
        .expect("seed profile_settings");

        let counts_before = table_counts(database.pool()).await;
        let sequence_before = sequence_rows(database.pool()).await;
        let schema_before = schema_objects(database.pool()).await;
        let file_before = file_bytes(&temp.database_path);

        for cycle in 0..100_i64 {
            let value = format!("value-{cycle}");
            settings
                .upsert("cycle_key", &value)
                .await
                .expect("settings upsert");
            let stored = settings
                .get("cycle_key")
                .await
                .expect("settings get")
                .expect("cycle key exists");
            assert_eq!(stored.value.as_deref(), Some(value.as_str()));
            assert!(settings.delete("cycle_key").await.expect("settings delete"));

            profiles
                .update(
                    main.id,
                    ProfileUpdate {
                        name: Some(format!("Main {cycle}")),
                        ..ProfileUpdate::default()
                    },
                )
                .await
                .expect("profile update");

            // Non-AUTOINCREMENT CRUD: PK-keyed rows, so sqlite_sequence is stable.
            sqlx::query(
                "INSERT INTO timeline(profile_id, hash, data, updated_at) VALUES(?, ?, ?, ?) \
                 ON CONFLICT(profile_id, hash) DO UPDATE SET data=excluded.data, \
                 updated_at=excluded.updated_at",
            )
            .bind(main.id)
            .bind("cycle-hash")
            .bind(format!("{{\"cycle\":{cycle}}}"))
            .bind(cycle)
            .execute(database.pool())
            .await
            .expect("timeline upsert");
            sqlx::query("DELETE FROM timeline WHERE profile_id = ? AND hash = ?")
                .bind(main.id)
                .bind("cycle-hash")
                .execute(database.pool())
                .await
                .expect("timeline delete");

            sqlx::query(
                "INSERT INTO profile_settings(profile_id, key, value, updated_at) \
                 VALUES(?, 'cycle-key', ?, ?) \
                 ON CONFLICT(profile_id, key) DO UPDATE SET value=excluded.value, \
                 updated_at=excluded.updated_at",
            )
            .bind(main.id)
            .bind(format!("{cycle}"))
            .bind(cycle)
            .execute(database.pool())
            .await
            .expect("profile_settings upsert");
            sqlx::query("DELETE FROM profile_settings WHERE profile_id = ? AND key = 'cycle-key'")
                .bind(main.id)
                .execute(database.pool())
                .await
                .expect("profile_settings delete");
        }

        assert_eq!(
            table_counts(database.pool()).await,
            counts_before,
            "100 CRUD cycles must not lose or duplicate rows"
        );
        assert_eq!(
            sequence_rows(database.pool()).await,
            sequence_before,
            "sqlite_sequence must not change during non-AUTOINCREMENT CRUD"
        );
        assert_eq!(
            schema_objects(database.pool()).await,
            schema_before,
            "no DDL may run at runtime"
        );

        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(database.pool())
            .await
            .expect("integrity_check");
        assert_eq!(integrity, "ok");
        let quick_check: String = sqlx::query_scalar("PRAGMA quick_check")
            .fetch_one(database.pool())
            .await
            .expect("quick_check");
        assert_eq!(quick_check, "ok");
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(database.pool())
            .await
            .expect("journal_mode");
        assert_eq!(journal_mode, "delete", "journal mode must stay delete");

        // No migration bookkeeping may exist: the runtime never creates tables.
        // (The explicit migration-table grep lives outside `server/src`.)
        let table_names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(database.pool())
                .await
                .expect("list sqlite_master tables");
        assert_eq!(
            table_names,
            vec![
                "bookmark_events",
                "bookmarks",
                "device_access",
                "ip_rate_limits",
                "profile_settings",
                "profiles",
                "schema_version",
                "settings",
                "sqlite_sequence",
                "timeline",
                "timeline_events",
            ]
        );

        // The file necessarily changed (writes happened); what matters is that the
        // database is still a healthy delete-journal v7 file with the same schema.
        assert_ne!(file_bytes(&temp.database_path), file_before);
        database.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reads_and_writes_do_not_raise_sqlite_busy() {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        database.profiles().list().await.expect("seed Main profile");

        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..20_i64 {
            let settings = database.settings();
            tasks.spawn(async move {
                settings
                    .upsert(&format!("concurrent-{index}"), &format!("value-{index}"))
                    .await
            });
        }
        for _ in 0..20 {
            let settings = database.settings();
            tasks.spawn(async move { settings.list().await.map(|_| ()) });
        }
        while let Some(joined) = tasks.join_next().await {
            joined
                .expect("task must not panic")
                .expect("single-writer pool must serialise without SQLITE_BUSY");
        }

        let rows = database.settings().list().await.expect("list settings");
        assert_eq!(rows.len(), 20, "all concurrent upserts landed");
        assert_eq!(database.pool().size(), 1);
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(database.pool())
            .await
            .expect("journal_mode");
        assert_eq!(journal_mode, "delete");
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(database.pool())
            .await
            .expect("integrity_check");
        assert_eq!(integrity, "ok");
        database.close().await;
    }

    #[tokio::test]
    async fn closing_the_pool_releases_the_database() {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        database.close().await;
        let settings = database.settings();
        let error = settings.list().await.expect_err("closed pool must fail");
        assert_eq!(error.kind(), DbErrorKind::Query);
        // Sanitised error: no driver text on the wire.
        assert_eq!(
            error.to_string(),
            "database operation failed (db_query_failed)"
        );
    }

    #[tokio::test]
    async fn row_decoding_is_available_without_macros() {
        // Sanity check for the runtime-query style used by the repositories.
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        let row = sqlx::query("SELECT key, visible FROM settings LIMIT 1")
            .fetch_optional(database.pool())
            .await
            .expect("runtime query");
        assert!(row.is_none(), "fixture has no settings rows");
        let version_row = sqlx::query("SELECT version FROM schema_version")
            .fetch_one(database.pool())
            .await
            .expect("runtime query");
        assert_eq!(version_row.get::<i64, _>("version"), 7);
        database.close().await;
    }
}
