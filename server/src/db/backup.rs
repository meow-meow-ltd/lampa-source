//! SQLite online backup / offline restore core (W1/T11).
//!
//! Source: plan §4.4/§13.3 T11, `review/inv/S2-sqlite.md` §6 and
//! `docs/decisions.md` (backup only through the SQLite backup API, never `cp`).
//!
//! * A backup is taken with the **rusqlite backup API** (`rusqlite::backup`,
//!   feature `backup`) from one **separate read-only connection**; the
//!   single-writer SQLx pool is never borrowed, so the server keeps serving
//!   writes while the copy runs.
//! * The artefact is `/data/backups/database-YYYYMMDDTHHMMSSZ.sqlite`
//!   (UTC), mode `0600` in a `0700` directory. Exactly the newest 14 artefacts
//!   are retained; only names produced by this module are pruned.
//! * Every artefact is verified with `PRAGMA integrity_check` + `quick_check`
//!   (must be `ok`) and hashed with SHA-256. Both are written to the log; the
//!   SHA-256 is the only secret-free digest surfaced by the API.
//! * `journal_mode` is never set here. The destination of a fresh backup is a
//!   new file, so SQLite uses the legacy `delete` journal; the restored file
//!   keeps the header of the verified artefact (also `delete`). No
//!   schema-changing statement is ever executed.
//! * Restore is **offline only**: the operator stops the server, runs the CLI
//!   (`lampa-server restore --offline ...`), then starts it again. There is no
//!   HTTP restore route. The target is replaced atomically (temp file in the
//!   same directory + `rename`) only after the artefact passed verification,
//!   and `/opt/lampa/data` (production) plus `/opt/lampa/staging-data` are
//!   refused unless `--allow-prod-data` is passed explicitly.
//! * The CLI itself lives here (`cli`/`run_cli`); `main` dispatches
//!   `backup`/`restore`/`check` to it before starting the HTTP server.
//!
//! Errors are sanitised: [`BackupError`] exposes a stable [`code`] and a static
//! message; driver detail is kept for logs only and is never serialised by the
//! maintenance API.
//!
//! [`code`]: BackupErrorKind::code

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::backup::StepResult;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

/// Subdirectory of the data volume that holds the artefacts.
pub const BACKUP_DIR_NAME: &str = "backups";
/// Artefact name prefix (`database-`).
pub const BACKUP_FILE_PREFIX: &str = "database-";
/// Artefact name suffix (`.sqlite`).
pub const BACKUP_FILE_SUFFIX: &str = ".sqlite";
/// Number of artefacts kept by the retention pass (newest first).
pub const BACKUP_RETENTION: usize = 14;
/// Artefact mode: owner read/write only.
pub const BACKUP_FILE_MODE: u32 = 0o600;
/// Backup directory mode: owner only.
pub const BACKUP_DIR_MODE: u32 = 0o700;
/// Production host data directory — never a restore target by default.
pub const PROD_DATA_DIR: &str = "/opt/lampa/data";
/// Staging host data directory — never a restore target by default.
pub const STAGING_DATA_DIR: &str = "/opt/lampa/staging-data";
/// Application schema version of the legacy v7 database.
pub const EXPECTED_SCHEMA_VERSION: i64 = 7;
/// `PRAGMA busy_timeout` on the auxiliary backup/verify connections.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);
/// How long a one-shot backup may retry while the writer briefly holds a lock.
const BACKUP_RETRY_DEADLINE: Duration = Duration::from_secs(5);
/// Pause between `SQLITE_BUSY`/`SQLITE_LOCKED` retries.
const BACKUP_RETRY_PAUSE: Duration = Duration::from_millis(2);
/// Buffer size for hashing and the offline restore file copy.
const COPY_BUFFER: usize = 64 * 1024;
/// Accepted source `journal_mode` (legacy, never WAL — plan §4.5/§11).
const REQUIRED_JOURNAL_MODE: &str = "delete";

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Stable, sanitised failure classes of the backup/restore core.
///
/// The `code` is the only value the maintenance API serialises; messages and
/// driver detail never contain SQL text on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupErrorKind {
    /// The source database file does not exist (or is not a regular file).
    MissingDatabase,
    /// The backup artefact does not exist.
    MissingBackup,
    /// A path is relative, not a file, or otherwise unusable.
    InvalidPath,
    /// The restore target is a protected production/staging directory.
    UnsafeTarget,
    /// Offline restore was invoked without the explicit `--offline` marker.
    OfflineRequired,
    /// The target has a hot journal / sidecar or an active writer holds a lock.
    DatabaseInUse,
    /// `PRAGMA integrity_check` did not return `ok`.
    IntegrityCheckFailed,
    /// `PRAGMA quick_check` did not return `ok`.
    QuickCheckFailed,
    /// The artefact is not a v7 legacy database.
    SchemaMismatch,
    /// The artefact is not in the legacy `delete` journal mode.
    JournalModeMismatch,
    /// Filesystem failure.
    Io,
    /// SQLite failure (detail is log-only).
    Sqlite,
    /// The backup API itself failed.
    BackupFailed,
    /// The replacement/copy phase of an offline restore failed.
    RestoreFailed,
    /// The system clock is before the Unix epoch.
    Clock,
    /// Invalid CLI usage (missing/unknown flag or subcommand).
    Usage,
}

impl BackupErrorKind {
    /// Stable machine-readable code (the only error value sent to clients).
    pub fn code(self) -> &'static str {
        match self {
            Self::MissingDatabase => "missing_database",
            Self::MissingBackup => "missing_backup",
            Self::InvalidPath => "invalid_path",
            Self::UnsafeTarget => "unsafe_target",
            Self::OfflineRequired => "offline_required",
            Self::DatabaseInUse => "database_in_use",
            Self::IntegrityCheckFailed => "integrity_check_failed",
            Self::QuickCheckFailed => "quick_check_failed",
            Self::SchemaMismatch => "schema_mismatch",
            Self::JournalModeMismatch => "journal_mode_mismatch",
            Self::Io => "io_error",
            Self::Sqlite => "sqlite_error",
            Self::BackupFailed => "backup_failed",
            Self::RestoreFailed => "restore_failed",
            Self::Clock => "clock_error",
            Self::Usage => "usage",
        }
    }

    /// Static human message without driver/SQL detail.
    fn message(self) -> &'static str {
        match self {
            Self::MissingDatabase => "database file is missing",
            Self::MissingBackup => "backup file is missing",
            Self::InvalidPath => "path is not usable for this operation",
            Self::UnsafeTarget => "restore target is a protected data directory",
            Self::OfflineRequired => "restore requires an explicit offline confirmation",
            Self::DatabaseInUse => "database is in use; stop the service before restoring",
            Self::IntegrityCheckFailed => "integrity_check did not return ok",
            Self::QuickCheckFailed => "quick_check did not return ok",
            Self::SchemaMismatch => "database schema version is not 7",
            Self::JournalModeMismatch => "database journal_mode is not delete",
            Self::Io => "filesystem operation failed",
            Self::Sqlite => "SQLite operation failed",
            Self::BackupFailed => "SQLite backup API failed",
            Self::RestoreFailed => "offline restore failed",
            Self::Clock => "system clock is before the Unix epoch",
            Self::Usage => "invalid command line",
        }
    }
}

/// Sanitised backup/restore error.
///
/// `detail()` is intended for logs only; it may contain driver text and must
/// never be returned to an HTTP client.
#[derive(Debug, Clone)]
pub struct BackupError {
    kind: BackupErrorKind,
    detail: Option<String>,
}

