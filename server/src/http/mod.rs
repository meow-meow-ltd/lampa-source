//! HTTP layer: router assembly and handlers.

pub mod api;
pub mod health;
pub mod middleware;
pub mod ready;

use axum::routing::get;
use axum::Router;

use crate::state::AppState;

/// Builds the application router.
///
/// Kept as a pure function so tests can drive it through
/// `tower::ServiceExt::oneshot` without binding a socket.
///
/// The W1 API registry (`api`) is mounted exactly once and wrapped in the
/// placeholder auth guard from `middleware` with `route_layer`, so the guard
/// runs only for matched API routes (unknown paths keep their 404) and T10
/// replaces that guard's body without touching this file. `/healthz` and
/// `/readyz` stay outside the guard (liveness/readiness must not depend on
/// credentials) and are byte-for-byte unchanged.
pub fn router(state: AppState) -> Router {
    let api = api::router().route_layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::auth_guard,
    ));
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(ready::readyz))
        .merge(api)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::db::test_support::TempDb;
    use crate::db::Database;
    use crate::state::DbState;

    fn test_config() -> Config {
        Config::from_lookup(|_| None).expect("default config")
    }

    async fn unavailable_state() -> AppState {
        // A database that does not exist: the guard fails closed, the process
        // would still serve /healthz and report 503 on /readyz.
        let temp = TempDb::empty();
        let missing = temp.path("missing.sqlite");
        let url = TempDb::url_for(&missing);
        let error = Database::open(&url, false)
            .await
            .expect_err("missing database must fail the guard");
        AppState::new(test_config(), DbState::Unavailable(error))
    }

    async fn get(state: AppState, uri: &str) -> axum::response::Response {
        router(state)
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("body bytes");
        serde_json::from_slice(&body).expect("json body")
    }

    /// Sends a bodyless request, optionally carrying one `Cookie` header.
    async fn send(
        state: AppState,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        router(state)
            .oneshot(builder.body(Body::empty()).expect("request"))
            .await
            .expect("response")
    }

    /// Auth class the guard must apply to a route (mirrors the T03 fixtures).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Auth {
        Public,
        Device,
        Admin,
    }

    /// Frozen W1 route table: the 27 T03 legacy routes + T10 session probe +
    /// T11 maintenance backup. The URI is the exact request target used by the
    /// fixture (query strings included). No route may appear twice: axum
    /// panics when the same method+path is mounted twice, and every test here
    /// builds the router.
    const ROUTES: &[(Auth, &str, &str)] = &[
        (Auth::Public, "POST", "/api/pin/verify"),
        (Auth::Admin, "GET", "/api/admin/devices"),
        (Auth::Admin, "POST", "/api/admin/device/action"),
        (Auth::Admin, "GET", "/api/settings"),
        (Auth::Admin, "PUT", "/api/settings"),
        (Auth::Admin, "DELETE", "/api/settings/head_notice"),
        (Auth::Device, "POST", "/api/values"),
        (Auth::Device, "GET", "/api/value?key=admin_password"),
        (Auth::Device, "GET", "/api/profiles/all"),
        (Auth::Device, "POST", "/api/profiles/create"),
        (Auth::Device, "POST", "/api/profiles/update"),
        (Auth::Device, "POST", "/api/profiles/delete"),
        (Auth::Device, "GET", "/api/profile/settings"),
        (Auth::Device, "POST", "/api/profile/settings"),
        (Auth::Device, "GET", "/api/bookmarks/dump"),
        (Auth::Device, "GET", "/api/bookmarks/changelog?since=0"),
        (Auth::Device, "POST", "/api/bookmarks/add"),
        (Auth::Device, "POST", "/api/bookmarks/remove"),
        (Auth::Device, "POST", "/api/bookmarks/clear"),
        (Auth::Device, "POST", "/api/bookmarks/sync"),
        (Auth::Device, "GET", "/api/timeline/dump"),
        (Auth::Device, "GET", "/api/timeline/changelog?since=0"),
        (Auth::Device, "POST", "/api/timeline/update"),
        (Auth::Device, "POST", "/api/users/backup/export"),
        (Auth::Device, "GET", "/api/users/backup/import"),
        (Auth::Public, "POST", "/admin/login"),
        (Auth::Public, "POST", "/admin/logout"),
        (Auth::Public, "GET", "/api/auth/session"),
        (Auth::Admin, "POST", "/api/admin/maintenance/backup"),
    ];

    fn cookie_for(auth: Auth) -> Option<&'static str> {
        match auth {
            Auth::Public => None,
            Auth::Device => Some("device_token=stub-token"),
            Auth::Admin => Some("auth_token=stub-token"),
        }
    }

    #[tokio::test]
    async fn healthz_returns_200_with_json_status_ok() {
        let response = get(unavailable_state().await, "/healthz").await;
        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(
            content_type.starts_with("application/json"),
            "unexpected content-type: {content_type}"
        );

        let body = to_bytes(response.into_body(), 1024)
            .await
            .expect("body bytes");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json, serde_json::json!({ "status": "ok" }));
        assert_eq!(body.as_ref(), br#"{"status":"ok"}"#.as_slice());
    }

    #[tokio::test]
    async fn healthz_stays_200_even_when_the_database_guard_failed() {
        // Explicit regression test for "healthz stays DB-independent".
        let response = get(unavailable_state().await, "/healthz").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "status": "ok" })
        );
    }

    #[tokio::test]
    async fn readyz_reports_503_with_a_sanitised_reason_when_guard_failed() {
        let response = get(unavailable_state().await, "/readyz").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let json = body_json(response).await;
        assert_eq!(
            json,
            serde_json::json!({
                "status": "not_ready",
                "db": "unavailable",
                "reason": "missing_file",
            })
        );
        assert!(!json.to_string().contains("INSERT"));
        assert!(!json.to_string().contains("SELECT"));
    }

    #[tokio::test]
    async fn readyz_reports_200_and_the_schema_version_when_ready() {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        let state = AppState::new(test_config(), DbState::Ready(database.clone()));

        let response = get(state, "/readyz").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({
                "status": "ready",
                "db": "ok",
                "schema_version": 7,
            })
        );
        database.close().await;
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let response = get(unavailable_state().await, "/nope").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn probes_reject_non_get_methods() {
        for uri in ["/healthz", "/readyz"] {
            let response = router(unavailable_state().await)
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{uri} must only accept GET"
            );
        }
    }

    #[test]
    fn route_table_is_27_legacy_plus_the_two_w1_additions() {
        assert_eq!(ROUTES.len(), 29);
        let legacy = ROUTES
            .iter()
            .filter(|(_, _, uri)| {
                !matches!(*uri, "/api/auth/session" | "/api/admin/maintenance/backup")
            })
            .count();
        assert_eq!(legacy, 27, "27 legacy routes from the T03 golden fixtures");
    }

    #[tokio::test]
    async fn every_w1_route_is_mounted() {
        // Mount-only contract: once handlers are implemented, a mounted route
        // may answer 200/401/503; it must never fall through to the 404
        // fallback. Status-specific behaviour is covered by the handler tests.
        let state = unavailable_state().await;
        for (auth, method, uri) in ROUTES {
            let response = send(state.clone(), method, uri, cookie_for(*auth)).await;
            assert_ne!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{method} {uri} must be mounted"
            );
        }
    }

    #[tokio::test]
    async fn protected_w1_routes_require_a_credential() {
        let state = unavailable_state().await;
        for (auth, method, uri) in ROUTES {
            if *auth == Auth::Public {
                continue;
            }
            let response = send(state.clone(), method, uri, None).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} must reject a missing credential"
            );
            assert_eq!(
                body_json(response).await,
                serde_json::json!({ "error": "token_required" }),
                "{method} {uri}"
            );
        }
    }

    #[tokio::test]
    async fn blank_credentials_are_reported_as_invalid_tokens() {
        let state = unavailable_state().await;
        for (auth, method, uri) in ROUTES {
            let cookie = match auth {
                Auth::Public => continue,
                Auth::Device => "device_token=",
                Auth::Admin => "auth_token=",
            };
            let response = send(state.clone(), method, uri, Some(cookie)).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} must reject a blank credential"
            );
            assert_eq!(
                body_json(response).await,
                serde_json::json!({ "error": "invalid_token" }),
                "{method} {uri}"
            );
        }
    }
}
