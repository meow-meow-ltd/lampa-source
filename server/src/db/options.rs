//! SQLite connect options and connection-URL policy.
//!
//! The URL is validated *before* SQLx sees it. Only `mode=rw` is accepted:
//! the server opens an existing database read-write and never creates one
//! (`mode=rwc`/`mode=ro`/`mode=memory` are configuration errors). PRAGMAs are
//! applied to every pooled connection; `journal_mode` is deliberately never set
//! (W0-7: the file must stay in `delete` mode, enforced by the startup guard).

use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteSynchronous};

use crate::db::guard::{SchemaErrorKind, SchemaGuardError};

/// The server is the only writer: one pooled connection (plan §4.2, D9).
pub const MAX_CONNECTIONS: u32 = 1;
/// Keep the single connection warm from startup.
pub const MIN_CONNECTIONS: u32 = 1;
/// `PRAGMA busy_timeout` on every connection (milliseconds).
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);
/// How long a caller may wait for the single connection.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);
/// `PRAGMA cache_size` (negative = KiB).
pub const CACHE_SIZE_PRAGMA: &str = "-2000";
/// `PRAGMA temp_store` value (`FILE` = 1).
pub const TEMP_STORE_PRAGMA: &str = "FILE";

/// Validates a `sqlite:` URL and builds the documented connect options.
///
/// Required URL shape: `sqlite:<absolute path>?mode=rw` (additional `cache=...`
/// is tolerated; `immutable=true` and every other `mode` are rejected).
pub(crate) fn connect_options(url: &str) -> Result<SqliteConnectOptions, SchemaGuardError> {
    validate_url(url)?;
    let options = SqliteConnectOptions::from_str(url)
        .map_err(|_| invalid_url("cannot parse SQLite connection URL"))?;
    if options.get_filename().as_os_str().is_empty() {
        return Err(invalid_url("database path is empty"));
    }

    Ok(options
        // Enforced even after URL validation: no create, no read-only.
        .create_if_missing(false)
        .read_only(false)
        .busy_timeout(BUSY_TIMEOUT)
        // Legacy schema has zero FK constraints; orphans are normal (S2 R10).
        .foreign_keys(false)
        .synchronous(SqliteSynchronous::Full)
        .pragma("cache_size", CACHE_SIZE_PRAGMA)
        .pragma("temp_store", TEMP_STORE_PRAGMA))
    // NOTE: `journal_mode` is intentionally not set here; sqlx 0.8 leaves it
    // untouched by default. The guard asserts the persistent mode is `delete`.
}

fn invalid_url(detail: &str) -> SchemaGuardError {
    SchemaGuardError::new(SchemaErrorKind::InvalidUrl, detail)
}

/// Enforces `mode=rw` and rejects unsafe SQLite URI parameters.
fn validate_url(url: &str) -> Result<(), SchemaGuardError> {
    let rest = url
        .strip_prefix("sqlite:")
        .ok_or_else(|| invalid_url("URL must start with `sqlite:`"))?;
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };
    if path.is_empty() {
        return Err(invalid_url("database path is empty"));
    }
    if path == ":memory:" {
        return Err(invalid_url("in-memory databases are not supported"));
    }
    if !path.starts_with('/') {
        return Err(invalid_url("database path must be absolute"));
    }

    let mut mode: Option<&str> = None;
    let mut has_immutable = false;
    if let Some(query) = query {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "mode" => {
                    if mode.is_some() {
                        return Err(invalid_url("duplicate `mode` parameter"));
                    }
                    mode = Some(value);
                }
                "immutable" => has_immutable = matches!(value, "true" | "1"),
                // `cache=private|shared` only affects SQLite shared-cache mode and is
                // irrelevant for a single connection; sqlx validates the value itself.
                "cache" => {}
                _ => {
                    // Unknown parameters are rejected by sqlx later; do not silently
                    // accept anything that could change open semantics.
                    if key != "vfs" {
                        return Err(invalid_url("unsupported connection URL parameter"));
                    }
                }
            }
        }
    }

    match mode {
        Some("rw") => {}
        Some(_) => {
            return Err(invalid_url(
                "only `mode=rw` is allowed (no create, no read-only)",
            ))
        }
        None => {
            return Err(invalid_url(
                "`mode=rw` is required (the server never creates a database)",
            ))
        }
    }
    if has_immutable {
        return Err(invalid_url("`immutable=true` is not allowed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::TempDb;
    use crate::db::Database;

    #[test]
    fn accepts_the_documented_mode_rw_url() {
        let path = std::env::temp_dir().join("lampa-options-accept.sqlite");
        let url = TempDb::url_for(&path);
        let options = connect_options(&url).expect("mode=rw is valid");
        assert_eq!(options.get_filename(), path.as_path());
    }

    #[test]
    fn rejects_every_url_that_could_create_or_open_read_only() {
        let path = "/tmp/lampa-options-reject.sqlite";
        let cases = [
            format!("postgres://{path}"),
            format!("sqlite:{path}"),
            format!("sqlite:{path}?mode=ro"),
            format!("sqlite:{path}?mode=rwc"),
            format!("sqlite:{path}?mode=memory"),
            format!("sqlite:{path}?mode=rw&mode=rwc"),
            format!("sqlite:{path}?immutable=true&mode=rw"),
            format!("sqlite:{path}?mode=rw&cache=shared&unknown=1"),
            "sqlite::memory:".to_owned(),
            "sqlite:?mode=rw".to_owned(),
        ];
        for url in cases {
            let error = connect_options(&url).expect_err(&format!("must reject {url}"));
            assert_eq!(
                error.kind(),
                SchemaErrorKind::InvalidUrl,
                "unexpected kind for {url}"
            );
            // The sanitised message never echoes the URL.
            assert!(!error.to_string().contains("mode="));
        }
    }

    #[tokio::test]
    async fn invalid_url_is_rejected_before_touching_the_filesystem() {
        let temp = TempDb::empty();
        let missing = temp.path("missing.sqlite");
        let error = Database::open(&format!("sqlite:{}?mode=rwc", missing.display()), false)
            .await
            .expect_err("rwc must be refused");
        assert_eq!(error.kind(), SchemaErrorKind::InvalidUrl);
        assert!(!missing.exists());
    }
}