impl BackupError {
    fn new(kind: BackupErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: Some(detail.into()),
        }
    }

    fn plain(kind: BackupErrorKind) -> Self {
        Self { kind, detail: None }
    }

    fn io(context: &str, error: io::Error) -> Self {
        Self::new(BackupErrorKind::Io, format!("{context}: {error}"))
    }

    fn sqlite(error: rusqlite::Error) -> Self {
        Self::new(BackupErrorKind::Sqlite, error.to_string())
    }

    /// Stable failure class.
    pub fn kind(&self) -> BackupErrorKind {
        self.kind
    }

    /// Driver detail for logs only.
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl fmt::Display for BackupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.kind.message(), self.kind.code())
    }
}

impl std::error::Error for BackupError {}

/// Result of a successful backup.
#[derive(Debug, Clone)]
pub struct BackupOutcome {
    /// Full path of the created artefact.
    pub file: PathBuf,
    /// Artefact size in bytes.
    pub bytes: u64,
    /// Lower-case hex SHA-256 of the artefact.
    pub sha256: String,
    /// Wall-clock duration of the whole operation (including digest/pruning).
    pub ms: u64,
    /// `PRAGMA integrity_check` result (`ok`).
    pub integrity: String,
    /// `PRAGMA journal_mode` of the artefact (`delete`).
    pub journal_mode: String,
    /// How many old artefacts the retention pass removed.
    pub removed: usize,
}

/// Result of verifying one database/artefact file.
#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    /// Full path of the verified file.
    pub file: PathBuf,
    /// File size in bytes.
    pub bytes: u64,
    /// Lower-case hex SHA-256 of the file.
    pub sha256: String,
    /// `PRAGMA integrity_check` result (`ok`).
    pub integrity: String,
    /// `PRAGMA quick_check` result (`ok`).
    pub quick_check: String,
    /// Persistent journal mode (must be `delete` for a restorable v7 file).
    pub journal_mode: String,
    /// `schema_version.version` value, when the table/row exists.
    pub schema_version: Option<i64>,
    /// Duration of the verification (integrity + digest).
    pub ms: u64,
}

/// Options for the offline restore CLI.
#[derive(Debug, Clone, Copy, Default)]
pub struct RestoreOptions {
    /// Explicit acknowledgement that the operator stopped the service.
    pub offline: bool,
    /// Allow `/opt/lampa/data` (production) or `/opt/lampa/staging-data`.
    pub allow_prod_data: bool,
}

/// Result of a successful offline restore.
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    /// Restored target database path.
    pub target: PathBuf,
    /// Verified source artefact.
    pub source: PathBuf,
    /// Size of the restored file in bytes.
    pub bytes: u64,
    /// SHA-256 of the restored file (equals the artefact digest).
    pub sha256: String,
    /// Wall-clock duration of the whole restore.
    pub ms: u64,
    /// `PRAGMA integrity_check` of the restored file (`ok`).
    pub integrity: String,
    /// `PRAGMA journal_mode` of the restored file (`delete`).
    pub journal_mode: String,
    /// `schema_version.version` of the restored file (7).
    pub schema_version: i64,
}

/// Parses the path out of a validated `sqlite:<absolute path>?mode=rw` URL.
///
/// The W0 configuration validation is the source of truth; this helper is only
/// used by the maintenance API/CLI to locate the file behind the URL.
pub fn database_path_from_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("sqlite:")?;
    let (path, _query) = rest.split_once('?').unwrap_or((rest, ""));
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

/// Default artefact directory for a database file (`<data dir>/backups`).
///
/// `/data/database.sqlite` → `/data/backups`, exactly the path required by
/// plan §4.4. Tests use temporary databases, so their artefacts stay in the
/// temporary directory.
pub fn default_backups_dir(database_path: &Path) -> PathBuf {
    database_path
        .parent()
        .map(|parent| parent.join(BACKUP_DIR_NAME))
        .unwrap_or_else(|| PathBuf::from(BACKUP_DIR_NAME))
}

/// Formats the UTC artefact name for a Unix timestamp.
///
/// `database-YYYYMMDDTHHMMSSZ.sqlite` — lexicographic order equals
/// chronological order, which the retention pass relies on.
pub fn backup_file_name(seconds_since_epoch: i64) -> String {
    let days = seconds_since_epoch.div_euclid(86_400);
    let seconds_of_day = seconds_since_epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!(
        "{BACKUP_FILE_PREFIX}{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z\
         {BACKUP_FILE_SUFFIX}"
    )
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01 → y/m/d).
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

/// True when `name` is exactly one of our artefacts.
fn is_backup_name(name: &str) -> bool {
    let Some(stamp) = name
        .strip_prefix(BACKUP_FILE_PREFIX)
        .and_then(|rest| rest.strip_suffix(BACKUP_FILE_SUFFIX))
    else {
        return false;
    };
    let bytes = stamp.as_bytes();
    if bytes.len() != 16 || bytes[8] != b'T' || bytes[15] != b'Z' {
        return false;
    }
    bytes[..8].iter().all(u8::is_ascii_digit) && bytes[9..15].iter().all(u8::is_ascii_digit)
}

/// Lexically normalises an absolute path (no filesystem access).
fn normalise(path: &Path) -> PathBuf {
    let mut normalised = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalised.pop();
            }
            other => normalised.push(other.as_os_str()),
        }
    }
    normalised
}

fn is_forbidden_prefix(path: &Path, prefix: &str) -> bool {
    let path = normalise(path);
    path == Path::new(prefix) || path.starts_with(prefix)
}

/// Production/staging protection for restore targets.
///
/// Lexical and (when possible) canonical paths are both checked, so a symlink
/// into the production data directory cannot bypass the guard.
pub fn is_protected_restore_target(path: &Path) -> bool {
    let targets = [PROD_DATA_DIR, STAGING_DATA_DIR];
    let lexical = normalise(path);
    if targets
        .iter()
        .any(|prefix| is_forbidden_prefix(&lexical, prefix))
    {
        return true;
    }
    let resolved = fs::canonicalize(path).or_else(|_| {
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        fs::canonicalize(parent).map(|parent| match path.file_name() {
            Some(name) => parent.join(name),
            None => parent,
        })
    });
    resolved.is_ok_and(|resolved| {
        targets
            .iter()
            .any(|prefix| is_forbidden_prefix(&resolved, prefix))
    })
}

