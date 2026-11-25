//! `GET /healthz` — liveness probe.
//!
//! Intentionally database- and engine-free: it only proves that the process
//! can serve HTTP. Readiness checks (DB schema, media engine) belong to
//! `/readyz` in a later wave.

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

/// Body of the liveness response.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Always `"ok"` while the process serves HTTP.
    pub status: &'static str,
}

/// Returns `200 application/json {"status":"ok"}`.
pub async fn healthz(State(state): State<AppState>) -> Json<HealthResponse> {
    tracing::debug!(
        host = state.config().host(),
        port = state.config().port(),
        "healthz"
    );
    Json(HealthResponse { status: "ok" })
}
