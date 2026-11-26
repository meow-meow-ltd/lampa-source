//! Maintenance API route group — owner: T11.
//!
//! W1 addition (not part of the 27 T03 fixtures):
//! `POST /api/admin/maintenance/backup`. The admin cookie (`auth_token`) is
//! enforced by [`crate::http::middleware::auth_guard`] before this handler
//! runs; the handler itself never accepts a path or file name from the client,
//! so no path traversal is possible.
//!
//! The handler calls [`crate::db::backup::create_backup`] on a blocking thread
//! (rusqlite is synchronous). The SQLite backup API uses its own read-only
//! connection, so the single-writer SQLx pool stays available to the server
//! while the copy runs.
//!
//! Response contract:
//!
//! * `200 {"ok":true,"file":"/data/backups/database-…Z.sqlite","bytes":N,
//!   "sha256":"…","ms":N}`;
//! * `503 {"ok":false,"error":"db_unavailable"}` on a fail-closed server;
//! * other failures map to a stable code (`database_in_use`, `io_error`,
//!   `backup_failed`, …) with `400`/`409`/`500`/`503`; raw SQL/driver text is
//!   logged only and never serialised.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use tracing::error;

use crate::db::backup::{
    create_backup, database_path_from_url, default_backups_dir, BackupErrorKind,
};
use crate::state::AppState;

/// Mounts `POST /api/admin/maintenance/backup`.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/admin/maintenance/backup", post(backup_handler))
}

/// Creates an online backup of the configured database.
async fn backup_handler(State(state): State<AppState>) -> Response {
    if state.db().database().is_none() {
        // Fail-closed startup: no database, no backup. `/readyz` already
        // reports the sanitised guard reason; the API repeats only the code.
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "db_unavailable");
    }
    let Some(database_path) = database_path_from_url(state.config().database_url()) else {
        error!("maintenance backup: configured DATABASE_URL has no absolute sqlite path");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "invalid_database_url");
    };
    let backups_dir = default_backups_dir(&database_path);

    let result =
        tokio::task::spawn_blocking(move || create_backup(&database_path, &backups_dir)).await;

    match result {
        Ok(Ok(outcome)) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "file": outcome.file.to_string_lossy(),
                "bytes": outcome.bytes,
                "sha256": outcome.sha256,
                "ms": outcome.ms,
            })),
        )
            .into_response(),
        Ok(Err(error)) => {
            error!(
                code = error.kind().code(),
                detail = error.detail().unwrap_or_default(),
                "maintenance backup failed"
            );
            let status = status_for(error.kind());
            error_response(status, error.kind().code())
        }
        Err(join_error) => {
            error!(%join_error, "maintenance backup task failed to join");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "backup_task_failed")
        }
    }
}

