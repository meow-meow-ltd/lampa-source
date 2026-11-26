//! Per-profile settings route group — owner: T09.
//!
//! Frozen paths (T03 golden fixtures 13–14): `GET`/`POST /api/profile/settings`.
//! Device cookie (`device_token`) is enforced by
//! [`crate::http::middleware::auth_guard`] before these handlers run.
//!
//! Legacy semantics ported from `backend/routes/profile_settings.js`:
//!
//! * the profile id comes from the `profile` (or `x-profile`) header, else from
//!   the JSON body (`profile`, then `params.profile`); a missing/non-numeric id
//!   yields `400 {"secuses":false,"text":"Missing profile"}`;
//! * `GET` without `?key=` returns every setting of the profile as
//!   `{secuses:true,value:{key:value,...}}`; with `?key=` it returns
//!   `{secuses:true,value:<string|null>}` (a missing row is `null`);
//! * `POST` upserts on the composite `(profile_id, key)` key, trims the key,
//!   stringifies non-string JSON values (legacy `JSON.stringify`), and treats a
//!   JSON `null` value as **delete**;
//! * failures never leak SQL text (`{"secuses":false,"error":"database_error"}`).

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::Value;

use crate::profiles::profile_settings::ProfileSettingsRepo;
use crate::state::AppState;

use super::profiles::{fields_from_body, js_parse_int, percent_decode, Fields};

/// Mounts `GET`/`POST /api/profile/settings`.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/profile/settings", get(settings).post(save_setting))
}

#[derive(Debug, Serialize)]
struct ValuesResponse {
    secuses: bool,
    value: Value,
}

#[derive(Debug, Serialize)]
struct SecusesResponse {
    secuses: bool,
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

/// `GET /api/profile/settings`.
async fn settings(State(state): State<AppState>, request: Request) -> Response {
    let Some(database) = state.db().database() else {
        return db_unavailable();
    };
    let (parts, body) = request.into_parts();
    let fields = fields_from_body(body).await;
    let profile_id = profile_id(&parts.headers, &fields);
    if profile_id == 0 {
        return legacy_text_error(StatusCode::BAD_REQUEST, "Missing profile");
    }
    let query = parts.uri.query().unwrap_or_default();
    let repo = ProfileSettingsRepo::new(database.pool().clone());

    if let Some(key) = query_value(query, "key").filter(|key| !key.is_empty()) {
        return match repo.get(profile_id, &key).await {
            Ok(setting) => json_response(
                StatusCode::OK,
                ValuesResponse {
                    secuses: true,
                    value: setting
                        .and_then(|setting| setting.value)
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                },
            ),
            Err(error) => database_error("profile_settings.get", &error),
        };
    }

    match repo.list(profile_id).await {
        Ok(settings) => {
            let mut values = serde_json::Map::new();
            for setting in settings {
                values.insert(
                    setting.key,
                    setting.value.map(Value::String).unwrap_or(Value::Null),
                );
            }
            json_response(
                StatusCode::OK,
                ValuesResponse {
                    secuses: true,
                    value: Value::Object(values),
                },
            )
        }
        Err(error) => database_error("profile_settings.list", &error),
    }
}

/// `POST /api/profile/settings`.
async fn save_setting(State(state): State<AppState>, request: Request) -> Response {
    let Some(database) = state.db().database() else {
        return db_unavailable();
    };
    let (parts, body) = request.into_parts();
    let fields = fields_from_body(body).await;
    let profile_id = profile_id(&parts.headers, &fields);
    if profile_id == 0 {
        return legacy_text_error(StatusCode::BAD_REQUEST, "Missing profile");
    }

    let key = fields
        .field("key")
        .and_then(|value| value.as_string())
        .map(|key| key.trim().to_owned())
        .unwrap_or_default();
    if key.is_empty() {
        return legacy_text_error(StatusCode::BAD_REQUEST, "Missing key");
    }
    let Some(value) = fields.field("value") else {
        return legacy_text_error(StatusCode::BAD_REQUEST, "Missing value");
    };

    // Legacy: a JSON `null` deletes the row; non-string values are stringified.
    let stored = if value.is_json_null() {
        None
    } else {
        match value.as_string() {
            Some(text) => Some(text),
            None => match serde_json::to_string(&value.as_json_owned()) {
                Ok(text) => Some(text),
                Err(_) => return legacy_text_error(StatusCode::BAD_REQUEST, "Invalid value"),
            },
        }
    };

    let repo = ProfileSettingsRepo::new(database.pool().clone());
    let result = match stored.as_deref() {
        None => repo.delete(profile_id, &key).await.map(|_| ()),
        Some(value) => repo.upsert(profile_id, &key, value).await.map(|_| ()),
    };
    match result {
        Ok(()) => json_response(StatusCode::OK, SecusesResponse { secuses: true }),
        Err(error) => database_error("profile_settings.save", &error),
    }
}

/// Legacy `profileIdFromReq`: header first, then body, as JS truthiness dictates.
fn profile_id(headers: &HeaderMap, fields: &Fields) -> i64 {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
    };
    if let Some(raw) = header("profile") {
        return js_parse_int(raw).unwrap_or(0);
    }
    if let Some(raw) = header("x-profile") {
        return js_parse_int(raw).unwrap_or(0);
    }
    if let Some(value) = fields.field("profile") {
        if is_js_truthy(&value.as_json_owned()) {
            return value.as_js_int().unwrap_or(0);
        }
    }
    if let Some(params) = fields.field("params") {
        let params = params.as_json_owned();
        if let Some(profile) = params.get("profile") {
            if is_js_truthy(profile) {
                return profile
                    .as_i64()
                    .or_else(|| profile.as_str().and_then(js_parse_int))
                    .unwrap_or(0);
            }
        }
    }
    0
}

