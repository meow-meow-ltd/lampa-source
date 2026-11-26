//! Auth session route group — owner: T10.
//!
//! W1 addition (not part of the 27 T03 fixtures): `GET /api/auth/session`.
//! Public at the guard level (reachable before pairing), but the handler reports
//! a paired state only for a valid device/admin credential:
//!
//! * `200 {"paired":true,"device_id":"…","admin":false}` for an allowed device;
//! * `200 {"paired":true,"admin":true}` for a valid `auth_token`;
//! * `401 {"paired":false,"error":"token_required"|"invalid_token"}` otherwise.
//!
//! Both the `200` and the `401` branches carry `Cache-Control: no-store`:
//! session state (paired or not) must never be cached by a proxy or browser.
//!
//! Transitional C (S8 §4.3/§8.3): a request authenticated by the legacy readable
//! `device_token` gets the HttpOnly `lampa_session` re-issued with the **same**
//! token (no rotation, so old devices are never re-paired) and the legacy cookie
//! is left in place. This endpoint never writes `last_seen_at`.

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Serialize;

use crate::auth::cookie::{self, ADMIN_TOKEN_COOKIE, DEVICE_TOKEN_COOKIE, LAMPA_SESSION_COOKIE};
use crate::auth::{CookieValue, SessionOutcome};
use crate::state::AppState;

/// Mounts `GET /api/auth/session`.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/auth/session", get(session))
}

/// Paired device response (`admin` additionally reports a valid admin cookie).
#[derive(Serialize)]
struct SessionDevice {
    paired: bool,
    device_id: String,
    admin: bool,
}

/// Paired admin-only response (no device cookie).
#[derive(Serialize)]
struct SessionAdmin {
    paired: bool,
    admin: bool,
}

/// Unpaired response; the body stays JSON so the client can branch on `error`.
#[derive(Serialize)]
struct SessionUnauthorized {
    paired: bool,
    error: &'static str,
}

/// `GET /api/auth/session` handler.
async fn session(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let device_names: &[&str] = if state.auth().legacy_cookie_enabled() {
        &[LAMPA_SESSION_COOKIE, DEVICE_TOKEN_COOKIE]
    } else {
        &[LAMPA_SESSION_COOKIE]
    };
    let device = cookie::find(&headers, device_names);
    let admin = cookie::find(&headers, &[ADMIN_TOKEN_COOKIE]);
    let outcome = state.auth().session(device, admin).await;

    match outcome {
        SessionOutcome::Device { device_id, admin } => {
            let mut response = no_store(
                axum::Json(SessionDevice {
                    paired: true,
                    device_id,
                    admin,
                })
                .into_response(),
            );
            // No-re-pair migration: a valid legacy cookie upgrades to the
            // HttpOnly session cookie with the same value.
            if state.auth().legacy_cookie_enabled()
                && !matches!(
                    cookie::find(&headers, &[LAMPA_SESSION_COOKIE]),
                    CookieValue::Value(_)
                )
            {
                if let CookieValue::Value(token) = cookie::find(&headers, &[DEVICE_TOKEN_COOKIE]) {
                    append_cookie(
                        &mut response,
                        cookie::session_cookie(token, cookie::is_https(&headers)),
                    );
                }
            }
            response
        }
        SessionOutcome::Admin => no_store(
            axum::Json(SessionAdmin {
                paired: true,
                admin: true,
            })
            .into_response(),
        ),
        SessionOutcome::Missing => unauthorized("token_required", false),
        SessionOutcome::Invalid => unauthorized("invalid_token", true),
        SessionOutcome::Unavailable => {
            let presented =
                !matches!(device, CookieValue::Missing) || !matches!(admin, CookieValue::Missing);
            if presented {
                unauthorized("invalid_token", true)
            } else {
                unauthorized("token_required", false)
            }
        }
        SessionOutcome::DatabaseError => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({ "error": "internal_error" })),
        )
            .into_response(),
    }
}

/// `401 {"paired":false,"error":...}` plus (for invalid credentials) cookie
/// expiry so the client returns to the PIN gate. Like the `200` branches it is
/// `Cache-Control: no-store` (session state must never be cached, G2-04).
fn unauthorized(error: &'static str, clear: bool) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        axum::Json(SessionUnauthorized {
            paired: false,
            error,
        }),
    )
        .into_response();
    if clear {
        for cookie in cookie::clear_device_cookies() {
            append_cookie(&mut response, cookie);
        }
    }
    no_store(response)
}

