//! Startup schema guard: fail-closed before the first application query.
//!
//! Guard order (plan §4.2/§4.3, `docs/decisions.md` W0-7):
//!
//! 1. the database file must already exist (`mode=rw`, never create);
//! 2. no `-wal`/`-shm` sidecars and a legacy (rollback-journal) file header —
//!    checked with plain file reads so a WAL database is refused *before* SQLite
//!    can create sidecars or convert the journal mode (zero writes);
//! 3. every required connection PRAGMA has the documented value on the pooled
//!    connection (`busy_timeout=5000`, `foreign_keys=OFF`, `synchronous=FULL`,
//!    `cache_size=-2000`, `temp_store=FILE`) and `journal_mode=delete`
//!    (`journal_mode` is never *set*; it is only asserted);
//! 4. `SELECT version FROM schema_version` equals [`EXPECTED_SCHEMA_VERSION`] (7);
//! 5. `PRAGMA quick_check` returns `ok`.
//!
//! Any failure becomes a [`SchemaGuardError`] with a stable [`SchemaErrorKind`];
//! the composition root serves `/readyz` = 503 from it and logs one CRITICAL line.
//! Nothing in this module writes: all checks are reads (plus connection-local
//! PRAGMAs that do not touch the file).

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// Application schema version this binary understands (`schema_version.version`).
pub const EXPECTED_SCHEMA_VERSION: i64 = 7;
/// Required `PRAGMA busy_timeout` value on every connection (milliseconds).
pub const REQUIRED_BUSY_TIMEOUT_MS: i64 = 5000;
/// Required `PRAGMA cache_size` value on every connection (KiB, negative = KiB).
pub const REQUIRED_CACHE_SIZE: i64 = -2000;
/// `PRAGMA temp_store` value for FILE.
pub const TEMP_STORE_FILE: i64 = 1;
/// `PRAGMA synchronous` value for FULL.
pub const SYNCHRONOUS_FULL: i64 = 2;
/// `PRAGMA foreign_keys` value for OFF (legacy schema has zero FK constraints).
pub const FOREIGN_KEYS_OFF: i64 = 0;
/// Required persistent journal mode.
pub const REQUIRED_JOURNAL_MODE: &str = "delete";

/// SQLite file header magic (`SQLite format 3\0`).
const SQLITE_HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";
/// Bytes needed to read the SQLite header fields we check.
const SQLITE_HEADER_LEN: usize = 100;
/// Header offset of the file format write version (1 = legacy, 2 = WAL).
const HEADER_WRITE_VERSION: usize = 18;
/// Header offset of the file format read version (1 = legacy, 2 = WAL).
const HEADER_READ_VERSION: usize = 19;

/// Stable failure categories of the startup guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaErrorKind {
    /// The database file does not exist (and `mode=rw` must not create it).
    MissingFile,
    /// The connection URL is malformed or not exactly `mode=rw`.
    InvalidUrl,
    /// The file exists but has no valid SQLite header.
    InvalidHeader,
    /// A `-wal`/`-shm` sidecar exists next to the database file.
    WalSidecar,
    /// The database is in WAL mode (or an unknown journal format).
    JournalModeNotDelete,
    /// The pool could not be created / connected.
    OpenFailed,
    /// A guard query against the connection failed.
    GuardQueryFailed,
    /// A required connection PRAGMA has an unexpected value.
    PragmaMismatch,
    /// `schema_version` table is missing or unreadable.
    SchemaVersionUnreadable,
    /// `schema_version.version` is not [`EXPECTED_SCHEMA_VERSION`].
    SchemaVersionMismatch,
    /// `PRAGMA quick_check` did not return `ok`.
    QuickCheckFailed,
}

