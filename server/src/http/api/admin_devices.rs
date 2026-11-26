//! Admin device route group — owner: T07.
//!
//! Frozen paths (T03 golden fixtures 02–03): `GET /api/admin/devices`,
//! `POST /api/admin/device/action`. Admin cookie (`auth_token`) is enforced by
//! [`crate::http::middleware::auth_guard`].
//!
//! Legacy semantics (`backend/routes/admin.js`, plan §3.5/§5.4):
//! * `GET` returns every `device_access` row as
//!   `{id, device_id, ip, user_agent, status, attempts, last_attempt_at,
//!   blocked_until, last_seen_at}` in legacy table order (rowid) — the token
//!   digest is never selected;
//! * `POST {id, action}`:
//!   * `unblock` → `status='pending', attempts=0, blocked_until=NULL`;
//!   * `revoke` → `status='pending', token_hash=NULL` (the device must re-pair);
//!   * `delete` → removes the row;
//!   * success → `200 {"success":true}` even when no row matched (legacy never
//!     checked `rows_affected`);
//!   * an unknown/missing action → `400` with an **empty body** (legacy
//!     `res.writeHead(400); res.end()`), one of the preserved admin quirks;
//! * database failures are reported as a stable sanitised body — raw SQL
//!   diagnostics are never forwarded to a client.
//!
//! All `device_access` SQL lives in [`DevicesRepo`] (repository-owned queries and
//! `DbError` mapping); the handlers only translate repository results into the
//! legacy HTTP contract.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::sqlite::SqlitePool;

use crate::db::DbError;
use crate::state::AppState;

/// Mounts the two admin device routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/devices", get(list))
        .route("/api/admin/device/action", post(action))
}

/// One `device_access` row exactly as legacy fixture 02 renders it.
#[derive(Serialize)]
struct DeviceBody {
    id: i64,
    device_id: String,
    ip: Option<String>,
    user_agent: Option<String>,
    status: String,
    attempts: Option<i64>,
    last_attempt_at: Option<i64>,
    blocked_until: Option<i64>,
    last_seen_at: Option<i64>,
}

/// One repository row (deliberately without `token_hash`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceRecord {
    id: i64,
    device_id: String,
    ip: Option<String>,
    user_agent: Option<String>,
    status: String,
    attempts: Option<i64>,
    last_attempt_at: Option<i64>,
    blocked_until: Option<i64>,
    last_seen_at: Option<i64>,
}

impl From<DeviceRecord> for DeviceBody {
    fn from(record: DeviceRecord) -> Self {
        Self {
            id: record.id,
            device_id: record.device_id,
            ip: record.ip,
            user_agent: record.user_agent,
            status: record.status,
            attempts: record.attempts,
            last_attempt_at: record.last_attempt_at,
            blocked_until: record.blocked_until,
            last_seen_at: record.last_seen_at,
        }
    }
}

/// `{"success":true}` — the legacy mutation acknowledgement.
#[derive(Serialize)]
struct SuccessBody {
    success: bool,
}

fn success() -> Response {
    Json(SuccessBody { success: true }).into_response()
}

/// `{"error":"..."}` with a stable, non-SQL code.
fn error(status: StatusCode, code: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": code }))).into_response()
}

/// Sanitised `500` for repository failures; the driver text is never forwarded.
fn database_error(failure: &DbError) -> Response {
    tracing::error!(
        code = failure.code(),
        "admin devices API database error (details logged server-side only)"
    );
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}

/// Repository over the `device_access` table for the admin endpoints.
///
/// Owns the SELECT/UPDATE/DELETE statements and maps driver errors to
/// [`DbError`], so the HTTP handlers never execute SQL. `id` stays a bound
/// `?` parameter (a missing/non-numeric legacy id binds NULL and matches no row,
/// which still answers `{"success":true}`).
#[derive(Debug, Clone)]
struct DevicesRepo {
    pool: SqlitePool,
}

impl DevicesRepo {
    fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// All devices in legacy table (rowid) order, without the token digest.
    async fn list(&self) -> Result<Vec<DeviceRecord>, DbError> {
        let rows = sqlx::query_as::<
            _,
            (
                i64,
                String,
                Option<String>,
                Option<String>,
                String,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
            ),
        >(
            "SELECT id, device_id, ip, user_agent, status, attempts, last_attempt_at, \
             blocked_until, last_seen_at FROM device_access",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    device_id,
                    ip,
                    user_agent,
                    status,
                    attempts,
                    last_attempt_at,
                    blocked_until,
                    last_seen_at,
                )| DeviceRecord {
                    id,
                    device_id,
                    ip,
                    user_agent,
                    status,
                    attempts,
                    last_attempt_at,
                    blocked_until,
                    last_seen_at,
                },
            )
            .collect())
    }

    /// Legacy `unblock`: clear the block and the failed-attempt counter.
    async fn unblock(&self, id: Option<i64>) -> Result<(), DbError> {
        self.execute(
            "UPDATE device_access SET status='pending', attempts=0, blocked_until=NULL \
             WHERE id=?",
            id,
        )
        .await
    }

    /// Legacy `revoke`: force re-pairing by clearing the token digest.
    async fn revoke(&self, id: Option<i64>) -> Result<(), DbError> {
        self.execute(
            "UPDATE device_access SET status='pending', token_hash=NULL WHERE id=?",
            id,
        )
        .await
    }

    /// Legacy `delete`: remove the row (no cascade; there are no FK constraints).
    async fn delete(&self, id: Option<i64>) -> Result<(), DbError> {
        self.execute("DELETE FROM device_access WHERE id=?", id)
            .await
    }

    /// Runs one static statement with the bound legacy id.
    async fn execute(&self, sql: &'static str, id: Option<i64>) -> Result<(), DbError> {
        sqlx::query(sql)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(DbError::query)?;
        Ok(())
    }
}

