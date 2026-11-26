//! Authentication guard — real device/admin validation (T10).
//!
//! [`auth_guard`] is the middleware installed exactly once by
//! [`crate::http::router`] around the API registry. Auth classes are frozen by
//! the W1 route table / T03 fixtures:
//!
//! | Class | Paths | Required credential |
//! |---|---|---|
//! | public | `POST /api/pin/verify`, `GET /api/auth/session`, `POST /admin/login`, `POST /admin/logout` | none |
//! | admin | `GET`/`PUT /api/settings`, `DELETE /api/settings/{key}`, `/api/admin/*` | `auth_token` == `settings.admin_token` |
//! | device | every other mounted `/api/*` route | `device_token`/`lampa_session` → SHA-256 → `device_access.token_hash` with `status='allowed'` |
//!
//! Failure contract (unchanged from the I0 scaffold):
//! * credential missing → `401 {"error":"token_required"}`
//! * credential blank/unknown/revoked → `401 {"error":"invalid_token"}` and the
//!   device/admin cookies are expired in the same response;
//! * **never 403 for a blocked device** — the PIN gate must remain reachable.
//!
//! Transitional C (D6/S8): when `DEVICE_LEGACY_COOKIE=1` the guard accepts both
//! `lampa_session` and the readable legacy `device_token`; `0` makes it
//! B-strict (`lampa_session` only). Admin routes always use `auth_token`.
//!
//! On an authenticated device request the guard inserts
//! [`crate::auth::AuthenticatedDevice`] into the request extensions for the
//! handlers (and W3 media auth), and refreshes `last_seen_at` at most once per
//! 30 s. Credential values are never logged.

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use tracing::debug;

use crate::auth::cookie::{self, LAMPA_SESSION_COOKIE};
use crate::auth::{self, AdminAuth, DeviceAuth};
use crate::state::AppState;

// Keep the I0 constant names available to route-group owners.
pub use crate::auth::cookie::{ADMIN_TOKEN_COOKIE, DEVICE_TOKEN_COOKIE};

/// Authentication class of a mounted route, derived from its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthClass {
    /// No credential required (PIN bootstrap, session probe, admin login/logout).
    Public,
    /// `device_token`/`lampa_session` cookie required (regular device API).
    Device,
    /// `auth_token` cookie required (settings/admin API).
    Admin,
}

/// Classifies a request path.
///
/// The guard only runs for routes mounted by [`crate::http::api::router`], so
/// every path reaching it is one of the three classes above; the fallback is
/// `Device` (the safest non-public default) for defence in depth.
pub(crate) fn auth_class(path: &str) -> AuthClass {
    match path {
        "/api/pin/verify" | "/api/auth/session" | "/admin/login" | "/admin/logout" => {
            AuthClass::Public
        }
        _ if path == "/api/settings" || path.starts_with("/api/settings/") => AuthClass::Admin,
        _ if path == "/api/admin" || path.starts_with("/api/admin/") => AuthClass::Admin,
        _ => AuthClass::Device,
    }
}

/// Final middleware signature installed by `http::router`.
pub async fn auth_guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    match auth_class(request.uri().path()) {
        AuthClass::Public => next.run(request).await,
        AuthClass::Device => guard_device(&state, request, next).await,
        AuthClass::Admin => guard_admin(&state, request, next).await,
    }
}

/// Device routes: cookie → SHA-256 → `device_access` (`status='allowed'`).
async fn guard_device(state: &AppState, mut request: Request, next: Next) -> Response {
    // Transitional C accepts both names (session first); B-strict only the
    // HttpOnly session cookie.
    let names: &[&str] = if state.auth().legacy_cookie_enabled() {
        &[LAMPA_SESSION_COOKIE, DEVICE_TOKEN_COOKIE]
    } else {
        &[LAMPA_SESSION_COOKIE]
    };
    let presented = cookie::find(request.headers(), names);
    match state
        .auth()
        .authenticate_device(presented, auth::now_millis())
        .await
    {
        DeviceAuth::Allowed(device) => {
            request.extensions_mut().insert(device);
            next.run(request).await
        }
        DeviceAuth::Missing => {
            log_rejection(&request, "missing");
            unauthorized("token_required", cookie::clear_device_cookies())
        }
        DeviceAuth::Empty | DeviceAuth::Invalid => {
            log_rejection(&request, "invalid");
            unauthorized("invalid_token", cookie::clear_device_cookies())
        }
        DeviceAuth::Unavailable | DeviceAuth::Error => {
            // Fail closed with the frozen 401 contract while readiness reports
            // 503; no data route can serve without the guarded pool.
            log_rejection(&request, "unavailable");
            unauthorized("invalid_token", cookie::clear_device_cookies())
        }
    }
}