impl SchemaErrorKind {
    /// Stable snake_case code, safe for `/readyz` bodies and logs.
    pub fn code(self) -> &'static str {
        match self {
            SchemaErrorKind::MissingFile => "missing_file",
            SchemaErrorKind::InvalidUrl => "invalid_url",
            SchemaErrorKind::InvalidHeader => "invalid_header",
            SchemaErrorKind::WalSidecar => "wal_sidecar_present",
            SchemaErrorKind::JournalModeNotDelete => "journal_mode_not_delete",
            SchemaErrorKind::OpenFailed => "open_failed",
            SchemaErrorKind::GuardQueryFailed => "guard_query_failed",
            SchemaErrorKind::PragmaMismatch => "pragma_mismatch",
            SchemaErrorKind::SchemaVersionUnreadable => "schema_version_unreadable",
            SchemaErrorKind::SchemaVersionMismatch => "schema_version_mismatch",
            SchemaErrorKind::QuickCheckFailed => "quick_check_failed",
        }
    }
}

/// Fail-closed reason returned by [`super::Database::open`].
///
/// `Display` stays generic (safe for any log line); human-readable diagnostics
/// live in [`SchemaGuardError::detail`] and are meant for server logs only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaGuardError {
    kind: SchemaErrorKind,
    detail: Option<String>,
}

impl SchemaGuardError {
    /// Builds an error with an optional non-secret diagnostic detail.
    pub(crate) fn new(kind: SchemaErrorKind, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self {
            kind,
            detail: if detail.is_empty() {
                None
            } else {
                Some(detail)
            },
        }
    }

    /// Builds a guarded error without diagnostic detail.
    pub(crate) fn plain(kind: SchemaErrorKind) -> Self {
        Self { kind, detail: None }
    }

    /// Failure category.
    #[allow(dead_code)] // convenience accessor (tests/T05 readiness checks)
    pub fn kind(&self) -> SchemaErrorKind {
        self.kind
    }

    /// Stable snake_case code (safe for API/readiness responses).
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// Server-log-only diagnostic (never guaranteed to be client-safe).
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl fmt::Display for SchemaGuardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "database schema guard failed ({})",
            self.kind.code()
        )
    }
}

impl std::error::Error for SchemaGuardError {}

/// Non-secret facts proven by a successful guard, exposed by `/readyz`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardInfo {
    /// Version found in `schema_version` (`None` only when `ALLOW_UNKNOWN_SCHEMA=1`).
    pub schema_version: Option<i64>,
    /// Whether the lab-only `ALLOW_UNKNOWN_SCHEMA=1` bypass was active.
    pub allow_unknown_schema: bool,
}

/// Rejects a database file that is in WAL mode or has WAL sidecars.
///
/// This runs *before* any SQLite connection is opened, so the server cannot
/// create `-wal`/`-shm` files or convert the journal mode while refusing.
pub(crate) fn check_persistent_journal_mode(path: &Path) -> Result<(), SchemaGuardError> {
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        if Path::new(&sidecar).exists() {
            return Err(SchemaGuardError::new(
                SchemaErrorKind::WalSidecar,
                format!("sidecar file present: {suffix}"),
            ));
        }
    }

    let metadata = std::fs::metadata(path).map_err(|_| {
        SchemaGuardError::new(
            SchemaErrorKind::MissingFile,
            "database file is not readable",
        )
    })?;
    let length = metadata.len();
    if length == 0 {
        // SQLite treats a zero-byte file as a brand-new empty database.
        return Ok(());
    }
    if length < SQLITE_HEADER_LEN as u64 {
        return Err(SchemaGuardError::plain(SchemaErrorKind::InvalidHeader));
    }

    let mut header = [0_u8; SQLITE_HEADER_LEN];
    let mut file =
        File::open(path).map_err(|_| SchemaGuardError::plain(SchemaErrorKind::MissingFile))?;
    file.read_exact(&mut header)
        .map_err(|_| SchemaGuardError::plain(SchemaErrorKind::InvalidHeader))?;

    if &header[..SQLITE_HEADER_MAGIC.len()] != SQLITE_HEADER_MAGIC.as_slice() {
        return Err(SchemaGuardError::plain(SchemaErrorKind::InvalidHeader));
    }

    let write_version = header[HEADER_WRITE_VERSION];
    let read_version = header[HEADER_READ_VERSION];
    if write_version == 2 || read_version == 2 {
        return Err(SchemaGuardError::new(
            SchemaErrorKind::JournalModeNotDelete,
            "file header says WAL mode",
        ));
    }
    if write_version != 1 || read_version != 1 {
        return Err(SchemaGuardError::new(
            SchemaErrorKind::JournalModeNotDelete,
            format!("unexpected header journal versions {write_version}/{read_version}"),
        ));
    }
    Ok(())
}

