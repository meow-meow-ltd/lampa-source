//! Test-only helpers: synthetic v7 databases under the OS temp directory.
//!
//! TEST-ONLY (`#[cfg(test)]`): this module is never part of a release build.
//! It reads `tests/fixtures/schema7.sql` (embedded at test-compile time) and
//! applies it exclusively to throw-away files created under `std::env::temp_dir()`
//! (`/tmp` in the builder container). It is never pointed at `/opt/lampa/data`,
//! `/opt/lampa/staging-data`, or any other production/staging database.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Connection;

use crate::db::{Database, SchemaGuardError};

/// Exact legacy v7 DDL (TEST-ONLY; see the fixture header for provenance).
const SCHEMA7_SQL: &str = include_str!("../../tests/fixtures/schema7.sql");

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Owning handle to a temporary directory plus the synthetic `database.sqlite`.
pub struct TempDb {
    dir: PathBuf,
    /// Path to the synthetic database file (`<dir>/database.sqlite`).
    pub database_path: PathBuf,
}

impl TempDb {
    /// Creates an empty temporary directory; no database file yet.
    pub fn empty() -> Self {
        let unique = format!(
            "lampa-db-test-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let database_path = dir.join("database.sqlite");
        Self { dir, database_path }
    }

    /// Creates a temporary directory and applies `schema7.sql` to `database.sqlite`.
    pub async fn new() -> Self {
        let temp = Self::empty();
        build_schema7(&temp.database_path).await;
        temp
    }

    /// Path inside the temporary directory.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// `sqlite:<path>?mode=rw` URL for the synthetic database.
    pub fn url(&self) -> String {
        Self::url_for(&self.database_path)
    }

    /// `mode=rw` URL for an arbitrary path.
    pub fn url_for(path: &Path) -> String {
        format!("sqlite:{}?mode=rw", path.display())
    }

    /// `-wal`/`-shm`/`-journal` sidecar path (`<db><suffix>`).
    pub fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        PathBuf::from(sidecar)
    }

    /// Opens the synthetic database through the production guard.
    pub async fn open(&self) -> Result<Database, SchemaGuardError> {
        self.open_with(false).await
    }

    /// Opens the synthetic database with an explicit `ALLOW_UNKNOWN_SCHEMA` flag.
    pub async fn open_with(
        &self,
        allow_unknown_schema: bool,
    ) -> Result<Database, SchemaGuardError> {
        Database::open(&self.url(), allow_unknown_schema).await
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Builds the v7 fixture database (TEST-ONLY; DDL never runs in production).
pub async fn build_schema7(path: &Path) -> PathBuf {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(false);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("open synthetic database");
    sqlx::raw_sql(SCHEMA7_SQL)
        .execute(&pool)
        .await
        .expect("apply test-only schema7.sql fixture");
    let objects: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master")
        .fetch_one(&pool)
        .await
        .expect("count sqlite_master objects");
    let version: i64 = sqlx::query_scalar("SELECT version FROM schema_version")
        .fetch_one(&pool)
        .await
        .expect("read schema_version");
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .expect("read journal_mode");
    pool.close().await;
    assert_eq!(
        objects, 23,
        "fixture must reproduce 23 sqlite_schema objects"
    );
    assert_eq!(version, 7, "fixture must set schema_version=7");
    assert_eq!(
        journal_mode, "delete",
        "fixture must stay in delete journal mode"
    );
    path.to_path_buf()
}

/// Runs arbitrary SQL against an existing synthetic database (tests only).
pub async fn exec_sql(path: &Path, sql: &str) {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(false);
    let mut connection = sqlx::SqliteConnection::connect_with(&options)
        .await
        .expect("open synthetic database for test SQL");
    sqlx::raw_sql(sql)
        .execute(&mut connection)
        .await
        .unwrap_or_else(|error| panic!("test SQL failed: {error}"));
    connection.close().await.expect("close test connection");
}

/// Reads the whole database file (for byte-identity assertions in tests).
pub fn file_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read synthetic database file")
}
