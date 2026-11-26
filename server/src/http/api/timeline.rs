//! Timeline route group — owner: T08.
//!
//! Frozen paths (T03 golden fixtures 21–23): `GET /api/timeline/dump`,
//! `GET /api/timeline/changelog`, `POST /api/timeline/update`. Device cookie
//! (`device_token`) is enforced by [`crate::http::middleware::auth_guard`].
//!
//! Legacy semantics preserved (`backend/routes/timeline.js`):
//! * `dump` answers `Content-Type: text/plain; charset=utf-8` while the body is
//!   JSON and `version` is `MAX(timeline_events.id)` for the profile;
//! * both reads return `{"version":N,"timelines":{hash: progress}}`; the stored
//!   JSON text is parsed and its `hash` member is forced to the row's hash
//!   (invalid JSON degrades to `{"hash":…}`, exactly like the legacy
//!   `parseData`);
//! * changelog has no row limit (the live database holds tens of thousands of
//!   events and the legacy route never truncates);
//! * `update` reads `body.params || body`, requires a non-empty string `hash`,
//!   defaults `percent`/`time`/`duration` through `value || 0`, stores
//!   `payload.profile || profileId`, upserts `(profile_id, hash)` and appends a
//!   `timeline_events` row. The `hash` column is TEXT and is stored verbatim.
//!
//! Deliberate W1 deviations (plan §3.6 п.6): database failures are logged
//! server-side and answered with `{"secuses":false,"error":"internal_error"}`;
//! raw SQL/driver text is never serialised.
//!
//! The repository lives in `src/timeline/**` (declared with `#[path]`; the crate
//! root is outside the T08 write scope). Request-body helpers mirror the
//! bookmarks handler — both legacy groups accept JSON and flat urlencoded bodies
//! regardless of `Content-Type`.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Serialize;

use crate::state::AppState;

#[path = "../../timeline/mod.rs"]
mod timeline_repo;

use timeline_repo::json::{self, Json};
use timeline_repo::{TimelineEntry, TimelineProgress, TimelineRepo};

/// Maximum accepted request body (progress payloads are small).
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Mounts the three timeline routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/timeline/dump", get(dump))
        .route("/api/timeline/changelog", get(changelog))
        .route("/api/timeline/update", post(update))
}

#[derive(Serialize)]
struct Success {
    secuses: bool,
}

#[derive(Serialize)]
struct FailureText {
    secuses: bool,
    text: &'static str,
}

#[derive(Serialize)]
struct FailureError {
    secuses: bool,
    error: &'static str,
}

/// `GET /api/timeline/dump`.
async fn dump(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(repo) = repo(&state) else {
        return database_unavailable();
    };
    let profile_id = header_profile_id(&headers);
    if profile_id == 0 {
        return missing_profile();
    }

    let version = match repo.version(profile_id).await {
        Ok(version) => version,
        Err(error) => return database_error(&error),
    };
    let entries = match repo.dump(profile_id).await {
        Ok(entries) => entries,
        Err(error) => return database_error(&error),
    };
    text_plain(timelines_response(version, &entries).to_compact_string())
}

/// `GET /api/timeline/changelog?since=`.
async fn changelog(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    let Some(repo) = repo(&state) else {
        return database_unavailable();
    };
    let profile_id = header_profile_id(&headers);
    if profile_id == 0 {
        return missing_profile();
    }
    let since = query_param(&uri, "since")
        .and_then(|raw| json::parse_int_str(&raw))
        .unwrap_or(0);

    let version = match repo.version(profile_id).await {
        Ok(version) => version,
        Err(error) => return database_error(&error),
    };
    let entries = match repo.changelog(profile_id, since).await {
        Ok(entries) => entries,
        Err(error) => return database_error(&error),
    };
    json_text(timelines_response(version, &entries).to_compact_string())
}

/// Builds the legacy `{version, timelines}` body.
fn timelines_response(version: i64, entries: &[TimelineEntry]) -> Json {
    let mut timelines: Vec<(String, Json)> = Vec::new();
    for entry in entries {
        let data = parse_data(&entry.hash, &entry.data);
        json::insert_member(&mut timelines, entry.hash.clone(), data);
    }
    Json::Object(vec![
        ("version".to_owned(), Json::Number(version.to_string())),
        ("timelines".to_owned(), Json::Object(timelines)),
    ])
}

/// Legacy `parseData`: JSON.parse, force `hash`, fall back to `{hash}`.
fn parse_data(hash: &str, data: &str) -> Json {
    match Json::parse(data) {
        Some(Json::Object(mut entries)) => {
            json::insert_member(
                &mut entries,
                "hash".to_owned(),
                Json::String(hash.to_owned()),
            );
            Json::Object(entries)
        }
        Some(other) => other,
        None => Json::Object(vec![("hash".to_owned(), Json::String(hash.to_owned()))]),
    }
}