/// Runs every connection/schema assertion on the (single) pooled connection.
pub(crate) async fn inspect(
    pool: &SqlitePool,
    allow_unknown_schema: bool,
) -> Result<GuardInfo, SchemaGuardError> {
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(pool)
        .await
        .map_err(|_| {
            SchemaGuardError::new(SchemaErrorKind::GuardQueryFailed, "PRAGMA journal_mode")
        })?;
    if !journal_mode.eq_ignore_ascii_case(REQUIRED_JOURNAL_MODE) {
        return Err(SchemaGuardError::new(
            SchemaErrorKind::JournalModeNotDelete,
            format!("journal_mode={journal_mode}"),
        ));
    }

    assert_pragma(
        pool,
        "PRAGMA busy_timeout",
        "busy_timeout",
        REQUIRED_BUSY_TIMEOUT_MS,
    )
    .await?;
    assert_pragma(
        pool,
        "PRAGMA foreign_keys",
        "foreign_keys",
        FOREIGN_KEYS_OFF,
    )
    .await?;
    assert_pragma(pool, "PRAGMA synchronous", "synchronous", SYNCHRONOUS_FULL).await?;
    assert_pragma(pool, "PRAGMA cache_size", "cache_size", REQUIRED_CACHE_SIZE).await?;
    assert_pragma(pool, "PRAGMA temp_store", "temp_store", TEMP_STORE_FILE).await?;

    let schema_version = if allow_unknown_schema {
        // Lab-only bypass: never enabled by production config (W0-7).
        None
    } else {
        Some(check_schema_version(pool).await?)
    };

    let quick_check: String = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_one(pool)
        .await
        .map_err(|_| {
            SchemaGuardError::new(SchemaErrorKind::GuardQueryFailed, "PRAGMA quick_check")
        })?;
    if !quick_check.eq_ignore_ascii_case("ok") {
        return Err(SchemaGuardError::new(
            SchemaErrorKind::QuickCheckFailed,
            truncate_detail(&quick_check),
        ));
    }

    Ok(GuardInfo {
        schema_version,
        allow_unknown_schema,
    })
}

async fn assert_pragma(
    pool: &SqlitePool,
    query: &'static str,
    name: &'static str,
    expected: i64,
) -> Result<(), SchemaGuardError> {
    // `query` is a fixed literal from this module, never user input.
    let row = sqlx::query(query)
        .fetch_one(pool)
        .await
        .map_err(|_| SchemaGuardError::new(SchemaErrorKind::GuardQueryFailed, name))?;
    let actual = match row.try_get::<i64, _>(0) {
        Ok(value) => value,
        Err(_) => {
            return Err(SchemaGuardError::new(
                SchemaErrorKind::GuardQueryFailed,
                name,
            ))
        }
    };
    if actual != expected {
        return Err(SchemaGuardError::new(
            SchemaErrorKind::PragmaMismatch,
            format!("{name}={actual} (expected {expected})"),
        ));
    }
    Ok(())
}

