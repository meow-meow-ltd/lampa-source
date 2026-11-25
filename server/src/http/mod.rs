//! HTTP layer: router assembly and handlers.

pub mod health;
pub mod ready;

use axum::routing::get;
use axum::Router;

use crate::state::AppState;

/// Builds the application router.
///
/// Kept as a pure function so tests can drive it through
/// `tower::ServiceExt::oneshot` without binding a socket.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(ready::readyz))
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
}
