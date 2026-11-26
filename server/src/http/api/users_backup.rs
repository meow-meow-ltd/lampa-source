//! User backup route group — owner: T09.
//!
//! Frozen paths (T03 golden fixtures 24–25): `POST /api/users/backup/export`
//! (write) and `GET /api/users/backup/import` (read) — the legacy inverted
//! naming is part of the contract, not a bug. Device cookie (`device_token`)
//! is enforced by [`crate::http::middleware::auth_guard`] before these handlers
//! run.
//!
//! Ported from `backend/routes/backup.js`:
//!
//! * `POST export` requires `{"data":"<JSON text>"}`; a missing/empty string is
//!   `400 {"secuses":false,"text":"Missing data"}` and non-JSON text is
//!   `400 {"secuses":false,"text":"Invalid JSON"}`. Valid text is stored
//!   **verbatim** in `settings.backup_latest` (visible forced to `0`) and the
//!   answer is `{"secuses":true}`;
//! * `GET import` returns `{"secuses":true,"data":<parsed blob|null>}`; a
//!   missing row, an empty value or an unparseable stored blob all yield
//!   `data: null` (legacy `JSON.parse` guard).
//!
//! So the direction is inverted relative to the route names: `export` writes,
//! `import` reads. Failures never leak SQL text.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::Value;

use crate::profiles::users_backup::UsersBackupRepo;
use crate::state::AppState;

use super::profiles::read_fields;

/// Mounts the two user-backup routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/users/backup/export", post(export))
        .route("/api/users/backup/import", get(import))
}

#[derive(Debug, Serialize)]
struct SecusesResponse {
    secuses: bool,
}

#[derive(Debug, Serialize)]
struct ImportResponse {
    secuses: bool,
    data: Value,
}

#[derive(Debug, Serialize)]
struct LegacyTextError {
    secuses: bool,
    text: &'static str,
}

#[derive(Debug, Serialize)]
struct LegacyError {
    secuses: bool,
    error: &'static str,
}

/// `POST /api/users/backup/export` — the legacy **write** side.
async fn export(State(state): State<AppState>, request: Request) -> Response {
    let Some(database) = state.db().database() else {
        return db_unavailable();
    };
    let fields = read_fields(request).await;
    let raw = fields
        .field("data")
        .and_then(|value| value.as_string())
        .unwrap_or_default();
    if raw.is_empty() {
        return legacy_text_error(StatusCode::BAD_REQUEST, "Missing data");
    }
    if serde_json::from_str::<Value>(&raw).is_err() {
        return legacy_text_error(StatusCode::BAD_REQUEST, "Invalid JSON");
    }

    let repo = UsersBackupRepo::new(database.pool().clone());
    match repo.save(&raw).await {
        Ok(()) => json_response(StatusCode::OK, SecusesResponse { secuses: true }),
        Err(error) => database_error("users_backup.save", &error),
    }
}

/// `GET /api/users/backup/import` — the legacy **read** side.
async fn import(State(state): State<AppState>) -> Response {
    let Some(database) = state.db().database() else {
        return db_unavailable();
    };
    let repo = UsersBackupRepo::new(database.pool().clone());
    let data = match repo.load().await {
        Ok(Some(raw)) if !raw.is_empty() => match serde_json::from_str::<Value>(&raw) {
            Ok(value) => value,
            Err(_) => Value::Null,
        },
        Ok(_) => Value::Null,
        Err(error) => return database_error("users_backup.load", &error),
    };
    json_response(
        StatusCode::OK,
        ImportResponse {
            secuses: true,
            data,
        },
    )
}

fn json_response<T: Serialize>(status: StatusCode, payload: T) -> Response {
    (status, Json(payload)).into_response()
}

fn legacy_text_error(status: StatusCode, text: &'static str) -> Response {
    json_response(
        status,
        LegacyTextError {
            secuses: false,
            text,
        },
    )
}

/// `500 {"secuses":false,"error":"database_error"}` — never SQL text.
fn database_error(operation: &'static str, error: &crate::db::DbError) -> Response {
    tracing::error!(
        operation,
        code = error.code(),
        error = ?error,
        "user backup API database failure"
    );
    json_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        LegacyError {
            secuses: false,
            error: "database_error",
        },
    )
}

