//! HTTP layer: router assembly and handlers.

pub mod health;

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
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::config::Config;

    fn test_state() -> AppState {
        let config = Config::from_lookup(|_| None).expect("default config");
        AppState::new(config)
    }

    async fn get_healthz() -> axum::response::Response {
        router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    #[tokio::test]
    async fn healthz_returns_200_with_json_status_ok() {
        let response = get_healthz().await;
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
    async fn unknown_route_returns_404() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn healthz_rejects_non_get_methods() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