/// Creates a verified online backup of `database` inside `backups_dir`.
///
/// The source is opened read-only on a dedicated rusqlite connection; the
/// single-writer pool is untouched. The artefact is written under a temporary
/// `0600` file and atomically renamed into place after the integrity check and
/// digest succeed. The retention pass then keeps the newest
/// [`BACKUP_RETENTION`] artefacts.
pub fn create_backup(database: &Path, backups_dir: &Path) -> Result<BackupOutcome, BackupError> {
    let started = Instant::now();
    if !database.is_absolute() || !backups_dir.is_absolute() {
        return Err(BackupError::new(
            BackupErrorKind::InvalidPath,
            "database and backups directory must be absolute",
        ));
    }
    if !database.is_file() {
        return Err(BackupError::new(
            BackupErrorKind::MissingDatabase,
            database.display().to_string(),
        ));
    }

    create_private_dir(backups_dir)?;
    let (file, temporary) = reserve_artefact_path(backups_dir)?;

    // Pre-create the destination with 0600 so the file never appears with the
    // process umask; SQLite then writes into that file.
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(BACKUP_FILE_MODE)
        .open(&temporary)
        .map_err(|error| BackupError::io("create temporary backup file", error))?;
    drop(created);

    let result = fill_backup(database, &temporary);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    let mut verified = match verify_database(&temporary) {
        Ok(verified) => verified,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    fs::set_permissions(&temporary, fs::Permissions::from_mode(BACKUP_FILE_MODE))
        .map_err(|error| BackupError::io("chmod backup file", error))?;
    // No extra fsync: the destination connection uses SQLite's default
    // synchronous=FULL, so sqlite3_backup_finish already flushed the artefact.
    // (Plan §4.4 budget: backup ≤200 ms on 8 MiB.)
    fs::rename(&temporary, &file).map_err(|error| BackupError::io("publish backup file", error))?;

    let bytes = fs::metadata(&file)
        .map_err(|error| BackupError::io("stat backup file", error))?
        .len();
    verified.sha256 = sha256_file(&file)?;
    let removed = apply_retention(backups_dir)?;
    let ms = elapsed_ms(started);

    info!(
        file = %file.display(),
        bytes,
        sha256 = %verified.sha256,
        integrity = %verified.integrity,
        quick_check = %verified.quick_check,
        journal_mode = %verified.journal_mode,
        retention = BACKUP_RETENTION,
        removed,
        ms,
        "database backup complete (SQLite backup API)"
    );

    Ok(BackupOutcome {
        file,
        bytes,
        sha256: verified.sha256,
        ms,
        integrity: verified.integrity,
        journal_mode: verified.journal_mode,
        removed,
    })
}

/// Copies the source database into `temporary` with the SQLite backup API.
fn fill_backup(database: &Path, temporary: &Path) -> Result<(), BackupError> {
    let source = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(BackupError::sqlite)?;
    source
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(BackupError::sqlite)?;

    let mut destination = Connection::open_with_flags(
        temporary,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(BackupError::sqlite)?;
    destination
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(BackupError::sqlite)?;

    {
        let backup = rusqlite::backup::Backup::new(&source, &mut destination)
            .map_err(|error| BackupError::new(BackupErrorKind::BackupFailed, error.to_string()))?;
        // One shot (`step(-1)`) instead of `run_to_completion`: SQLite restarts
        // a stepped backup from page 1 whenever the source is written, so a
        // continuously busy single-writer pool could starve it indefinitely.
        // A one-shot copy holds a SHARED lock for the whole (small, ≤200 ms
        // budget) copy — writers wait those few milliseconds, exactly like a
        // long read transaction, and the service is never stopped.
        let deadline = Instant::now() + BACKUP_RETRY_DEADLINE;
        loop {
            match backup.step(-1) {
                Ok(StepResult::Done) => break,
                Ok(StepResult::More) => continue,
                Ok(StepResult::Busy | StepResult::Locked) => {
                    if Instant::now() >= deadline {
                        return Err(BackupError::new(
                            BackupErrorKind::BackupFailed,
                            "source database stayed locked during the backup",
                        ));
                    }
                    std::thread::sleep(BACKUP_RETRY_PAUSE);
                }
                // `StepResult` is `#[non_exhaustive]`; a future variant must not
                // be treated as success.
                Ok(_) => {
                    return Err(BackupError::new(
                        BackupErrorKind::BackupFailed,
                        "unexpected SQLite backup step result",
                    ))
                }
                Err(error) => {
                    return Err(BackupError::new(
                        BackupErrorKind::BackupFailed,
                        error.to_string(),
                    ))
                }
            }
        }
    }
    destination
        .close()
        .map_err(|(_, error)| BackupError::sqlite(error))?;
    drop(source);
    Ok(())
}

/// Reserves an unused artefact path; collisions (same-second calls) advance the
/// timestamp by one second, keeping the documented name shape.
fn reserve_artefact_path(backups_dir: &Path) -> Result<(PathBuf, PathBuf), BackupError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BackupError::plain(BackupErrorKind::Clock))?
        .as_secs() as i64;
    for advance in 0..120 {
        let stamp = now + advance;
        let file = backups_dir.join(backup_file_name(stamp));
        if !file.exists() {
            let temporary = backups_dir.join(format!(
                ".{}{}.tmp",
                file.file_name().unwrap_or_default().to_string_lossy(),
                std::process::id()
            ));
            if !temporary.exists() {
                return Ok((file, temporary));
            }
        }
    }
    Err(BackupError::new(
        BackupErrorKind::Io,
        "could not reserve a unique backup file name",
    ))
}

/// Creates the artefact directory as `0700`.
fn create_private_dir(backups_dir: &Path) -> Result<(), BackupError> {
    fs::create_dir_all(backups_dir)
        .map_err(|error| BackupError::io("create backups directory", error))?;
    fs::set_permissions(backups_dir, fs::Permissions::from_mode(BACKUP_DIR_MODE))
        .map_err(|error| BackupError::io("chmod backups directory", error))
}