/// Admin routes: `auth_token` vs `settings.admin_token` (constant-time).
async fn guard_admin(state: &AppState, request: Request, next: Next) -> Response {
    let presented = cookie::find(request.headers(), &[ADMIN_TOKEN_COOKIE]);
    match state.auth().authenticate_admin(presented).await {
        AdminAuth::Allowed => next.run(request).await,
        AdminAuth::Missing => {
            log_rejection(&request, "missing");
            unauthorized("token_required", vec![cookie::clear_admin_cookie()])
        }
        AdminAuth::Empty | AdminAuth::Invalid => {
            log_rejection(&request, "invalid");
            unauthorized("invalid_token", vec![cookie::clear_admin_cookie()])
        }
        AdminAuth::Unavailable | AdminAuth::Error => {
            log_rejection(&request, "unavailable");
            unauthorized("invalid_token", vec![cookie::clear_admin_cookie()])
        }
    }
}

/// One non-secret rejection log line (path + reason only).
fn log_rejection(request: &Request, reason: &'static str) {
    debug!(
        path = request.uri().path(),
        status = "rejected",
        reason,
        "auth guard"
    );
}

/// `401` JSON response with the stable legacy error code plus cookie expiry.
fn unauthorized(error: &'static str, cookies: Vec<String>) -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, Json(json!({ "error": error }))).into_response();
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request as HttpRequest;
    use axum::routing::{get, post};
    use axum::Router;
    use tower::ServiceExt;

    use crate::auth::device::hash_token;
    use crate::config::Config;
    use crate::db::test_support::TempDb;
    use crate::db::{Database, SchemaErrorKind, SchemaGuardError};
    use crate::state::DbState;

    const ADMIN_TOKEN: &str = "admin-token-for-tests";
    const ALLOWED_TOKEN: &str = "known-token";

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

    async fn unavailable_state() -> AppState {
        AppState::new(
            config(true),
            DbState::Unavailable(SchemaGuardError::plain(SchemaErrorKind::MissingFile)),
        )
    }

    /// Minimal router that mounts the guard over throw-away handlers, so these
    /// tests never depend on the T07/T08/T09/T11 route-group bodies.
    fn guard_router(state: AppState) -> Router {
        async fn ok() -> StatusCode {
            StatusCode::OK
        }
        Router::new()
            .route("/api/values", post(ok))
            .route("/api/settings", get(ok))
            .route("/admin/login", post(ok))
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth_guard,
            ))
            .with_state(state)
    }

    async fn seed_device(database: &Database, device_id: &str, token: &str, status: &str) {
        sqlx::query(
            "INSERT INTO device_access(device_id, status, attempts, token_hash, last_seen_at) \
             VALUES(?, ?, 0, ?, NULL)",
        )
        .bind(device_id)
        .bind(status)
        .bind(hash_token(token))
        .execute(database.pool())
        .await
        .expect("seed device");
    }

    async fn seed_admin(database: &Database, token: &str) {
        sqlx::query("INSERT INTO settings(key, value, visible) VALUES('admin_token', ?, 0)")
            .bind(token)
            .execute(database.pool())
            .await
            .expect("seed admin token");
    }

    async fn send(state: AppState, method: &str, uri: &str, cookie: Option<&str>) -> Response {
        let mut builder = HttpRequest::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        guard_router(state)
            .oneshot(builder.body(Body::empty()).expect("request"))
            .await
            .expect("response")
    }
    async fn body_json(response: Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("body bytes");
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

    #[test]
    fn auth_classes_match_the_w1_contract() {
        for path in [
            "/api/pin/verify",
            "/api/auth/session",
            "/admin/login",
            "/admin/logout",
        ] {
            assert_eq!(auth_class(path), AuthClass::Public, "{path}");
        }
        for path in [
            "/api/settings",
            "/api/settings/head_notice",
            "/api/admin/devices",
            "/api/admin/device/action",
            "/api/admin/maintenance/backup",
        ] {
            assert_eq!(auth_class(path), AuthClass::Admin, "{path}");
        }
        for path in [
            "/api/values",
            "/api/value",
            "/api/profiles/all",
            "/api/profile/settings",
            "/api/bookmarks/dump",
            "/api/timeline/update",
            "/api/users/backup/export",
        ] {
            assert_eq!(auth_class(path), AuthClass::Device, "{path}");
        }
    }

    #[tokio::test]
    async fn public_routes_are_not_gated() {
        let (_temp, _database, state) = ready_state(true).await;
        let response = send(state, "POST", "/admin/login", None).await;
        assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn missing_device_cookie_is_token_required() {
        let (_temp, _database, state) = ready_state(true).await;
        let response = send(state, "POST", "/api/values", None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), 1024).await.expect("body");
        assert_eq!(body.as_ref(), br#"{"error":"token_required"}"#.as_slice());
    }

    #[tokio::test]
    async fn blank_device_cookie_is_invalid_token_and_clears_cookies() {
        let (_temp, _database, state) = ready_state(true).await;
        let response = send(state, "POST", "/api/values", Some("device_token=")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let cleared = set_cookies(&response);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "invalid_token" })
        );
        assert_eq!(cleared.len(), 2, "both device cookies must be expired");
        assert!(cleared
            .iter()
            .any(|cookie| cookie.starts_with("lampa_session=;")));
        assert!(cleared
            .iter()
            .any(|cookie| cookie.starts_with("device_token=;")));
        assert!(cleared.iter().all(|cookie| cookie.contains("Max-Age=0")));
    }

    #[tokio::test]
    async fn unknown_or_revoked_device_token_is_invalid_not_forbidden() {
        let (_temp, database, state) = ready_state(true).await;
        seed_device(&database, "device-pending", "pending-token", "pending").await;
        seed_device(&database, "device-blocked", "blocked-token", "blocked").await;

        for cookie in [
            "device_token=not-a-real-token",
            "device_token=pending-token",
            "device_token=blocked-token",
        ] {
            let response = send(state.clone(), "POST", "/api/values", Some(cookie)).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{cookie} must be 401 (never 403)"
            );
            assert_eq!(
                body_json(response).await,
                serde_json::json!({ "error": "invalid_token" })
            );
        }
    }

    #[tokio::test]
    async fn allowed_device_token_passes_both_cookie_names() {
        let (_temp, database, state) = ready_state(true).await;
        seed_device(&database, "device-allowed", ALLOWED_TOKEN, "allowed").await;
        for cookie in [
            format!("device_token={ALLOWED_TOKEN}"),
            format!("lampa_session={ALLOWED_TOKEN}"),
            format!("device_token={ALLOWED_TOKEN}; lampa_session={ALLOWED_TOKEN}"),
        ] {
            let response = send(state.clone(), "POST", "/api/values", Some(&cookie)).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{cookie} must reach the route handler"
            );
        }
    }

    #[tokio::test]
    async fn b_strict_rejects_the_legacy_cookie() {
        let (_temp, database, state) = ready_state(false).await;
        seed_device(&database, "device-allowed", ALLOWED_TOKEN, "allowed").await;
        let legacy = send(
            state.clone(),
            "POST",
            "/api/values",
            Some(&format!("device_token={ALLOWED_TOKEN}")),
        )
        .await;
        assert_eq!(legacy.status(), StatusCode::UNAUTHORIZED);
        let session = send(
            state,
            "POST",
            "/api/values",
            Some(&format!("lampa_session={ALLOWED_TOKEN}")),
        )
        .await;
        assert_eq!(session.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_routes_require_the_settings_admin_token() {
        let (_temp, database, state) = ready_state(true).await;
        seed_admin(&database, ADMIN_TOKEN).await;

        let valid = send(
            state.clone(),
            "GET",
            "/api/settings",
            Some(&format!("auth_token={ADMIN_TOKEN}")),
        )
        .await;
        assert_eq!(valid.status(), StatusCode::OK);

        let wrong = send(
            state.clone(),
            "GET",
            "/api/settings",
            Some("auth_token=wrong"),
        )
        .await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let cleared = set_cookies(&wrong);
        assert_eq!(
            body_json(wrong).await,
            serde_json::json!({ "error": "invalid_token" })
        );
        assert!(cleared
            .iter()
            .any(|cookie| cookie.starts_with("auth_token=;")));
        assert!(cleared.iter().any(|cookie| cookie.contains("HttpOnly")));

        let missing = send(state, "GET", "/api/settings", None).await;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(missing).await,
            serde_json::json!({ "error": "token_required" })
        );
    }

    #[tokio::test]
    async fn admin_routes_reject_a_device_cookie() {
        let (_temp, database, state) = ready_state(true).await;
        seed_device(&database, "device-allowed", ALLOWED_TOKEN, "allowed").await;
        seed_admin(&database, ADMIN_TOKEN).await;
        let response = send(
            state,
            "GET",
            "/api/settings",
            Some(&format!("device_token={ALLOWED_TOKEN}")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn database_unavailable_fails_closed_after_the_credential_check() {
        let state = unavailable_state().await;
        let missing = send(state.clone(), "POST", "/api/values", None).await;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(missing).await,
            serde_json::json!({ "error": "token_required" })
        );
        let present = send(
            state,
            "POST",
            "/api/values",
            Some("device_token=stub-token"),
        )
        .await;
        assert_eq!(present.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_json(present).await,
            serde_json::json!({ "error": "invalid_token" })
        );
    }

    #[tokio::test]
    async fn authenticated_device_requests_are_throttled_and_reach_the_handler() {
        let (_temp, database, state) = ready_state(true).await;
        seed_device(&database, "device-allowed", ALLOWED_TOKEN, "allowed").await;
        let response = send(
            state,
            "POST",
            "/api/values",
            Some(&format!("device_token={ALLOWED_TOKEN}")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let last_seen: Option<i64> = sqlx::query_scalar(
            "SELECT last_seen_at FROM device_access WHERE device_id = 'device-allowed'",
        )
        .fetch_one(database.pool())
        .await
        .expect("read last_seen");
        assert!(
            last_seen.is_some(),
            "first authenticated request stamps last_seen"
        );
    }
}