async fn check_schema_version(pool: &SqlitePool) -> Result<i64, SchemaGuardError> {
    // Runtime query only: the schema is validated at startup, never at build time.
    let version: Option<i64> = sqlx::query_scalar("SELECT version FROM schema_version LIMIT 1")
        .fetch_optional(pool)
        .await
        .map_err(|_| {
            SchemaGuardError::new(
                SchemaErrorKind::SchemaVersionUnreadable,
                "schema_version is missing or unreadable",
            )
        })?;
    match version {
        Some(EXPECTED_SCHEMA_VERSION) => Ok(EXPECTED_SCHEMA_VERSION),
        Some(found) => Err(SchemaGuardError::new(
            SchemaErrorKind::SchemaVersionMismatch,
            format!("found schema_version={found}, expected {EXPECTED_SCHEMA_VERSION}"),
        )),
        None => Err(SchemaGuardError::new(
            SchemaErrorKind::SchemaVersionMismatch,
            "schema_version table has no row",
        )),
    }
}

fn truncate_detail(value: &str) -> String {
    const MAX: usize = 200;
    let mut detail: String = value.chars().take(MAX).collect();
    if value.chars().count() > MAX {
        detail.push('…');
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::{exec_sql, file_bytes, TempDb};
    use crate::db::Database;

    #[test]
    fn expected_schema_version_is_seven() {
        assert_eq!(EXPECTED_SCHEMA_VERSION, 7);
    }

    #[tokio::test]
    async fn header_check_rejects_zero_byte_and_garbage_files() {
        let temp = TempDb::new().await;
        let empty = temp.path("empty.sqlite");
        std::fs::write(&empty, []).expect("create empty file");
        check_persistent_journal_mode(&empty).expect("zero-byte file is a valid empty database");

        let garbage = temp.path("garbage.sqlite");
        std::fs::write(&garbage, vec![0xAA_u8; 4096]).expect("create garbage file");
        let error = check_persistent_journal_mode(&garbage).expect_err("garbage header");
        assert_eq!(error.kind(), SchemaErrorKind::InvalidHeader);

        let short = temp.path("short.sqlite");
        std::fs::write(&short, b"SQLite format 3").expect("create short file");
        let error = check_persistent_journal_mode(&short).expect_err("short header");
        assert_eq!(error.kind(), SchemaErrorKind::InvalidHeader);
    }

    #[tokio::test]
    async fn guard_accepts_v7_and_reports_it() {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("fixture v7 must pass the guard");
        let info = database.guard_info();
        assert_eq!(info.schema_version, Some(7));
        assert!(!info.allow_unknown_schema);
        database.close().await;
    }

    #[tokio::test]
    async fn guard_refuses_schema_6_and_8_without_writing_the_file() {
        for version in [6_i64, 8_i64] {
            let temp = TempDb::new().await;
            exec_sql(
                &temp.database_path,
                &format!("UPDATE schema_version SET version = {version}"),
            )
            .await;
            let before = file_bytes(&temp.database_path);

            let error = match Database::open(&temp.url(), false).await {
                Ok(database) => {
                    database.close().await;
                    panic!("schema {version} must be refused");
                }
                Err(error) => error,
            };

            assert_eq!(error.kind(), SchemaErrorKind::SchemaVersionMismatch);
            assert_eq!(
                file_bytes(&temp.database_path),
                before,
                "guard must not modify the file"
            );
            assert!(!TempDb::sidecar(&temp.database_path, "-journal").exists());
            assert!(!TempDb::sidecar(&temp.database_path, "-wal").exists());
            assert!(!TempDb::sidecar(&temp.database_path, "-shm").exists());
        }
    }

    #[tokio::test]
    async fn guard_refuses_missing_file_and_does_not_create_it() {
        let temp = TempDb::new().await;
        let missing = temp.path("missing.sqlite");
        let error = Database::open(&TempDb::url_for(&missing), false)
            .await
            .expect_err("missing file must be fail-closed");
        assert_eq!(error.kind(), SchemaErrorKind::MissingFile);
        assert!(!missing.exists(), "mode=rw must never create the file");
    }

    #[tokio::test]
    async fn guard_refuses_wal_without_converting_or_creating_sidecars() {
        let temp = TempDb::new().await;
        exec_sql(&temp.database_path, "PRAGMA journal_mode = WAL").await;
        let before = file_bytes(&temp.database_path);
        // SQLite leaves -wal/-shm behind on close in WAL mode; remove them so the
        // header check (not the sidecar check) is what refuses the database.
        let _ = std::fs::remove_file(TempDb::sidecar(&temp.database_path, "-wal"));
        let _ = std::fs::remove_file(TempDb::sidecar(&temp.database_path, "-shm"));

        let error = Database::open(&temp.url(), false)
            .await
            .expect_err("WAL must be fail-closed");
        assert_eq!(error.kind(), SchemaErrorKind::JournalModeNotDelete);
        assert_eq!(
            file_bytes(&temp.database_path),
            before,
            "WAL file must stay untouched"
        );
        assert!(!TempDb::sidecar(&temp.database_path, "-wal").exists());
        assert!(!TempDb::sidecar(&temp.database_path, "-shm").exists());
    }

    #[tokio::test]
    async fn guard_refuses_wal_sidecars() {
        let temp = TempDb::new().await;
        std::fs::write(TempDb::sidecar(&temp.database_path, "-wal"), b"").expect("fake -wal");
        let error = Database::open(&temp.url(), false)
            .await
            .expect_err("sidecar must be fail-closed");
        assert_eq!(error.kind(), SchemaErrorKind::WalSidecar);
    }

    #[tokio::test]
    async fn guard_refuses_corrupted_database_with_quick_check() {
        let temp = TempDb::new().await;
        // Make sure the file has data pages, then destroy the last page.
        exec_sql(
            &temp.database_path,
            "INSERT INTO timeline(profile_id, hash, data, updated_at) \
             SELECT 1, hex(randomblob(4)), hex(randomblob(1500)), 1 FROM (WITH RECURSIVE c(x) \
             AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 120) SELECT x FROM c)",
        )
        .await;
        let mut bytes = file_bytes(&temp.database_path);
        let page_size = 4096;
        assert!(bytes.len() > page_size * 3);
        let last_page = (bytes.len() / page_size - 1) * page_size;
        for byte in &mut bytes[last_page..] {
            *byte = 0xAA;
        }
        std::fs::write(&temp.database_path, &bytes).expect("corrupt last page");
        let before = file_bytes(&temp.database_path);

        let error = Database::open(&temp.url(), false)
            .await
            .expect_err("corrupted database must be fail-closed");
        assert_eq!(error.kind(), SchemaErrorKind::QuickCheckFailed);
        assert_eq!(
            file_bytes(&temp.database_path),
            before,
            "guard must not repair the file"
        );
    }

    #[tokio::test]
    async fn allow_unknown_schema_is_lab_only_bypass() {
        let temp = TempDb::new().await;
        let database = temp.open_with(true).await.expect("lab bypass opens v7");
        assert_eq!(database.guard_info().schema_version, None);
        assert!(database.guard_info().allow_unknown_schema);
        database.close().await;

        exec_sql(&temp.database_path, "UPDATE schema_version SET version = 6").await;
        let database = temp
            .open_with(true)
            .await
            .expect("lab bypass accepts schema 6");
        database.close().await;
    }

    #[tokio::test]
    async fn allow_unknown_schema_still_requires_quick_check() {
        let temp = TempDb::new().await;
        std::fs::write(&temp.database_path, vec![0xAA_u8; 4096]).expect("garbage");
        let error = Database::open(&temp.url(), true)
            .await
            .expect_err("garbage is not a database even in lab mode");
        assert_eq!(error.kind(), SchemaErrorKind::InvalidHeader);
    }

    #[test]
    fn error_kinds_have_stable_codes() {
        assert_eq!(SchemaErrorKind::MissingFile.code(), "missing_file");
        assert_eq!(
            SchemaErrorKind::SchemaVersionMismatch.code(),
            "schema_version_mismatch"
        );
        assert_eq!(
            SchemaErrorKind::JournalModeNotDelete.code(),
            "journal_mode_not_delete"
        );
        assert_eq!(
            SchemaErrorKind::QuickCheckFailed.code(),
            "quick_check_failed"
        );
    }
}