fn repo(state: &AppState) -> Option<DevicesRepo> {
    state
        .db()
        .database()
        .map(|database| DevicesRepo::new(database.pool().clone()))
}

/// `GET /api/admin/devices` — the full device list.
async fn list(State(state): State<AppState>) -> Response {
    let Some(repo) = repo(&state) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
    };
    match repo.list().await {
        Ok(rows) => {
            Json(rows.into_iter().map(DeviceBody::from).collect::<Vec<_>>()).into_response()
        }
        Err(error) => database_error(&error),
    }
}

/// `POST /api/admin/device/action` — `unblock` / `revoke` / `delete`.
async fn action(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(repo) = repo(&state) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
    };
    let Some(object) = parse_object(&body) else {
        // Legacy body-parser rejected a malformed body with an empty 400.
        return StatusCode::BAD_REQUEST.into_response();
    };
    let action = object.get("action").and_then(Value::as_str);
    // Node binds whatever `id` arrived; a non-numeric id simply matches no row
    // and the request still answers success.
    let id = object.get("id").and_then(Value::as_i64);

    let result = match action {
        Some("unblock") => repo.unblock(id).await,
        Some("revoke") => repo.revoke(id).await,
        Some("delete") => repo.delete(id).await,
        // Preserved legacy quirk: empty-body 400 for an unknown action.
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    match result {
        Ok(()) => success(),
        Err(error) => database_error(&error),
    }
}

/// Parses a JSON object body regardless of `Content-Type`.
fn parse_object(body: &[u8]) -> Option<Map<String, Value>> {
    let text = std::str::from_utf8(body).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(object)) => Some(object),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::TempDb;
    use axum::body::to_bytes;

    #[test]
    fn parse_object_accepts_only_json_objects() {
        let object = parse_object(br#"{"id":2,"action":"unblock"}"#).expect("object");
        assert_eq!(object.get("id").and_then(Value::as_i64), Some(2));
        assert_eq!(
            object.get("action").and_then(Value::as_str),
            Some("unblock")
        );
        assert!(parse_object(b"").is_none());
        assert!(parse_object(b"[]").is_none());
        assert!(parse_object(br#"{"id":1,"#).is_none());
    }

    #[tokio::test]
    async fn repository_failures_are_sanitized() {
        let response = database_error(&DbError::query(sqlx::Error::RowNotFound));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .expect("error body");
        assert_eq!(body.as_ref(), br#"{"error":"internal_error"}"#.as_slice());
        assert!(!String::from_utf8_lossy(&body).contains("rows"));
    }

    #[tokio::test]
    async fn repository_lists_and_applies_legacy_actions() {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        sqlx::query(
            "INSERT INTO device_access \
             (device_id, status, attempts, token_hash, ip, user_agent, last_attempt_at, \
              blocked_until, last_seen_at) VALUES \
             ('device-1', 'blocked', 5, 'hash-1', '1.2.3.4', 'ua/1', 10, 999, 20), \
             ('device-2', 'allowed', 0, 'hash-2', NULL, NULL, NULL, NULL, NULL)",
        )
        .execute(database.pool())
        .await
        .expect("seed devices");
        let repo = DevicesRepo::new(database.pool().clone());

        let rows = repo.list().await.expect("list");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].device_id, "device-1");
        assert_eq!(rows[0].status, "blocked");
        assert_eq!(rows[0].attempts, Some(5));
        assert_eq!(rows[1].device_id, "device-2");

        repo.unblock(Some(rows[0].id)).await.expect("unblock");
        let (status, attempts, blocked_until): (String, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT status, attempts, blocked_until FROM device_access WHERE device_id='device-1'",
        )
        .fetch_one(database.pool())
        .await
        .expect("read unblocked");
        assert_eq!(status, "pending");
        assert_eq!(attempts, Some(0));
        assert_eq!(blocked_until, None);

        repo.revoke(Some(rows[1].id)).await.expect("revoke");
        let (status, token_hash): (String, Option<String>) = sqlx::query_as(
            "SELECT status, token_hash FROM device_access WHERE device_id='device-2'",
        )
        .fetch_one(database.pool())
        .await
        .expect("read revoked");
        assert_eq!(status, "pending");
        assert_eq!(token_hash, None);

        repo.delete(Some(rows[0].id)).await.expect("delete");
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM device_access")
            .fetch_one(database.pool())
            .await
            .expect("count");
        assert_eq!(remaining, 1);

        // Legacy behaviour: unknown or absent ids are successful no-ops.
        repo.unblock(Some(99_999)).await.expect("unknown id");
        repo.delete(None).await.expect("missing id binds NULL");
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM device_access")
            .fetch_one(database.pool())
            .await
            .expect("count");
        assert_eq!(remaining, 1);

        database.close().await;
    }
}
