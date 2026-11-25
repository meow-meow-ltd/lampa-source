//! `GET /readyz` — readiness probe driven by the database startup guard.
//!
//! `/healthz` stays a liveness probe (no DB/engine); `/readyz` reports whether the
//! database passed the fail-closed guard at startup:
//!
//! * 200 `{"status":"ready","db":"ok","schema_version":7}` when ready;
//! * 503 `{"status":"not_ready","db":"unavailable","reason":"<code>"}` otherwise.
//!
//! The reason is a stable snake_case code from [`crate::db::SchemaErrorKind`];
//! raw SQL/driver diagnostics are never included.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

/// Body of the readiness response (fields are additive-only for deploy jobs).
#[derive(Debug, Serialize)]
pub struct ReadinessResponse {
    /// `"ready"` or `"not_ready"`.
    pub status: &'static str,
    /// `"ok"` or `"unavailable"`.
    pub db: &'static str,
    /// Guard-confirmed `schema_version.version` (absent when not ready or in lab mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<i64>,
    /// Sanitised guard failure code (absent when ready).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// Reports database readiness; 200 when the guard passed, 503 otherwise.
pub async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<ReadinessResponse>) {
    match (state.db().database(), state.db().unavailable_reason()) {
        (Some(database), _) => (
            StatusCode::OK,
            Json(ReadinessResponse {
                status: "ready",
                db: "ok",
                schema_version: database.guard_info().schema_version,
                reason: None,
            }),
        ),
        (None, reason) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadinessResponse {
                status: "not_ready",
                db: "unavailable",
                schema_version: None,
                reason: Some(reason.map(|error| error.code()).unwrap_or("unavailable")),
            }),
        ),
    }
}