/// Deletes the oldest artefacts beyond [`BACKUP_RETENTION`].
///
/// Only files whose name matches the generated pattern are considered, so
/// unrelated files in the directory are never touched.
pub fn apply_retention(backups_dir: &Path) -> Result<usize, BackupError> {
    let mut artefacts: Vec<PathBuf> = fs::read_dir(backups_dir)
        .map_err(|error| BackupError::io("list backups directory", error))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| is_backup_name(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    artefacts.sort();
    if artefacts.len() <= BACKUP_RETENTION {
        return Ok(0);
    }
    let excess = artefacts.len() - BACKUP_RETENTION;
    let mut removed = 0;
    for old in artefacts.iter().take(excess) {
        match fs::remove_file(old) {
            Ok(()) => {
                removed += 1;
                info!(file = %old.display(), "pruned old database backup (retention)");
            }
            Err(error) => {
                warn!(file = %old.display(), %error, "could not prune old database backup");
            }
        }
    }
    Ok(removed)
}

/// Verifies one database/artefact: integrity + quick_check + digest.
pub fn verify_backup(path: &Path) -> Result<VerifyOutcome, BackupError> {
    let started = Instant::now();
    if !path.is_absolute() {
        return Err(BackupError::new(
            BackupErrorKind::InvalidPath,
            "backup path must be absolute",
        ));
    }
    if !path.is_file() {
        return Err(BackupError::new(
            BackupErrorKind::MissingBackup,
            path.display().to_string(),
        ));
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(BackupError::sqlite)?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(BackupError::sqlite)?;
    let inspected = inspect_connection(&conn)?;
    drop(conn);

    let bytes = fs::metadata(path)
        .map_err(|error| BackupError::io("stat backup file", error))?
        .len();
    let sha256 = sha256_file(path)?;
    let ms = elapsed_ms(started);
    info!(
        file = %path.display(),
        bytes,
        sha256 = %sha256,
        integrity = %inspected.integrity,
        quick_check = %inspected.quick_check,
        journal_mode = %inspected.journal_mode,
        schema_version = inspected.schema_version.unwrap_or_default(),
        ms,
        "database file verified (integrity_check + SHA-256)"
    );
    Ok(VerifyOutcome {
        file: path.to_path_buf(),
        bytes,
        sha256,
        integrity: inspected.integrity,
        quick_check: inspected.quick_check,
        journal_mode: inspected.journal_mode,
        schema_version: inspected.schema_version,
        ms,
    })
}

/// Offline restore: verify the artefact, then atomically replace the target.
///
/// Ordering is intentional and fail-closed:
/// 1. `offline` confirmation (there is no online restore path at all),
/// 2. production/staging target guard,
/// 3. backup integrity + schema + journal verification,
/// 4. target lock/sidecar check (a running writer makes the restore refuse),
/// 5. temp-file copy + fsync + atomic rename + post-restore verification.
pub fn restore_database(
    backup: &Path,
    target: &Path,
    options: &RestoreOptions,
) -> Result<RestoreOutcome, BackupError> {
    let started = Instant::now();
    if !options.offline {
        return Err(BackupError::new(
            BackupErrorKind::OfflineRequired,
            "restore requires --offline (stop the service before replacing the database)",
        ));
    }
    if !backup.is_absolute() || !target.is_absolute() {
        return Err(BackupError::new(
            BackupErrorKind::InvalidPath,
            "backup and target paths must be absolute",
        ));
    }
    if !options.allow_prod_data && is_protected_restore_target(target) {
        return Err(BackupError::new(
            BackupErrorKind::UnsafeTarget,
            target.display().to_string(),
        ));
    }
    if backup == target {
        return Err(BackupError::new(
            BackupErrorKind::InvalidPath,
            "backup and target are the same file",
        ));
    }
    if !target.is_file() {
        return Err(BackupError::new(
            BackupErrorKind::MissingDatabase,
            target.display().to_string(),
        ));
    }

    let source = verify_backup(backup)?;
    if source.integrity != "ok" {
        return Err(BackupError::plain(BackupErrorKind::IntegrityCheckFailed));
    }
    if source.quick_check != "ok" {
        return Err(BackupError::plain(BackupErrorKind::QuickCheckFailed));
    }
    if source.schema_version != Some(EXPECTED_SCHEMA_VERSION) {
        return Err(BackupError::new(
            BackupErrorKind::SchemaMismatch,
            format!("schema_version={:?}", source.schema_version),
        ));
    }
    if !source
        .journal_mode
        .eq_ignore_ascii_case(REQUIRED_JOURNAL_MODE)
    {
        return Err(BackupError::new(
            BackupErrorKind::JournalModeMismatch,
            source.journal_mode.clone(),
        ));
    }

    ensure_offline_target(target)?;
    let replaced = replace_file(backup, target)?;
    let restored = verify_database(target)?;
    if restored.integrity != "ok" || restored.quick_check != "ok" {
        return Err(BackupError::plain(BackupErrorKind::IntegrityCheckFailed));
    }
    if !restored
        .journal_mode
        .eq_ignore_ascii_case(REQUIRED_JOURNAL_MODE)
    {
        return Err(BackupError::new(
            BackupErrorKind::JournalModeMismatch,
            restored.journal_mode.clone(),
        ));
    }
    let schema_version = restored
        .schema_version
        .ok_or_else(|| BackupError::plain(BackupErrorKind::SchemaMismatch))?;
    if schema_version != EXPECTED_SCHEMA_VERSION {
        return Err(BackupError::new(
            BackupErrorKind::SchemaMismatch,
            format!("schema_version={schema_version}"),
        ));
    }
    if replaced.sha256 != source.sha256 {
        return Err(BackupError::new(
            BackupErrorKind::RestoreFailed,
            "restored file digest differs from the verified backup digest",
        ));
    }
    let ms = elapsed_ms(started);
    info!(
        target = %target.display(),
        source = %backup.display(),
        bytes = replaced.bytes,
        sha256 = %replaced.sha256,
        integrity = %restored.integrity,
        journal_mode = %restored.journal_mode,
        schema_version,
        ms,
        "offline database restore complete (verified before replacement)"
    );
    Ok(RestoreOutcome {
        target: target.to_path_buf(),
        source: backup.to_path_buf(),
        bytes: replaced.bytes,
        sha256: replaced.sha256,
        ms,
        integrity: restored.integrity,
        journal_mode: restored.journal_mode,
        schema_version,
    })
}

/// Proves the target is offline: no sidecars and no other connection holding a
/// write lock.
fn ensure_offline_target(target: &Path) -> Result<(), BackupError> {
    for suffix in ["-journal", "-wal", "-shm"] {
        let sidecar = sidecar_path(target, suffix);
        if sidecar.exists() {
            return Err(BackupError::new(
                BackupErrorKind::DatabaseInUse,
                format!("sidecar present: {}", sidecar.display()),
            ));
        }
    }
    let connection = Connection::open_with_flags(
        target,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(BackupError::sqlite)?;
    connection
        .busy_timeout(Duration::from_millis(0))
        .map_err(BackupError::sqlite)?;
    connection
        .execute_batch("PRAGMA query_only = 0; BEGIN EXCLUSIVE")
        .map_err(|error| BackupError::new(BackupErrorKind::DatabaseInUse, error.to_string()))?;
    connection
        .execute_batch("ROLLBACK")
        .map_err(|error| BackupError::new(BackupErrorKind::DatabaseInUse, error.to_string()))?;
    drop(connection);
    Ok(())
}

/// Copies the artefact bytes to a temporary file next to the target, fsyncs and
/// renames it over the target (a bounded byte-stream copy, never a filesystem
/// copy shortcut).
fn replace_file(backup: &Path, target: &Path) -> Result<FileStat, BackupError> {
    let parent = target.parent().ok_or_else(|| {
        BackupError::new(
            BackupErrorKind::InvalidPath,
            "target has no parent directory",
        )
    })?;
    let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}-restore-{}-{sequence}.tmp",
        target.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let copy_result = copy_bytes(backup, &temporary);
    if let Err(error) = copy_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) =
        fs::set_permissions(&temporary, fs::Permissions::from_mode(BACKUP_FILE_MODE))
    {
        let _ = fs::remove_file(&temporary);
        return Err(BackupError::io("chmod restored database", error));
    }
    if let Err(error) = sync_file(&temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(BackupError::io("replace database atomically", error));
    }
    sync_dir(parent);
    let bytes = fs::metadata(target)
        .map_err(|error| BackupError::io("stat restored database", error))?
        .len();
    let sha256 = sha256_file(target)?;
    Ok(FileStat { bytes, sha256 })
}

struct FileStat {
    bytes: u64,
    sha256: String,
}

/// Byte copy with a bounded buffer (used by offline restore only).
fn copy_bytes(source: &Path, destination: &Path) -> Result<(), BackupError> {
    let input = File::open(source).map_err(|error| BackupError::io("open backup file", error))?;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(BACKUP_FILE_MODE)
        .open(destination)
        .map_err(|error| BackupError::io("create restore temp file", error))?;
    let mut reader = BufReader::with_capacity(COPY_BUFFER, input);
    let mut writer = BufWriter::with_capacity(COPY_BUFFER, output);
    io::copy(&mut reader, &mut writer)
        .map_err(|error| BackupError::io("copy backup to restore temp file", error))?;
    writer
        .flush()
        .map_err(|error| BackupError::io("flush restore temp file", error))?;
    let file = writer
        .into_inner()
        .map_err(|error| BackupError::io("finish restore temp file", error.into_error()))?;
    file.sync_all()
        .map_err(|error| BackupError::io("fsync restore temp file", error))?;
    Ok(())
}

/// Verifies an already-filled database file and closes the connection.
fn verify_database(path: &Path) -> Result<VerifyOutcome, BackupError> {
    let started = Instant::now();
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(BackupError::sqlite)?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(BackupError::sqlite)?;
    let inspected = inspect_connection(&conn)?;
    drop(conn);
    let bytes = fs::metadata(path)
        .map_err(|error| BackupError::io("stat database file", error))?
        .len();
    let sha256 = sha256_file(path)?;
    Ok(VerifyOutcome {
        file: path.to_path_buf(),
        bytes,
        sha256,
        integrity: inspected.integrity,
        quick_check: inspected.quick_check,
        journal_mode: inspected.journal_mode,
        schema_version: inspected.schema_version,
        ms: elapsed_ms(started),
    })
}

struct Inspection {
    integrity: String,
    quick_check: String,
    journal_mode: String,
    schema_version: Option<i64>,
}

/// Runs the read-only integrity/journal/schema checks on one connection.
fn inspect_connection(conn: &Connection) -> Result<Inspection, BackupError> {
    let integrity = pragma_text(conn, "integrity_check")?;
    if integrity != "ok" {
        return Err(BackupError::new(
            BackupErrorKind::IntegrityCheckFailed,
            format!("integrity_check={integrity}"),
        ));
    }
    let quick_check = pragma_text(conn, "quick_check")?;
    if quick_check != "ok" {
        return Err(BackupError::new(
            BackupErrorKind::QuickCheckFailed,
            format!("quick_check={quick_check}"),
        ));
    }
    let journal_mode = pragma_text(conn, "journal_mode")?;
    let schema_version = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get::<_, i64>(0)
        })
        .optional()
        .map_err(BackupError::sqlite)?;
    Ok(Inspection {
        integrity,
        quick_check,
        journal_mode,
        schema_version,
    })
}