/// `Cache-Control: no-store` (session state must never be cached).
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn append_cookie(response: &mut Response, cookie: String) {
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    use crate::auth::device::hash_token;
    use crate::config::Config;
    use crate::db::test_support::TempDb;
    use crate::db::Database;
    use crate::state::DbState;

    const ADMIN_TOKEN: &str = "admin-token-for-tests";
    const DEVICE_TOKEN: &str = "known-token";

    fn config(legacy: bool) -> Config {
        let value = if legacy { "1" } else { "0" }.to_owned();
        Config::from_lookup(move |key| match key {
            "DEVICE_LEGACY_COOKIE" => Some(value.clone()),
            _ => None,
        })
        .expect("test config")
    }

    async fn ready_state(legacy: bool) -> (TempDb, Database, AppState) {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        let state = AppState::new(config(legacy), DbState::Ready(database.clone()));
        (temp, database, state)
    }

    async fn seed_device(database: &Database, token: &str, status: &str) {
        sqlx::query(
            "INSERT INTO device_access(device_id, status, attempts, token_hash, last_seen_at) \
             VALUES('device-1', ?, 0, ?, 1000)",
        )
        .bind(status)
        .bind(hash_token(token))
        .execute(database.pool())
        .await
        .expect("seed device");
    }

    async fn seed_admin(database: &Database) {
        sqlx::query("INSERT INTO settings(key, value, visible) VALUES('admin_token', ?, 0)")
            .bind(ADMIN_TOKEN)
            .execute(database.pool())
            .await
            .expect("seed admin");
    }

    async fn send(state: AppState, cookie: Option<&str>) -> Response {
        let mut builder = HttpRequest::builder()
            .method("GET")
            .uri("/api/auth/session");
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        router()
            .with_state(state)
            .oneshot(builder.body(Body::empty()).expect("request"))
            .await
            .expect("response")
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), 4096).await.expect("body");
        serde_json::from_slice(&body).expect("json body")
    }

    fn set_cookies(response: &Response) -> Vec<String> {
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn valid_device_cookie_returns_the_paired_device() {
        let (_temp, database, state) = ready_state(true).await;
        seed_device(&database, DEVICE_TOKEN, "allowed").await;
        let response = send(state, Some(&format!("device_token={DEVICE_TOKEN}"))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let cookies = set_cookies(&response);
        // Legacy token upgrades to the HttpOnly session cookie without rotation.
        assert_eq!(cookies.len(), 1);
        assert!(cookies[0].contains(&format!("lampa_session={DEVICE_TOKEN}")));
        assert!(cookies[0].contains("HttpOnly"));
        assert_eq!(
            body_json(response).await,
            serde_json::json!({
                "paired": true,
                "device_id": "device-1",
                "admin": false
            })
        );
    }

    #[tokio::test]
    async fn session_cookie_does_not_need_reissue() {
        let (_temp, database, state) = ready_state(true).await;
        seed_device(&database, DEVICE_TOKEN, "allowed").await;
        let response = send(state, Some(&format!("lampa_session={DEVICE_TOKEN}"))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(set_cookies(&response).is_empty());
        assert_eq!(body_json(response).await["paired"], true);
    }

    #[tokio::test]
    async fn valid_admin_cookie_returns_a_paired_admin_session() {
        let (_temp, database, state) = ready_state(true).await;
        seed_admin(&database).await;
        let response = send(state, Some(&format!("auth_token={ADMIN_TOKEN}"))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "paired": true, "admin": true })
        );
    }

    #[tokio::test]
    async fn device_and_admin_cookies_report_both() {
        let (_temp, database, state) = ready_state(true).await;
        seed_device(&database, DEVICE_TOKEN, "allowed").await;
        seed_admin(&database).await;
        let response = send(
            state,
            Some(&format!(
                "lampa_session={DEVICE_TOKEN}; auth_token={ADMIN_TOKEN}"
            )),
        )
        .await;
        let json = body_json(response).await;
        assert_eq!(json["paired"], true);
        assert_eq!(json["admin"], true);
        assert_eq!(json["device_id"], "device-1");
    }

    #[tokio::test]
    async fn no_cookie_is_401_token_required() {
        let (_temp, _database, state) = ready_state(true).await;
        let response = send(state, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        // G2-04: the 401 branch carries the same `Cache-Control: no-store` as
        // the 200 branches, so an unpaired session is never cached either.
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let body = to_bytes(response.into_body(), 1024).await.expect("body");
        assert_eq!(
            body.as_ref(),
            br#"{"paired":false,"error":"token_required"}"#.as_slice()
        );
    }

    #[tokio::test]
    async fn unknown_or_revoked_cookie_is_401_invalid_token_and_clears() {
        let (_temp, database, state) = ready_state(true).await;
        seed_device(&database, DEVICE_TOKEN, "pending").await;
        for cookie in [
            "device_token=not-a-real-token".to_owned(),
            format!("device_token={DEVICE_TOKEN}"),
        ] {
            let response = send(state.clone(), Some(&cookie)).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{cookie}");
            assert_eq!(
                response
                    .headers()
                    .get(header::CACHE_CONTROL)
                    .and_then(|value| value.to_str().ok()),
                Some("no-store"),
                "{cookie}"
            );
            let cookies = set_cookies(&response);
            assert_eq!(
                body_json(response).await,
                serde_json::json!({ "paired": false, "error": "invalid_token" })
            );
            assert!(cookies
                .iter()
                .any(|cookie| cookie.starts_with("lampa_session=;")));
        }
    }

    #[tokio::test]
    async fn blank_cookie_is_401_invalid_token() {
        let (_temp, _database, state) = ready_state(true).await;
        let response = send(state, Some("device_token=")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "paired": false, "error": "invalid_token" })
        );
    }

    #[tokio::test]
    async fn b_strict_does_not_accept_the_legacy_cookie() {
        let (_temp, database, state) = ready_state(false).await;
        seed_device(&database, DEVICE_TOKEN, "allowed").await;
        let response = send(state, Some(&format!("device_token={DEVICE_TOKEN}"))).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn database_unavailable_still_answers_401_not_404() {
        let state = AppState::new(
            config(true),
            DbState::Unavailable(crate::db::SchemaGuardError::plain(
                crate::db::SchemaErrorKind::MissingFile,
            )),
        );
        let response = send(state, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "paired": false, "error": "token_required" })
        );
    }

    #[tokio::test]
    async fn session_does_not_update_last_seen() {
        let (_temp, database, state) = ready_state(true).await;
        seed_device(&database, DEVICE_TOKEN, "allowed").await;
        let response = send(state, Some(&format!("device_token={DEVICE_TOKEN}"))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let last_seen: Option<i64> = sqlx::query_scalar(
            "SELECT last_seen_at FROM device_access WHERE device_id = 'device-1'",
        )
        .fetch_one(database.pool())
        .await
        .expect("read last_seen");
        assert_eq!(last_seen, Some(1000), "session bootstrap must not write");
    }
}
