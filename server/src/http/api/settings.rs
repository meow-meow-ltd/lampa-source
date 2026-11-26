//! Settings route group — owner: T07.
//!
//! Frozen paths (T03 golden fixtures 04–06):
//! `GET /api/settings`, `PUT /api/settings`, `DELETE /api/settings/{key}`.
//! Admin cookie (`auth_token`) is enforced by [`crate::http::middleware::auth_guard`].
//!
//! Legacy semantics (`backend/routes/settings.js`, plan §3.5):
//! * `GET` returns **all** rows `{key, value, visible}` in legacy table order;
//!   `admin_password`/`admin_token` are masked as `"***"` (R8 stop-condition:
//!   no endpoint may ever return those secrets in clear text — the T04
//!   [`crate::settings::SettingsRepo`] does the masking);
//! * `PUT` upserts `{key, value}` and answers `{"success":true}`; echoing the
//!   mask `"***"` back for a secret key is a no-op (a client round-trip of a
//!   masked `GET` must not destroy the stored secret);
//! * `DELETE /api/settings/{key}` takes the key from the last path segment and
//!   answers `{"success":true}` whether or not the row existed (legacy
//!   `DELETE ... WHERE key=?` had no `rows_affected` check);
//! * database failures are reported as a stable sanitised body — raw
//!   SQLite/SQLx diagnostics are never forwarded to a client (plan §3.6 п.6).

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::state::AppState;

/// Mounts the three settings routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/settings", get(list).put(upsert))
        .route("/api/settings/{key}", delete(remove))
}

/// One row of the legacy `GET /api/settings` array.
///
/// Field order is the fixture order; `visible` stays a legacy 0/1 integer (not a
/// JSON boolean) and `value` may be `null` exactly like Node's `JSON.stringify`.
#[derive(Serialize)]
struct SettingBody {
    key: String,
    value: Option<String>,
    visible: i64,
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

/// `GET /api/settings` — all rows, secrets masked by the repository.
async fn list(State(state): State<AppState>) -> Response {
    let Some(database) = state.db().database() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
    };
    match database.settings().list().await {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|setting| SettingBody {
                    key: setting.key,
                    value: setting.value,
                    visible: if setting.visible { 1 } else { 0 },
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    }
}

/// `PUT /api/settings` — legacy upsert of one `{key, value}` pair.
async fn upsert(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(database) = state.db().database() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
    };
    let Some(object) = parse_object(&body) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    let key = object.get("key").and_then(Value::as_str);
    let value = object.get("value").and_then(Value::as_str);
    let (Some(key), Some(value)) = (key, value) else {
        // Legacy `UPDATE settings SET value=? WHERE key=?` bound `undefined`
        // values and still answered `{"success":true}`. A malformed PUT must
        // not destroy a stored value, so it is acknowledged without writing.
        return success();
    };
    if key.trim().is_empty() {
        // The legacy statement matched nothing for an empty key; keep the
        // acknowledgement and leave the row untouched.
        return success();
    }
    match database.settings().upsert(key, value).await {
        Ok(()) => success(),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    }
}

/// `DELETE /api/settings/{key}` — deletes the last path segment.
async fn remove(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let Some(database) = state.db().database() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
    };
    match database.settings().delete(&key).await {
        // The legacy handler never inspected `rows_affected`.
        Ok(_) => success(),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    }
}

/// Parses a JSON object body regardless of `Content-Type` (the legacy client
/// sends JSON; the PIN path already ignores the header for form bodies).
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

    #[test]
    fn parse_object_accepts_json_objects_and_rejects_everything_else() {
        assert_eq!(
            parse_object(br#"{"key":"menu_data","value":"1"}"#)
                .expect("object")
                .get("key")
                .and_then(Value::as_str),
            Some("menu_data")
        );
        assert!(parse_object(br#"{"key":"menu_data"}"#).is_some());
        assert!(parse_object(b"").is_none());
        assert!(parse_object(b"   ").is_none());
        assert!(parse_object(b"[]").is_none());
        assert!(parse_object(b"null").is_none());
        assert!(parse_object(br#"{"key":"broken""#).is_none());
        assert!(parse_object(b"\xff\xfe").is_none());
    }
}
