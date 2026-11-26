//! Values route group — owner: T07.
//!
//! Frozen paths (T03 golden fixtures 07–08): `POST /api/values`,
//! `GET /api/value`. Device cookie (`device_token`/`lampa_session`) is enforced
//! by [`crate::http::middleware::auth_guard`].
//!
//! Legacy semantics (`backend/routes/data.js`, plan §3.5):
//! * `POST /api/values` reads `{keys: [opaque, ...]}` and returns a JSON object
//!   with one member per **existing** key; `visible=0` keys (and therefore
//!   `admin_password`/`admin_token`) come back as `null`, never as their value;
//! * `GET /api/value?key=<opaque>` returns the JSON string value for a
//!   `visible=1` key and the JSON literal `null` for an invisible/missing key;
//! * an absent or empty `keys` array → `400 {"error":"Missing or invalid keys parameter"}`;
//!   a missing/empty `key` query parameter → `400 {"error":"Missing key parameter"}`;
//! * keys are opaque strings: no case folding, trimming or allow-list;
//! * settings reads are delegated to [`crate::settings::SettingsRepo`] (bound
//!   `?` parameters, repository-owned `DbError` mapping); this HTTP module owns
//!   no SQL and only projects visible values into the legacy response;
//! * database failures are reported as a stable sanitised body — raw SQL
//!   diagnostics are never forwarded to a client.

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Map, Value};

use crate::db::DbError;
use crate::settings::{Setting, SettingsRepo};
use crate::state::AppState;

/// Mounts the two values routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/values", post(values))
        .route("/api/value", get(value))
}

/// `{"error":"..."}` with a stable, non-SQL code.
fn error(status: StatusCode, code: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": code }))).into_response()
}

/// Settings repository for this request, or `None` when the startup guard failed.
///
/// All `settings` queries live behind [`SettingsRepo`] (repository-owned SQL and
/// `DbError` mapping); these handlers never build or execute SQL themselves.
fn settings_repo(state: &AppState) -> Option<SettingsRepo> {
    state.db().database().map(|database| database.settings())
}

/// Legacy projection of one setting row: only `visible=1` values are readable;
/// invisible keys (and rows without a value) answer the JSON literal `null`.
fn visible_value(setting: &Setting) -> Value {
    if setting.visible {
        setting
            .value
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    }
}

/// Sanitised `500` for repository failures; the driver text is never forwarded.
fn database_error(failure: &DbError) -> Response {
    tracing::error!(
        code = failure.code(),
        "values API database error (details logged server-side only)"
    );
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}

/// `POST /api/values` — batch read of opaque setting keys.
async fn values(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(repo) = settings_repo(&state) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
    };
    let Some(keys) = parse_keys(&body) else {
        return error(StatusCode::BAD_REQUEST, "Missing or invalid keys parameter");
    };
    if keys.is_empty() {
        return error(StatusCode::BAD_REQUEST, "Missing or invalid keys parameter");
    }

    let mut result = Map::new();
    for key in &keys {
        // Keys are few in the legacy client, so one repository read per key is
        // cheap and keeps every opaque key a bound parameter (`?`), never SQL.
        // `SettingsRepo::get` also masks the two secret keys; `visible_value`
        // keeps `visible=0` rows (admin_password/admin_token) at `null`.
        match repo.get(key).await {
            Ok(Some(setting)) => {
                result.insert(setting.key.clone(), visible_value(&setting));
            }
            Ok(None) => {}
            Err(error) => return database_error(&error),
        }
    }
    // `serde_json::Map` serialises in key order, matching the legacy `SQLite`
    // primary-key scan order captured by fixture 07.
    Json(Value::Object(result)).into_response()
}

/// `GET /api/value?key=<opaque>` — one setting value (or `null`).
async fn value(State(state): State<AppState>, request: Request) -> Response {
    let Some(repo) = settings_repo(&state) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
    };
    let key = query_parameter(request.uri().query().unwrap_or_default(), "key");
    let Some(key) = key.filter(|key| !key.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "Missing key parameter");
    };

    match repo.get(&key).await {
        Ok(Some(setting)) => Json(visible_value(&setting)).into_response(),
        Ok(None) => Json(Value::Null).into_response(),
        Err(error) => database_error(&error),
    }
}