/// Fail-closed server (startup guard failed): no reads or writes.
fn db_unavailable() -> Response {
    json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        LegacyError {
            secuses: false,
            error: "db_unavailable",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes as read_body_bytes, Body};
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::db::test_support::TempDb;
    use crate::db::Database;
    use crate::state::DbState;

    async fn ready() -> (TempDb, Database, AppState) {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        let state = AppState::new(
            Config::from_lookup(|_| None).expect("test config"),
            DbState::Ready(database.clone()),
        );
        (temp, database, state)
    }

    async fn unavailable() -> (TempDb, AppState) {
        let temp = TempDb::empty();
        let missing = temp.path("missing.sqlite");
        let error = Database::open(&TempDb::url_for(&missing), false)
            .await
            .expect_err("missing database fails the guard");
        let state = AppState::new(
            Config::from_lookup(|_| None).expect("test config"),
            DbState::Unavailable(error),
        );
        (temp, state)
    }

    async fn send(
        state: AppState,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = HttpRequest::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let request = builder
            .body(match body {
                Some(body) => Body::from(body.to_owned()),
                None => Body::empty(),
            })
            .expect("request");
        let response = router()
            .with_state(state)
            .oneshot(request)
            .await
            .expect("response");
        let status = response.status();
        let bytes = read_body_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        (
            status,
            String::from_utf8(bytes.to_vec()).expect("utf8 body"),
        )
    }

    fn fixture(name: &str) -> Value {
        serde_json::from_str(name).expect("fixture JSON")
    }

    fn fixture_body(fixture: &Value) -> String {
        fixture["response"]["body"]
            .as_str()
            .expect("fixture body string")
            .to_owned()
    }

    fn fixture_request_body(fixture: &Value) -> String {
        serde_json::to_string(&fixture["request"]["body"]).expect("request body JSON")
    }

    async fn stored_backup(database: &Database) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT value FROM settings WHERE key = 'backup_latest'",
        )
        .fetch_optional(database.pool())
        .await
        .expect("read backup row")
        .flatten()
    }

    #[tokio::test]
    async fn fixture_24_export_and_fixture_25_import_are_byte_identical() {
        let (_temp, database, state) = ready().await;
        let request = fixture(include_str!(
            "../../../tests/fixtures/api/24_post_api_users_backup_export.json"
        ));
        let expected_import = fixture(include_str!(
            "../../../tests/fixtures/api/25_get_api_users_backup_import.json"
        ));

        // Fixture 24: POST /api/users/backup/export is the WRITE side.
        let (status, body) = send(
            state.clone(),
            "POST",
            "/api/users/backup/export",
            Some(&fixture_request_body(&request)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, fixture_body(&request));

        // The blob is stored verbatim under the legacy key.
        assert_eq!(
            stored_backup(&database).await.as_deref(),
            Some(r#"{"golden":true,"items":[1,2,3],"note":"synthetic backup"}"#)
        );

        // Fixture 25: GET /api/users/backup/import is the READ side.
        let (status, body) = send(state, "GET", "/api/users/backup/import", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, fixture_body(&expected_import));
        database.close().await;
    }

    #[tokio::test]
    async fn the_direction_is_inverted_and_import_alone_returns_null() {
        let (_temp, database, state) = ready().await;

        // No backup stored yet: import (read) answers null without writing.
        let (status, body) = send(state.clone(), "GET", "/api/users/backup/import", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true,"data":null}"#);
        assert_eq!(stored_backup(&database).await, None, "import never writes");

        // Export (write) stores; import then serves the stored blob.
        let (status, body) = send(
            state.clone(),
            "POST",
            "/api/users/backup/export",
            Some(r#"{"data":"{\"direction\":\"export-writes\"}"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true}"#);
        assert_eq!(
            stored_backup(&database).await.as_deref(),
            Some(r#"{"direction":"export-writes"}"#)
        );

        let (status, body) = send(state, "GET", "/api/users/backup/import", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"secuses":true,"data":{"direction":"export-writes"}}"#
        );
        database.close().await;
    }

    #[tokio::test]
    async fn export_validates_data_and_json() {
        let (_temp, database, state) = ready().await;

        for (body, expected) in [
            (r#"{}"#, r#"{"secuses":false,"text":"Missing data"}"#),
            (
                r#"{"data":""}"#,
                r#"{"secuses":false,"text":"Missing data"}"#,
            ),
            (
                r#"{"data":123}"#,
                r#"{"secuses":false,"text":"Missing data"}"#,
            ),
            (
                r#"{"data":"not json"}"#,
                r#"{"secuses":false,"text":"Invalid JSON"}"#,
            ),
            (
                r#"{"data":"{\"broken\":"}"#,
                r#"{"secuses":false,"text":"Invalid JSON"}"#,
            ),
        ] {
            let (status, response) = send(
                state.clone(),
                "POST",
                "/api/users/backup/export",
                Some(body),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(response, expected, "{body}");
        }
        assert_eq!(stored_backup(&database).await, None, "nothing was written");
        database.close().await;
    }

    #[tokio::test]
    async fn import_answers_null_for_missing_empty_or_corrupt_blobs() {
        let (_temp, database, state) = ready().await;

        // Missing row.
        let (_, body) = send(state.clone(), "GET", "/api/users/backup/import", None).await;
        assert_eq!(body, r#"{"secuses":true,"data":null}"#);

        for stored in ["", "not json at all", "{broken"] {
            sqlx::query(
                "INSERT INTO settings(key, value, visible) VALUES('backup_latest', ?, 0) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(stored)
            .execute(database.pool())
            .await
            .expect("seed stored blob");
            let (status, body) = send(state.clone(), "GET", "/api/users/backup/import", None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, r#"{"secuses":true,"data":null}"#, "{stored:?}");
        }

        // SQL NULL is absent too.
        sqlx::query("UPDATE settings SET value = NULL WHERE key = 'backup_latest'")
            .execute(database.pool())
            .await
            .expect("null the blob");
        let (_, body) = send(state, "GET", "/api/users/backup/import", None).await;
        assert_eq!(body, r#"{"secuses":true,"data":null}"#);
        database.close().await;
    }

    #[tokio::test]
    async fn export_overwrites_the_previous_blob_and_resets_visible() {
        let (_temp, database, state) = ready().await;
        sqlx::query(
            "INSERT INTO settings(key, value, visible) VALUES('backup_latest', '{\"old\":true}', 1)",
        )
        .execute(database.pool())
        .await
        .expect("seed old blob");

        let (status, _) = send(
            state.clone(),
            "POST",
            "/api/users/backup/export",
            Some(r#"{"data":"{\"new\":true}"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            stored_backup(&database).await.as_deref(),
            Some(r#"{"new":true}"#)
        );
        let (value, visible): (String, i64) =
            sqlx::query_as("SELECT value, visible FROM settings WHERE key = 'backup_latest'")
                .fetch_one(database.pool())
                .await
                .expect("row");
        assert_eq!(value, r#"{"new":true}"#);
        assert_eq!(visible, 0, "legacy upsert forces visible = 0");
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM settings WHERE key = 'backup_latest'")
                .fetch_one(database.pool())
                .await
                .expect("count");
        assert_eq!(count, 1, "one row, upserted");
        database.close().await;
    }

    #[tokio::test]
    async fn fail_closed_state_answers_503_without_sql_text() {
        let (_temp, state) = unavailable().await;
        for (method, uri, body) in [
            ("POST", "/api/users/backup/export", Some(r#"{"data":"{}"}"#)),
            ("GET", "/api/users/backup/import", None),
        ] {
            let (status, response) = send(state.clone(), method, uri, body).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(response, r#"{"secuses":false,"error":"db_unavailable"}"#);
            for forbidden in ["SELECT", "INSERT", "sqlite", "PRAGMA"] {
                assert!(!response.contains(forbidden), "no SQL text in {response}");
            }
        }
    }
}