/// HTTP mapping of the sanitised failure classes.
fn status_for(kind: BackupErrorKind) -> StatusCode {
    match kind {
        BackupErrorKind::MissingDatabase => StatusCode::SERVICE_UNAVAILABLE,
        BackupErrorKind::MissingBackup => StatusCode::NOT_FOUND,
        BackupErrorKind::InvalidPath
        | BackupErrorKind::UnsafeTarget
        | BackupErrorKind::OfflineRequired
        | BackupErrorKind::Usage => StatusCode::BAD_REQUEST,
        BackupErrorKind::DatabaseInUse => StatusCode::CONFLICT,
        BackupErrorKind::IntegrityCheckFailed
        | BackupErrorKind::QuickCheckFailed
        | BackupErrorKind::SchemaMismatch
        | BackupErrorKind::JournalModeMismatch
        | BackupErrorKind::Io
        | BackupErrorKind::Sqlite
        | BackupErrorKind::BackupFailed
        | BackupErrorKind::RestoreFailed
        | BackupErrorKind::Clock => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error_response(status: StatusCode, code: &str) -> Response {
    (
        status,
        Json(json!({
            "ok": false,
            "error": code,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::db::test_support::TempDb;
    use crate::db::Database;
    use crate::state::DbState;

    fn config_for(database_url: &str) -> Config {
        let url = database_url.to_owned();
        Config::from_lookup(move |key| match key {
            "DATABASE_URL" => Some(url.clone()),
            _ => None,
        })
        .expect("test config")
    }

    async fn state_for(database: Database, database_url: &str) -> AppState {
        AppState::new(config_for(database_url), DbState::Ready(database))
    }

    async fn post(state: AppState, cookie: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/admin/maintenance/backup");
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        let response = crate::http::router(state)
            .oneshot(builder.body(Body::empty()).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 20).await.expect("body");
        let json = serde_json::from_slice(&body).expect("json body");
        (status, json)
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    /// Seeds the legacy `settings.admin_token` row so the real T10 admin guard
    /// accepts the cookie. The value is synthetic and lives only in the temp DB.
    async fn seed_admin_token(temp: &TempDb, token: &str) {
        crate::db::test_support::exec_sql(
            &temp.database_path,
            &format!(
                "INSERT INTO settings(key, value, visible) VALUES('admin_token', '{token}', 0)"
            ),
        )
        .await;
    }

    #[tokio::test]
    async fn backup_with_admin_cookie_returns_digest_and_private_file() {
        let temp = TempDb::new().await;
        seed_admin_token(&temp, "test-admin-token").await;
        let database = temp.open().await.expect("v7 fixture passes the guard");
        let url = temp.url();
        let state = state_for(database.clone(), &url).await;

        let (status, body) = post(state, Some("auth_token=test-admin-token")).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body["ok"], true);
        assert!(body["bytes"].as_u64().unwrap() > 0);
        assert!(body["ms"].is_u64());
        let file = body["file"].as_str().expect("file");
        assert!(Path::new(file).is_file());
        assert!(file.contains("/backups/database-"));
        assert_eq!(
            body["sha256"].as_str().expect("sha256"),
            crate::db::backup::sha256_file(Path::new(file)).expect("digest")
        );
        let backups_dir = temp.path("backups");
        assert_eq!(mode(&backups_dir), 0o700);
        assert_eq!(mode(Path::new(file)), 0o600);

        let serialised = body.to_string();
        for forbidden in ["PRAGMA", "SELECT", "INSERT", "sqlite3"] {
            assert!(
                !serialised.contains(forbidden),
                "API body must not leak driver/SQL text: {serialised}"
            );
        }
        database.close().await;
    }

    #[tokio::test]
    async fn missing_and_non_admin_credentials_are_rejected_before_the_handler() {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture passes the guard");
        let url = temp.url();

        let (status, body) = post(state_for(database.clone(), &url).await, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({ "error": "token_required" }));

        let (status, body) = post(
            state_for(database.clone(), &url).await,
            Some("device_token=x"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({ "error": "token_required" }));

        let (status, body) =
            post(state_for(database.clone(), &url).await, Some("auth_token=")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({ "error": "invalid_token" }));
        database.close().await;
    }

    #[tokio::test]
    async fn fail_closed_database_handler_returns_503_without_driver_text() {
        let temp = TempDb::empty();
        let missing = temp.path("missing.sqlite");
        let url = TempDb::url_for(&missing);
        let error = Database::open(&url, false)
            .await
            .expect_err("missing database fails the guard");
        let state = AppState::new(config_for(&url), DbState::Unavailable(error));

        // The real auth guard runs first and fails closed (401) because T10
        // cannot validate an admin token without the pool; the handler itself
        // still owns the sanitised 503 branch (defence in depth, no driver text).
        let response = backup_handler(State(state)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(body, json!({ "ok": false, "error": "db_unavailable" }));
        assert!(!body.to_string().contains("sqlite"));
        assert!(!missing.exists());
    }

    #[tokio::test]
    async fn missing_database_file_after_startup_is_mapped_without_sql_text() {
        let temp = TempDb::new().await;
        seed_admin_token(&temp, "test-admin-token").await;
        let database = temp.open().await.expect("v7 fixture passes the guard");
        let url = temp.url();
        // The pooled writer keeps its open inode (so the admin guard still
        // works), but a new backup connection cannot open the unlinked path.
        std::fs::remove_file(&temp.database_path).expect("remove source file");
        let state = state_for(database.clone(), &url).await;

        let (status, body) = post(state, Some("auth_token=test-admin-token")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], "missing_database");
        let serialised = body.to_string();
        assert!(!serialised.contains("PRAGMA"));
        assert!(!serialised.contains("SELECT"));
        assert!(!serialised.contains("sqlite3"));
        database.close().await;
    }
}