/// JS truthiness for the legacy `a || b` fallbacks.
fn is_js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// First value of `name` in a raw query string (`None` when absent).
fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (percent_decode(key) == name).then(|| percent_decode(value))
    })
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
        "profile settings API database failure"
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
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = HttpRequest::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
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

    fn fixture_body(fixture: &str) -> String {
        let fixture: Value = serde_json::from_str(fixture).expect("fixture JSON");
        fixture["response"]["body"]
            .as_str()
            .expect("fixture body string")
            .to_owned()
    }

    /// T03 synthetic database: 3 profiles + profile_settings for profiles 1 and 2.
    async fn seed_golden(database: &Database) {
        for (id, name, icon, is_main, is_child, sort) in [
            (1_i64, "Main", "l_1", 1_i64, 0_i64, 1_i64),
            (2, "Kids", "l_2", 0, 1, 2),
            (3, "Guest", "l_3", 0, 0, 3),
        ] {
            sqlx::query(
                "INSERT INTO profiles(id, name, icon, is_main, is_child, sort_order, created_at, updated_at) \
                 VALUES(?, ?, ?, ?, ?, ?, 1000, 1000)",
            )
            .bind(id)
            .bind(name)
            .bind(icon)
            .bind(is_main)
            .bind(is_child)
            .bind(sort)
            .execute(database.pool())
            .await
            .expect("seed synthetic profile");
        }
        for (profile_id, key, value) in [
            (1_i64, "cinema_mode", "golden"),
            (1, "last_device", "golden-device-0001"),
            (1, "watch_progress", "1234"),
            (2, "cinema_mode", "off"),
        ] {
            sqlx::query(
                "INSERT INTO profile_settings(profile_id, key, value, updated_at) VALUES(?, ?, ?, 1000)",
            )
            .bind(profile_id)
            .bind(key)
            .bind(value)
            .execute(database.pool())
            .await
            .expect("seed synthetic profile setting");
        }
    }

    async fn scalar(database: &Database, sql: &str, profile_id: i64, key: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(sql)
            .bind(profile_id)
            .bind(key)
            .fetch_optional(database.pool())
            .await
            .expect("scalar")
            .flatten()
    }

    #[tokio::test]
    async fn fixture_13_settings_all_is_byte_identical() {
        let (_temp, database, state) = ready().await;
        seed_golden(&database).await;

        let (status, body) = send(
            state,
            "GET",
            "/api/profile/settings",
            &[("profile", "1")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            fixture_body(include_str!(
                "../../../tests/fixtures/api/13_get_api_profile_settings.json"
            ))
        );
        database.close().await;
    }

    #[tokio::test]
    async fn fixture_14_upsert_is_byte_identical_and_keyed_by_profile() {
        let (_temp, database, state) = ready().await;
        seed_golden(&database).await;

        let (status, body) = send(
            state,
            "POST",
            "/api/profile/settings",
            &[("profile", "1"), ("content-type", "application/json")],
            Some(r#"{"key":"golden_flag","value":"on"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            fixture_body(include_str!(
                "../../../tests/fixtures/api/14_post_api_profile_settings.json"
            ))
        );

        assert_eq!(
            scalar(
                &database,
                "SELECT value FROM profile_settings WHERE profile_id = ? AND key = ?",
                1,
                "golden_flag"
            )
            .await
            .as_deref(),
            Some("on")
        );
        assert_eq!(
            scalar(
                &database,
                "SELECT value FROM profile_settings WHERE profile_id = ? AND key = ?",
                2,
                "golden_flag"
            )
            .await,
            None,
            "the composite key isolates profiles"
        );
        let updated_at: i64 = sqlx::query_scalar(
            "SELECT updated_at FROM profile_settings WHERE profile_id = 1 AND key = 'golden_flag'",
        )
        .fetch_one(database.pool())
        .await
        .expect("updated_at");
        assert!(updated_at > 1000, "upsert bumps updated_at");
        database.close().await;
    }

    #[tokio::test]
    async fn single_key_get_returns_the_value_or_null() {
        let (_temp, database, state) = ready().await;
        seed_golden(&database).await;

        let (status, body) = send(
            state.clone(),
            "GET",
            "/api/profile/settings?key=cinema_mode",
            &[("profile", "1")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true,"value":"golden"}"#);

        let (status, body) = send(
            state.clone(),
            "GET",
            "/api/profile/settings?key=not_there",
            &[("profile", "1")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true,"value":null}"#);

        // Empty `key=` is falsy in legacy JS -> the full map is returned.
        let (status, body) = send(
            state,
            "GET",
            "/api/profile/settings?key=",
            &[("profile", "1")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("cinema_mode"));
        database.close().await;
    }

    #[tokio::test]
    async fn a_json_null_value_deletes_the_row() {
        let (_temp, database, state) = ready().await;
        seed_golden(&database).await;

        let (status, body) = send(
            state,
            "POST",
            "/api/profile/settings",
            &[("profile", "2"), ("content-type", "application/json")],
            Some(r#"{"key":"cinema_mode","value":null}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true}"#);
        assert_eq!(
            scalar(
                &database,
                "SELECT value FROM profile_settings WHERE profile_id = ? AND key = ?",
                2,
                "cinema_mode"
            )
            .await,
            None,
            "null deletes instead of storing \"null\""
        );
        assert_eq!(
            scalar(
                &database,
                "SELECT value FROM profile_settings WHERE profile_id = ? AND key = ?",
                1,
                "cinema_mode"
            )
            .await
            .as_deref(),
            Some("golden"),
            "other profiles are untouched"
        );
        database.close().await;
    }

    #[tokio::test]
    async fn non_string_values_are_stringified_like_json_stringify() {
        let (_temp, database, state) = ready().await;
        seed_golden(&database).await;

        for (body, expected) in [
            (r#"{"key":"number","value":42}"#, "42"),
            (r#"{"key":"boolean","value":false}"#, "false"),
            (
                r#"{"key":"object","value":{"a":1,"b":[true,null]}}"#,
                r#"{"a":1,"b":[true,null]}"#,
            ),
            (r#"{"key":"array","value":[1,2,3]}"#, "[1,2,3]"),
        ] {
            let (status, response) = send(
                state.clone(),
                "POST",
                "/api/profile/settings",
                &[("profile", "3"), ("content-type", "application/json")],
                Some(body),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(response, r#"{"secuses":true}"#);
            let key = match body.contains("\"number\"") {
                true => "number",
                false if body.contains("\"boolean\"") => "boolean",
                false if body.contains("\"object\"") => "object",
                _ => "array",
            };
            assert_eq!(
                scalar(
                    &database,
                    "SELECT value FROM profile_settings WHERE profile_id = ? AND key = ?",
                    3,
                    key
                )
                .await
                .as_deref(),
                Some(expected),
                "{body}"
            );
        }
        database.close().await;
    }

    #[tokio::test]
    async fn missing_profile_key_or_value_are_legacy_400s() {
        let (_temp, database, state) = ready().await;
        seed_golden(&database).await;

        for (headers, body, expected) in [
            (
                vec![],
                None,
                r#"{"secuses":false,"text":"Missing profile"}"#,
            ),
            (
                vec![("profile", "abc")],
                None,
                r#"{"secuses":false,"text":"Missing profile"}"#,
            ),
            (
                vec![("profile", "0")],
                None,
                r#"{"secuses":false,"text":"Missing profile"}"#,
            ),
            (
                vec![("profile", "1")],
                Some(r#"{}"#),
                r#"{"secuses":false,"text":"Missing key"}"#,
            ),
            (
                vec![("profile", "1")],
                Some(r#"{"key":"   "}"#),
                r#"{"secuses":false,"text":"Missing key"}"#,
            ),
            (
                vec![("profile", "1")],
                Some(r#"{"key":"x"}"#),
                r#"{"secuses":false,"text":"Missing value"}"#,
            ),
        ] {
            let (status, response) = send(
                state.clone(),
                "POST",
                "/api/profile/settings",
                &headers
                    .iter()
                    .map(|(name, value)| (*name, *value))
                    .chain([("content-type", "application/json")])
                    .collect::<Vec<_>>(),
                body,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
            assert_eq!(response, expected);
        }
        database.close().await;
    }

    #[tokio::test]
    async fn profile_comes_from_x_profile_header_or_body() {
        let (_temp, database, state) = ready().await;
        seed_golden(&database).await;

        let (status, body) = send(
            state.clone(),
            "GET",
            "/api/profile/settings",
            &[("x-profile", "2")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true,"value":{"cinema_mode":"off"}}"#);

        let (status, body) = send(
            state.clone(),
            "POST",
            "/api/profile/settings",
            &[("content-type", "application/json")],
            Some(r#"{"profile":2,"key":"from_body","value":"yes"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true}"#);
        assert_eq!(
            scalar(
                &database,
                "SELECT value FROM profile_settings WHERE profile_id = ? AND key = ?",
                2,
                "from_body"
            )
            .await
            .as_deref(),
            Some("yes")
        );

        let (status, body) = send(
            state,
            "POST",
            "/api/profile/settings",
            &[("content-type", "application/json")],
            Some(r#"{"params":{"profile":3},"key":"nested","value":"yes"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true}"#);
        database.close().await;
    }

    #[tokio::test]
    async fn urlencoded_bodies_are_accepted() {
        let (_temp, database, state) = ready().await;
        seed_golden(&database).await;

        let (status, body) = send(
            state,
            "POST",
            "/api/profile/settings",
            &[
                ("profile", "1"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            Some("key=form_key&value=form_value"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true}"#);
        assert_eq!(
            scalar(
                &database,
                "SELECT value FROM profile_settings WHERE profile_id = ? AND key = ?",
                1,
                "form_key"
            )
            .await
            .as_deref(),
            Some("form_value")
        );
        database.close().await;
    }

    #[tokio::test]
    async fn an_existing_value_is_overwritten() {
        let (_temp, database, state) = ready().await;
        seed_golden(&database).await;

        let (status, _) = send(
            state,
            "POST",
            "/api/profile/settings",
            &[("profile", "1"), ("content-type", "application/json")],
            Some(r#"{"key":"cinema_mode","value":"updated"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            scalar(
                &database,
                "SELECT value FROM profile_settings WHERE profile_id = ? AND key = ?",
                1,
                "cinema_mode"
            )
            .await
            .as_deref(),
            Some("updated")
        );
        database.close().await;
    }

    #[tokio::test]
    async fn fail_closed_state_answers_503_without_sql_text() {
        let (_temp, state) = unavailable().await;
        let (status, body) = send(
            state.clone(),
            "GET",
            "/api/profile/settings",
            &[("profile", "1")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, r#"{"secuses":false,"error":"db_unavailable"}"#);

        let (status, body) = send(
            state,
            "POST",
            "/api/profile/settings",
            &[("profile", "1")],
            Some(r#"{"key":"x","value":"y"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, r#"{"secuses":false,"error":"db_unavailable"}"#);
        for forbidden in ["SELECT", "INSERT", "UPDATE", "sqlite", "PRAGMA"] {
            assert!(!body.contains(forbidden), "no SQL text in {body}");
        }
    }

    #[test]
    fn query_values_are_percent_decoded_and_first_wins() {
        assert_eq!(
            query_value("key=cinema%5Fmode&other=1", "key"),
            Some("cinema_mode".to_owned())
        );
        assert_eq!(query_value("key=a+b&key=c", "key"), Some("a b".to_owned()));
        assert_eq!(query_value("other=1", "key"), None);
        assert_eq!(query_value("ke%79=x", "key"), Some("x".to_owned()));
    }
}