/// Reads a text-valued PRAGMA. The name is a static internal constant.
fn pragma_text(conn: &Connection, name: &str) -> Result<String, BackupError> {
    conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, String>(0))
        .map_err(|error| BackupError::new(BackupErrorKind::IntegrityCheckFailed, error.to_string()))
}

/// Streams the SHA-256 of a file without loading it into memory.
pub fn sha256_file(path: &Path) -> Result<String, BackupError> {
    let file = File::open(path).map_err(|error| BackupError::io("open file for digest", error))?;
    let mut reader = BufReader::with_capacity(COPY_BUFFER, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| BackupError::io("read file for digest", error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize().as_slice()))
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

fn sync_file(path: &Path) -> Result<(), BackupError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| BackupError::io("fsync file", error))
}

/// Best-effort directory fsync so a rename survives a crash. Failure to open or
/// sync a directory is not fatal for the data file itself.
fn sync_dir(path: &Path) {
    if let Err(error) = File::open(path).and_then(|dir| dir.sync_all()) {
        warn!(directory = %path.display(), %error, "could not fsync backup directory");
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// `--help` text for the maintenance CLI.
pub const CLI_USAGE: &str = "\
lampa-server backup [--database PATH] [--backups-dir PATH]
lampa-server check --backup PATH
lampa-server restore --offline --backup PATH --database PATH [--allow-prod-data]

  backup   Online SQLite backup API copy into <backups-dir>/database-<UTC>.sqlite
           (0600, dir 0700, retention 14). The source is opened read-only.
  check    integrity_check + quick_check + SHA-256 of one database/artefact file.
  restore  OFFLINE replacement of the target: stop the service, run with
           --offline, then start it again. The target is replaced only after the
           artefact passed verification. /opt/lampa/data and
           /opt/lampa/staging-data require --allow-prod-data.";

/// One successful CLI invocation, ready for JSON serialisation.
#[derive(Debug, Clone)]
pub struct CliReport {
    /// Subcommand name (`backup`/`check`/`restore`).
    pub command: &'static str,
    /// Machine-readable report printed on stdout by [`run_cli`].
    pub json: serde_json::Value,
}

/// True when the process arguments select the maintenance CLI.
///
/// Wired at the top of `main` as:
/// ```ignore
/// let args: Vec<String> = std::env::args().skip(1).collect();
/// if db::backup::is_cli_invocation(&args) {
///     std::process::exit(db::backup::run_cli(&args));
/// }
/// ```
pub fn is_cli_invocation(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some("backup" | "restore" | "check")
    )
}

/// Runs one maintenance CLI invocation and returns the process exit code.
///
/// `0` success, `1` operational failure, `2` usage error. A JSON report goes to
/// stdout on success and `{"ok":false,"error":"<code>"}` to stderr on failure;
/// tracing logs (integrity + SHA-256 evidence) always go to stderr.
pub fn run_cli(args: &[String]) -> i32 {
    init_cli_tracing();
    match cli(args) {
        Ok(report) => {
            info!(command = report.command, "maintenance CLI succeeded");
            println!("{report_json}", report_json = report.json);
            0
        }
        Err(error) => {
            warn!(
                code = error.kind().code(),
                detail = error.detail().unwrap_or_default(),
                "maintenance CLI failed"
            );
            eprintln!(
                "{}",
                serde_json::json!({ "ok": false, "error": error.kind().code() })
            );
            if error.kind() == BackupErrorKind::Usage {
                eprintln!("{CLI_USAGE}");
                2
            } else {
                1
            }
        }
    }
}

/// Executes one maintenance CLI invocation without printing anything.
///
/// This is the testable core of the CLI; [`run_cli`] adds tracing, stdout/stderr
/// and exit codes. Errors use [`BackupErrorKind::Usage`] for command-line
/// mistakes.
pub fn cli(args: &[String]) -> Result<CliReport, BackupError> {
    let command = args
        .first()
        .map(String::as_str)
        .ok_or_else(|| usage_error("missing subcommand"))?;
    let rest = &args[1..];
    match command {
        "backup" => cli_backup(rest),
        "check" => cli_check(rest),
        "restore" => cli_restore(rest),
        other => Err(usage_error(format!("unknown subcommand: {other}"))),
    }
}

fn cli_backup(args: &[String]) -> Result<CliReport, BackupError> {
    let parsed = parse_flags(args, FlagSet::Backup)?;
    let database = parsed.database.unwrap_or_else(default_cli_database_path);
    let backups_dir = parsed
        .backups_dir
        .unwrap_or_else(|| default_backups_dir(&database));
    let outcome = create_backup(&database, &backups_dir)?;
    Ok(CliReport {
        command: "backup",
        json: serde_json::json!({
            "ok": true,
            "command": "backup",
            "file": outcome.file.to_string_lossy(),
            "bytes": outcome.bytes,
            "sha256": outcome.sha256,
            "ms": outcome.ms,
            "integrity": outcome.integrity,
            "journal_mode": outcome.journal_mode,
            "removed": outcome.removed,
        }),
    })
}

fn cli_check(args: &[String]) -> Result<CliReport, BackupError> {
    let parsed = parse_flags(args, FlagSet::Check)?;
    let backup = parsed
        .backup
        .ok_or_else(|| usage_error("check requires --backup PATH"))?;
    let verified = verify_backup(&backup)?;
    Ok(CliReport {
        command: "check",
        json: serde_json::json!({
            "ok": true,
            "command": "check",
            "file": verified.file.to_string_lossy(),
            "bytes": verified.bytes,
            "sha256": verified.sha256,
            "ms": verified.ms,
            "integrity": verified.integrity,
            "quick_check": verified.quick_check,
            "journal_mode": verified.journal_mode,
            "schema_version": verified.schema_version,
        }),
    })
}

fn cli_restore(args: &[String]) -> Result<CliReport, BackupError> {
    let parsed = parse_flags(args, FlagSet::Restore)?;
    let backup = parsed
        .backup
        .ok_or_else(|| usage_error("restore requires --backup PATH"))?;
    let database = parsed
        .database
        .ok_or_else(|| usage_error("restore requires --database PATH"))?;
    if !parsed.offline {
        return Err(usage_error(
            "restore is offline only: stop the service and pass --offline",
        ));
    }
    let outcome = restore_database(
        &backup,
        &database,
        &RestoreOptions {
            offline: true,
            allow_prod_data: parsed.allow_prod_data,
        },
    )?;
    Ok(CliReport {
        command: "restore",
        json: serde_json::json!({
            "ok": true,
            "command": "restore",
            "target": outcome.target.to_string_lossy(),
            "source": outcome.source.to_string_lossy(),
            "bytes": outcome.bytes,
            "sha256": outcome.sha256,
            "ms": outcome.ms,
            "integrity": outcome.integrity,
            "journal_mode": outcome.journal_mode,
            "schema_version": outcome.schema_version,
        }),
    })
}

/// Default database path for `backup` when `--database` is omitted.
///
/// Mirrors the server default: `DATABASE_URL` when it holds a valid absolute
/// `sqlite:` URL, otherwise `/data/database.sqlite`.
fn default_cli_database_path() -> PathBuf {
    std::env::var("DATABASE_URL")
        .ok()
        .and_then(|url| database_path_from_url(&url))
        .unwrap_or_else(|| PathBuf::from("/data/database.sqlite"))
}

#[derive(Debug, Default)]
struct ParsedFlags {
    database: Option<PathBuf>,
    backups_dir: Option<PathBuf>,
    backup: Option<PathBuf>,
    offline: bool,
    allow_prod_data: bool,
}

#[derive(Debug, Clone, Copy)]
enum FlagSet {
    Backup,
    Check,
    Restore,
}

fn parse_flags(args: &[String], allowed: FlagSet) -> Result<ParsedFlags, BackupError> {
    let mut parsed = ParsedFlags::default();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        let mut value = |flag: &str| -> Result<PathBuf, BackupError> {
            let value = args
                .get(index)
                .ok_or_else(|| usage_error(format!("{flag} requires a value")))?;
            index += 1;
            Ok(PathBuf::from(value))
        };
        match flag {
            "--database" if matches!(allowed, FlagSet::Backup | FlagSet::Restore) => {
                parsed.database = Some(value("--database")?);
            }
            "--backups-dir" if matches!(allowed, FlagSet::Backup) => {
                parsed.backups_dir = Some(value("--backups-dir")?);
            }
            "--backup" if matches!(allowed, FlagSet::Check | FlagSet::Restore) => {
                parsed.backup = Some(value("--backup")?);
            }
            "--offline" if matches!(allowed, FlagSet::Restore) => parsed.offline = true,
            "--allow-prod-data" if matches!(allowed, FlagSet::Restore) => {
                parsed.allow_prod_data = true;
            }
            other => return Err(usage_error(format!("unexpected argument: {other}"))),
        }
    }
    Ok(parsed)
}