/// Parses the legacy `{keys: [...]}` request body (JSON, `Content-Type`
/// irrelevant).
fn parse_keys(body: &[u8]) -> Option<Vec<String>> {
    let text = std::str::from_utf8(body).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    let document: Value = serde_json::from_str(text).ok()?;
    let array = document.get("keys")?.as_array()?;
    let mut keys = Vec::with_capacity(array.len());
    for item in array {
        keys.push(item.as_str()?.to_owned());
    }
    Some(keys)
}

/// Reads one query parameter, decoding `application/x-www-form-urlencoded`
/// escapes exactly like Node's `url.parse(req.url, true).query.[name]`.
fn query_parameter(query: &str, wanted: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == wanted {
            return Some(percent_decode(value));
        }
    }
    None
}

/// Minimal `application/x-www-form-urlencoded` value decoder (`%XX` and `+`).
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if let Some(hex) = value.get(index + 1..index + 3) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    decoded.push(byte);
                    index += 3;
                    continue;
                }
            }
        }
        if bytes[index] == b'+' {
            decoded.push(b' ');
        } else {
            decoded.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn parse_keys_accepts_only_a_non_empty_json_string_array() {
        assert_eq!(
            parse_keys(br#"{"keys":["menu_data","admin_password"]}"#),
            Some(vec!["menu_data".to_owned(), "admin_password".to_owned()])
        );
        assert_eq!(parse_keys(br#"{"keys":[]}"#), Some(Vec::new()));
        assert!(parse_keys(b"").is_none());
        assert!(parse_keys(br#"{"keys":"menu_data"}"#).is_none());
        assert!(parse_keys(br#"{"keys":[1]}"#).is_none());
        assert!(parse_keys(br#"{"other":[]}"#).is_none());
        assert!(parse_keys(br#"{"keys":["#).is_none());
    }

    #[test]
    fn visible_value_masks_invisible_and_missing_values() {
        let secret = Setting {
            key: "admin_password".to_owned(),
            value: Some("hunter2".to_owned()),
            visible: false,
        };
        assert_eq!(visible_value(&secret), Value::Null);
        let visible = Setting {
            key: "menu_data".to_owned(),
            value: Some("1".to_owned()),
            visible: true,
        };
        assert_eq!(visible_value(&visible), Value::String("1".to_owned()));
        let empty = Setting {
            key: "empty".to_owned(),
            value: None,
            visible: true,
        };
        assert_eq!(visible_value(&empty), Value::Null);
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
    async fn repository_binds_opaque_keys_and_keeps_visibility() {
        use crate::db::test_support::TempDb;

        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        sqlx::query(
            "INSERT INTO settings(key, value, visible) VALUES \
             ('menu_data', '1', 1), ('admin_password', 'hunter2', 0), \
             ('opaque''key %2F \u{00fc}', 'value', 1)",
        )
        .execute(database.pool())
        .await
        .expect("seed settings");
        let repo = database.settings();

        // Opaque keys are bound parameters: quotes/percent signs are data.
        let opaque = "opaque'key %2F ü";
        let setting = repo
            .get(opaque)
            .await
            .expect("get opaque")
            .expect("opaque row");
        assert_eq!(visible_value(&setting), Value::String("value".to_owned()));
        assert_eq!(
            repo.get("x' OR 1=1 --").await.expect("lookup"),
            None,
            "an injected key is just a missing key"
        );
        assert_eq!(repo.get("missing").await.expect("lookup"), None);

        let secret = repo
            .get("admin_password")
            .await
            .expect("get secret")
            .expect("secret row");
        assert!(!secret.visible, "legacy secrets are invisible");
        assert_eq!(
            secret.value.as_deref(),
            Some(crate::settings::MASKED_VALUE),
            "repository API masks secret values"
        );
        assert_eq!(visible_value(&secret), Value::Null);

        database.close().await;
    }

    #[test]
    fn query_parameter_decodes_opaque_keys() {
        assert_eq!(
            query_parameter("key=menu_data", "key").as_deref(),
            Some("menu_data")
        );
        assert_eq!(
            query_parameter("key=opaque%20key%2F%C3%BC", "key").as_deref(),
            Some("opaque key/ü")
        );
        assert_eq!(
            query_parameter("key=a+b&x=1", "key").as_deref(),
            Some("a b")
        );
        assert_eq!(query_parameter("key=", "key").as_deref(), Some(""));
        assert_eq!(query_parameter("other=1", "key"), None);
        assert_eq!(query_parameter("", "key"), None);
    }
}