/// `POST /api/timeline/update`.
async fn update(State(state): State<AppState>, headers: HeaderMap, request: Request) -> Response {
    let Some(repo) = repo(&state) else {
        return database_unavailable();
    };
    let body = match parse_body(request).await {
        Ok(body) => body,
        Err(BodyError::TooLarge) => return payload_too_large(),
        Err(BodyError::Malformed) => return invalid_payload(),
    };
    let profile_id = resolve_profile_id(&headers, &body);
    if profile_id == 0 {
        return missing_profile();
    }

    // `const payload = req.body?.params || req.body || {}`.
    let payload = match body.get("params") {
        Some(value) if value.is_truthy() => value,
        _ => &body,
    };
    let hash = payload.get("hash").and_then(Json::as_str).unwrap_or("");
    if hash.is_empty() {
        return missing_hash();
    }

    let percent = value_or_zero(payload, "percent");
    let time = value_or_zero(payload, "time");
    let duration = value_or_zero(payload, "duration");
    // `payload.profile || profileId` — the JSON copy may differ from the row.
    let profile = match payload.get("profile") {
        Some(value) if value.is_truthy() => value.clone(),
        _ => Json::Number(profile_id.to_string()),
    };

    let progress = TimelineProgress {
        hash,
        percent: &percent,
        time: &time,
        duration: &duration,
        profile: &profile,
    };
    match repo.update(profile_id, &progress).await {
        Ok(()) => secuses_true(),
        Err(error) => database_error(&error),
    }
}

/// `payload[key] || 0` with the legacy truthiness rules.
fn value_or_zero(payload: &Json, key: &str) -> Json {
    match payload.get(key) {
        Some(value) if value.is_truthy() => value.clone(),
        _ => Json::Number("0".to_owned()),
    }
}

fn repo(state: &AppState) -> Option<TimelineRepo> {
    state
        .db()
        .database()
        .map(|database| TimelineRepo::new(database.pool().clone()))
}

fn header_profile_id(headers: &HeaderMap) -> i64 {
    header_value(headers, "profile")
        .or_else(|| header_value(headers, "x-profile"))
        .map(|raw| json::parse_int_str(raw).unwrap_or(0))
        .unwrap_or(0)
}

/// Legacy `profileIdFromReq`: header `profile` → `x-profile` → `body.profile`
/// → `body.params.profile`.
fn resolve_profile_id(headers: &HeaderMap, body: &Json) -> i64 {
    if let Some(raw) =
        header_value(headers, "profile").or_else(|| header_value(headers, "x-profile"))
    {
        return json::parse_int_str(raw).unwrap_or(0);
    }
    if let Some(value) = body.get("profile") {
        if value.is_truthy() {
            return value.js_parse_int().unwrap_or(0);
        }
    }
    body.get("params")
        .and_then(|params| params.get("profile"))
        .and_then(Json::js_parse_int)
        .unwrap_or(0)
}

fn header_value<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
}

/// Reads the legacy `since` query parameter (percent-decoded).
fn query_param(uri: &Uri, name: &str) -> Option<String> {
    let query = uri.query()?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if form_decode(key) == name {
            return Some(form_decode(value));
        }
    }
    None
}

/// Body parsing outcome: transport limit or malformed payload.
enum BodyError {
    TooLarge,
    Malformed,
}

/// Reads the request body and parses it as JSON or flat urlencoded fields.
async fn parse_body(request: Request) -> Result<Json, BodyError> {
    let bytes = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|_| BodyError::TooLarge)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| BodyError::Malformed)?;
    parse_text_body(text)
}

fn parse_text_body(text: &str) -> Result<Json, BodyError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Json::Object(Vec::new()));
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Json::parse(trimmed).ok_or(BodyError::Malformed);
    }
    Ok(parse_form(trimmed))
}

/// Flat `application/x-www-form-urlencoded` parser (jQuery `$.param` shape).
fn parse_form(text: &str) -> Json {
    let mut entries = Vec::new();
    for pair in text.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = form_decode(key);
        if key.is_empty() {
            continue;
        }
        json::insert_member(&mut entries, key, Json::String(form_decode(value)));
    }
    Json::Object(entries)
}

/// Minimal `application/x-www-form-urlencoded` value decoder.
fn form_decode(value: &str) -> String {
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

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn text_plain(body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    no_store(response)
}

fn json_text(body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    no_store(response)
}

fn json_response<T: Serialize>(body: T) -> Response {
    no_store(axum::Json(body).into_response())
}

fn secuses_true() -> Response {
    json_response(Success { secuses: true })
}

fn missing_profile() -> Response {
    json_response_failure(
        StatusCode::BAD_REQUEST,
        FailureText {
            secuses: false,
            text: "Missing profile",
        },
    )
}

fn missing_hash() -> Response {
    json_response_failure(
        StatusCode::BAD_REQUEST,
        FailureText {
            secuses: false,
            text: "Missing hash",
        },
    )
}

fn invalid_payload() -> Response {
    json_response_failure(
        StatusCode::BAD_REQUEST,
        FailureText {
            secuses: false,
            text: "Invalid payload",
        },
    )
}

fn payload_too_large() -> Response {
    json_response_failure(
        StatusCode::PAYLOAD_TOO_LARGE,
        FailureText {
            secuses: false,
            text: "Payload too large",
        },
    )
}

fn database_unavailable() -> Response {
    json_response_failure(
        StatusCode::SERVICE_UNAVAILABLE,
        FailureError {
            secuses: false,
            error: "database_unavailable",
        },
    )
}

fn database_error(error: &crate::db::DbError) -> Response {
    tracing::error!(
        code = error.code(),
        "timeline API database error (details logged server-side only)"
    );
    json_response_failure(
        StatusCode::INTERNAL_SERVER_ERROR,
        FailureError {
            secuses: false,
            error: "internal_error",
        },
    )
}

fn json_response_failure<T: Serialize>(status: StatusCode, body: T) -> Response {
    no_store((status, axum::Json(body)).into_response())
}