fn usage_error(detail: impl Into<String>) -> BackupError {
    BackupError::new(BackupErrorKind::Usage, detail)
}

fn init_cli_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::TempDb;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Serialises the two heavy 8 MiB timing tests. Both are `#[ignore]`d so
    /// the ordinary suite never measures a budget under parallel test load;
    /// the dedicated serialized step in `scripts/ci/run.sh`
    /// (`cargo test --release --bins -- --ignored --test-threads=1`) runs them
    /// one at a time. The lock remains a second guard for a manual `--ignored`
    /// run that leaves the harness parallel: a concurrent SHA-256 pass would
    /// distort the budget (production runs one backup at a time).
    static HEAVY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Timing budgets from plan §4.4. The unoptimised test profile is dominated
    /// by the soft SHA-256 backend (~350 ms per 8 MiB), so the strict product
    /// budget is asserted on optimised builds; the debug gate keeps a generous
    /// sanity bound. The heavy tests are `#[ignore]`d: the timing evidence is
    /// `cargo test --release --bins -- --ignored --test-threads=1` (step
    /// `backup-timing` of `scripts/ci/run.sh`), recorded in
    /// `docs/w1-notes/T11.md`.
    fn assert_within_budget(label: &str, ms: u64, budget_ms: u64) {
        if cfg!(debug_assertions) {
            const DEBUG_SANITY_MS: u64 = 5_000;
            assert!(
                ms <= DEBUG_SANITY_MS,
                "{label}: {ms} ms exceeded the debug sanity bound"
            );
            eprintln!(
                "{label}: {ms} ms in the debug profile \
                 (strict ≤{budget_ms} ms asserted by `cargo test --release`)"
            );
        } else {
            assert!(ms <= budget_ms, "{label}: {ms} ms (budget {budget_ms} ms)");
            eprintln!("{label}: {ms} ms (≤{budget_ms} ms)");
        }
    }

    fn permissions_mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lampa-backup-unit-{}-{}-{}",
            name,
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    async fn eight_mib_database() -> TempDb {
        let temp = TempDb::new().await;
        // 8 MiB of payload in the legacy `settings.value` column; no DDL.
        let payload = "x".repeat(8 * 1024 * 1024);
        let pool = sqlx::SqlitePool::connect(&temp.url())
            .await
            .expect("open fixture pool");
        sqlx::query("INSERT INTO settings(key, visible, value) VALUES('payload', 0, ?1)")
            .bind(payload)
            .execute(&pool)
            .await
            .expect("insert 8 MiB payload");
        pool.close().await;
        let size = fs::metadata(&temp.database_path)
            .expect("stat fixture")
            .len();
        assert!(
            size >= 8 * 1024 * 1024,
            "fixture must be at least 8 MiB, got {size} bytes"
        );
        temp
    }

    async fn setting_count(database: &TempDb, key: &str) -> i64 {
        let pool = sqlx::SqlitePool::connect(&database.url())
            .await
            .expect("open fixture pool");
        let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM settings WHERE key = ?1")
            .bind(key)
            .fetch_one(&pool)
            .await
            .expect("count settings rows");
        pool.close().await;
        count
    }

    #[test]
    fn backup_file_names_are_utc_and_sort_chronologically() {
        assert_eq!(
            backup_file_name(0),
            "database-19700101T000000Z.sqlite".to_owned()
        );
        assert_eq!(
            backup_file_name(1_767_225_600),
            "database-20260101T000000Z.sqlite".to_owned()
        );
        assert!(is_backup_name(&backup_file_name(1_767_225_600)));
        assert!(!is_backup_name("database-20260101T000000.sqlite"));
        assert!(!is_backup_name("database-20260101T000000Z.sqlite.bak"));
        assert!(!is_backup_name("other-20260101T000000Z.sqlite"));
        assert!(!is_backup_name("database-2026O101T000000Z.sqlite"));
    }

    #[test]
    fn retention_keeps_the_newest_fourteen_and_unrelated_files() {
        let dir = temp_dir("retention");
        fs::write(dir.join("keep-me.txt"), b"unrelated").expect("write unrelated");
        for day in 1..=16 {
            let name = format!("database-202601{day:02}T000000Z.sqlite");
            fs::write(dir.join(name), b"artefact").expect("write artefact");
        }
        let removed = apply_retention(&dir).expect("retention");
        assert_eq!(removed, 2);
        let mut names: Vec<String> = fs::read_dir(&dir)
            .expect("list")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 15, "14 artefacts + the unrelated file");
        assert!(dir.join("keep-me.txt").exists());
        assert!(!dir.join("database-20260101T000000Z.sqlite").exists());
        assert!(!dir.join("database-20260102T000000Z.sqlite").exists());
        assert!(dir.join("database-20260116T000000Z.sqlite").exists());
        assert_eq!(apply_retention(&dir).expect("idempotent"), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn backup_creates_a_private_verified_artefact() {
        let temp = TempDb::new().await;
        let source = temp.database_path.clone();
        let before = fs::read(&source).expect("read source");
        let backups = temp.path("backups");

        let outcome = create_backup(&source, &backups).expect("backup");

        assert_eq!(permissions_mode(&backups), BACKUP_DIR_MODE);
        assert_eq!(permissions_mode(&outcome.file), BACKUP_FILE_MODE);
        let name = outcome
            .file
            .file_name()
            .expect("file name")
            .to_string_lossy();
        assert!(is_backup_name(&name), "unexpected name: {name}");
        assert_eq!(outcome.integrity, "ok");
        assert_eq!(outcome.journal_mode, "delete");
        assert_eq!(outcome.removed, 0);
        assert_eq!(
            outcome.sha256,
            sha256_file(&outcome.file).expect("digest artefact")
        );
        assert_eq!(outcome.bytes, fs::metadata(&outcome.file).unwrap().len());
        // The source is opened read-only and must be untouched by a backup.
        assert_eq!(fs::read(&source).expect("re-read source"), before);
        assert!(!sidecar_path(&source, "-journal").exists());
        assert!(!sidecar_path(&source, "-wal").exists());

        let verified = verify_backup(&outcome.file).expect("verify artefact");
        assert_eq!(verified.sha256, outcome.sha256);
        assert_eq!(verified.schema_version, Some(EXPECTED_SCHEMA_VERSION));
        let _ = fs::remove_dir_all(backups);
    }

    // Heavy 8 MiB budget test: `#[ignore]` keeps `cargo test` deterministic.
    // Run it via `cargo test --release --bins -- --ignored --test-threads=1`
    // (scripts/ci/run.sh step `backup-timing`).
    #[tokio::test]
    #[ignore = "heavy 8 MiB timing test; run the serialized release step (scripts/ci/run.sh backup-timing)"]
    async fn backup_under_single_writer_load_is_valid_and_within_budget() {
        let _serial = HEAVY_TEST_LOCK.lock().await;
        const BUDGET_MS: u64 = 200;
        let temp = eight_mib_database().await;
        let database = temp.open().await.expect("guard accepts fixture");
        let settings = database.settings();
        let backups = temp.path("backups");
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let stop = Arc::clone(&stop);
            let settings = settings.clone();
            tokio::spawn(async move {
                let mut counter = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    settings
                        .upsert(
                            &format!("load-{}", counter % 16),
                            &format!("value-{counter}"),
                        )
                        .await
                        .expect("load write");
                    counter += 1;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                counter
            })
        };

        let mut measurements = Vec::new();
        for _ in 0..3 {
            let source = temp.database_path.clone();
            let directory = backups.clone();
            let outcome = tokio::task::spawn_blocking(move || create_backup(&source, &directory))
                .await
                .expect("backup task joins")
                .expect("backup under load");
            assert_eq!(outcome.integrity, "ok");
            assert_eq!(outcome.journal_mode, "delete");
            assert_within_budget("backup", outcome.ms, BUDGET_MS);
            measurements.push((outcome.file.clone(), outcome.sha256.clone(), outcome.ms));
        }
        stop.store(true, Ordering::Relaxed);
        let writes = writer.await.expect("writer joins");
        assert!(writes > 0, "writer must have produced concurrent load");

        for (file, sha, _) in &measurements {
            let verified = verify_backup(file).expect("loaded backup is valid");
            assert_eq!(verified.sha256, *sha);
            assert_eq!(verified.schema_version, Some(EXPECTED_SCHEMA_VERSION));
        }
        database.close().await;
        eprintln!(
            "backup timing (8 MiB, under single-writer load): {:?}",
            measurements
                .iter()
                .map(|(_, _, ms)| *ms)
                .collect::<Vec<_>>()
        );
    }

    // Heavy 8 MiB budget test: `#[ignore]` keeps `cargo test` deterministic.
    // Run it via `cargo test --release --bins -- --ignored --test-threads=1`
    // (scripts/ci/run.sh step `backup-timing`).
    #[tokio::test]
    #[ignore = "heavy 8 MiB timing test; run the serialized release step (scripts/ci/run.sh backup-timing)"]
    async fn offline_restore_replaces_target_only_after_verification() {
        let _serial = HEAVY_TEST_LOCK.lock().await;
        const BUDGET_MS: u64 = 300;
        let source = eight_mib_database().await;
        let source_backup_dir = source.path("backups");
        let artefact = create_backup(&source.database_path, &source_backup_dir)
            .expect("backup")
            .file;
        let artefact_sha = sha256_file(&artefact).expect("artefact digest");

        let target = TempDb::new().await;
        target.open().await.expect("target opens").close().await;
        crate::db::test_support::exec_sql(
            &target.database_path,
            "INSERT INTO settings(key, value, visible) VALUES('marker', 'must-disappear', 0)",
        )
        .await;
        assert_ne!(
            sha256_file(&target.database_path).expect("target digest"),
            artefact_sha
        );

        let outcome = restore_database(
            &artefact,
            &target.database_path,
            &RestoreOptions {
                offline: true,
                allow_prod_data: false,
            },
        )
        .expect("offline restore");

        assert_eq!(outcome.sha256, artefact_sha);
        assert_eq!(outcome.integrity, "ok");
        assert_eq!(outcome.journal_mode, "delete");
        assert_eq!(outcome.schema_version, EXPECTED_SCHEMA_VERSION);
        assert_within_budget("restore", outcome.ms, BUDGET_MS);
        assert_eq!(permissions_mode(&target.database_path), BACKUP_FILE_MODE);
        assert_eq!(
            setting_count(&target, "marker").await,
            0,
            "restore must replace the target with the artefact content"
        );
        assert!(!sidecar_path(&target.database_path, "-journal").exists());
        eprintln!("restore timing (8 MiB): {} ms", outcome.ms);
    }

    #[tokio::test]
    async fn restore_refuses_prod_targets_corruption_and_online_state() {
        let source = TempDb::new().await;
        let backup_dir = source.path("backups");
        let artefact = create_backup(&source.database_path, &backup_dir)
            .expect("backup")
            .file;
        let target = TempDb::new().await;
        let target_path = target.database_path.clone();

        for protected in [
            "/opt/lampa/data/backup-guard-test.sqlite",
            "/opt/lampa/staging-data/backup-guard-test.sqlite",
        ] {
            let error = restore_database(
                &artefact,
                Path::new(protected),
                &RestoreOptions {
                    offline: true,
                    allow_prod_data: false,
                },
            )
            .expect_err("protected target must be refused before any filesystem access");
            assert_eq!(error.kind(), BackupErrorKind::UnsafeTarget, "{protected}");
        }

        let error = restore_database(
            &artefact,
            &target_path,
            &RestoreOptions {
                offline: false,
                allow_prod_data: false,
            },
        )
        .expect_err("online restore must be refused");
        assert_eq!(error.kind(), BackupErrorKind::OfflineRequired);

        // A hot journal proves the writer did not stop cleanly.
        let journal = sidecar_path(&target_path, "-journal");
        fs::write(&journal, b"hot").expect("create fake hot journal");
        let before = fs::read(&target_path).expect("read target");
        let error = restore_database(
            &artefact,
            &target_path,
            &RestoreOptions {
                offline: true,
                allow_prod_data: false,
            },
        )
        .expect_err("hot journal must refuse restore");
        assert_eq!(error.kind(), BackupErrorKind::DatabaseInUse);
        assert_eq!(fs::read(&target_path).expect("unchanged target"), before);
        fs::remove_file(&journal).expect("remove fake journal");

        let corrupt = target.path("corrupt.sqlite");
        fs::write(&corrupt, vec![b'x'; 4096]).expect("write corrupt artefact");
        let error = restore_database(
            &corrupt,
            &target_path,
            &RestoreOptions {
                offline: true,
                allow_prod_data: false,
            },
        )
        .expect_err("corrupt artefact must refuse restore");
        assert_eq!(error.kind(), BackupErrorKind::IntegrityCheckFailed);
        assert_eq!(fs::read(&target_path).expect("unchanged target"), before);

        let error = verify_backup(&corrupt).expect_err("corrupt artefact");
        assert_eq!(error.kind(), BackupErrorKind::IntegrityCheckFailed);
        assert!(!error.to_string().contains("PRAGMA"));
        assert!(!error.to_string().contains("SELECT"));
    }

    #[tokio::test]
    async fn restore_refuses_a_non_v7_artefact() {
        let source = TempDb::new().await;
        let backup_dir = source.path("backups");
        let artefact = create_backup(&source.database_path, &backup_dir)
            .expect("backup")
            .file;
        let connection = Connection::open(&artefact).expect("open artefact");
        connection
            .execute("UPDATE schema_version SET version = 6", [])
            .expect("downgrade schema");
        connection.close().expect("close artefact");

        let target = TempDb::new().await;
        let error = restore_database(
            &artefact,
            &target.database_path,
            &RestoreOptions {
                offline: true,
                allow_prod_data: false,
            },
        )
        .expect_err("schema 6 must be refused");
        assert_eq!(error.kind(), BackupErrorKind::SchemaMismatch);

        let error = restore_database(
            &artefact,
            &target.database_path,
            &RestoreOptions {
                offline: true,
                allow_prod_data: true,
            },
        )
        .expect_err("allow_prod_data does not unlock schema verification");
        assert_eq!(error.kind(), BackupErrorKind::SchemaMismatch);
    }

    #[tokio::test]
    async fn database_path_is_parsed_only_from_absolute_mode_rw_urls() {
        assert_eq!(
            database_path_from_url("sqlite:/data/database.sqlite?mode=rw"),
            Some(PathBuf::from("/data/database.sqlite"))
        );
        assert_eq!(
            default_backups_dir(Path::new("/data/database.sqlite")),
            PathBuf::from("/data/backups")
        );
        assert_eq!(
            database_path_from_url("sqlite:relative.sqlite?mode=rw"),
            None
        );
        assert_eq!(database_path_from_url("postgres://x"), None);
    }

    #[test]
    fn protected_targets_are_lexically_detected_without_touching_the_disk() {
        assert!(is_protected_restore_target(Path::new(
            "/opt/lampa/data/database.sqlite"
        )));
        assert!(is_protected_restore_target(Path::new(
            "/opt/lampa/data/backups/database-20260101T000000Z.sqlite"
        )));
        assert!(is_protected_restore_target(Path::new(
            "/opt/lampa/staging-data/database.sqlite"
        )));
        assert!(is_protected_restore_target(Path::new(
            "/opt/lampa/data/../data/database.sqlite"
        )));
        assert!(!is_protected_restore_target(Path::new(
            "/tmp/lampa-restore-target.sqlite"
        )));
        assert!(!is_protected_restore_target(Path::new(
            "/opt/lampa/data-other/database.sqlite"
        )));
    }

    fn cli_args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[tokio::test]
    async fn cli_backup_check_restore_roundtrip_reports_evidence() {
        let source = TempDb::new().await;
        let backups = source.path("backups");
        let source_path = source.database_path.to_string_lossy().into_owned();
        let backups_path = backups.to_string_lossy().into_owned();

        let backup_report = cli(&cli_args(&[
            "backup",
            "--database",
            &source_path,
            "--backups-dir",
            &backups_path,
        ]))
        .expect("backup command");
        assert_eq!(backup_report.command, "backup");
        let file = backup_report.json["file"]
            .as_str()
            .expect("file field")
            .to_owned();
        assert!(Path::new(&file).is_file());
        assert!(is_backup_name(
            Path::new(&file)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        ));
        assert_eq!(backup_report.json["ok"], true);
        assert_eq!(backup_report.json["integrity"], "ok");
        assert_eq!(backup_report.json["journal_mode"], "delete");
        assert_eq!(backup_report.json["removed"], 0);
        let sha = backup_report.json["sha256"]
            .as_str()
            .expect("sha")
            .to_owned();
        assert_eq!(sha, sha256_file(Path::new(&file)).expect("digest"));
        assert_eq!(permissions_mode(Path::new(&file)), BACKUP_FILE_MODE);
        assert_eq!(permissions_mode(&backups), BACKUP_DIR_MODE);

        let check_report = cli(&cli_args(&["check", "--backup", &file])).expect("check command");
        assert_eq!(check_report.json["ok"], true);
        assert_eq!(check_report.json["sha256"], sha);
        assert_eq!(check_report.json["schema_version"], 7);
        assert_eq!(check_report.json["quick_check"], "ok");

        // `restore` refuses to run without the explicit offline marker.
        let target = TempDb::new().await;
        let target_path = target.database_path.to_string_lossy().into_owned();
        let error = cli(&cli_args(&[
            "restore",
            "--backup",
            &file,
            "--database",
            &target_path,
        ]))
        .expect_err("online restore must be refused");
        assert_eq!(error.kind(), BackupErrorKind::Usage);

        // Offline restore swaps the target for the verified artefact.
        let restore_report = cli(&cli_args(&[
            "restore",
            "--offline",
            "--backup",
            &file,
            "--database",
            &target_path,
        ]))
        .expect("offline restore");
        assert_eq!(restore_report.json["ok"], true);
        assert_eq!(restore_report.json["sha256"], sha);
        assert_eq!(restore_report.json["schema_version"], 7);
        assert_eq!(
            sha256_file(&target.database_path).expect("target digest"),
            sha
        );

        let _ = fs::remove_dir_all(backups);
    }

    #[tokio::test]
    async fn cli_restore_refuses_prod_targets_without_the_explicit_flag() {
        let source = TempDb::new().await;
        let backups = source.path("backups");
        let artefact = create_backup(&source.database_path, &backups)
            .expect("backup")
            .file;
        let artefact_path = artefact.to_string_lossy().into_owned();

        let error = cli(&cli_args(&[
            "restore",
            "--offline",
            "--backup",
            &artefact_path,
            "--database",
            "/opt/lampa/data/database.sqlite",
        ]))
        .expect_err("prod target guard");
        assert_eq!(error.kind(), BackupErrorKind::UnsafeTarget);

        let error = cli(&cli_args(&[
            "restore",
            "--offline",
            "--backup",
            &artefact_path,
            "--database",
            "/opt/lampa/staging-data/database.sqlite",
        ]))
        .expect_err("staging target guard");
        assert_eq!(error.kind(), BackupErrorKind::UnsafeTarget);

        let _ = fs::remove_dir_all(backups);
    }

    #[test]
    fn cli_rejects_unknown_subcommands_and_flags() {
        let error = cli(&cli_args(&["frobnicate"])).expect_err("unknown subcommand");
        assert_eq!(error.kind(), BackupErrorKind::Usage);
        let error = cli(&cli_args(&["backup", "--offline"])).expect_err("flag not valid here");
        assert_eq!(error.kind(), BackupErrorKind::Usage);
        let error = cli(&cli_args(&["check"])).expect_err("missing --backup");
        assert_eq!(error.kind(), BackupErrorKind::Usage);
        assert!(!is_cli_invocation(&cli_args(&["frobnicate"])));
        assert!(is_cli_invocation(&cli_args(&["backup"])));
        assert!(is_cli_invocation(&cli_args(&["check"])));
        assert!(is_cli_invocation(&cli_args(&["restore"])));
    }
}
