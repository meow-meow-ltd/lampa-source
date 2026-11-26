//! Bookmarks route group — owner: T08.
//!
//! Frozen paths (T03 golden fixtures 15–20): `GET /api/bookmarks/dump`,
//! `GET /api/bookmarks/changelog`, `POST /api/bookmarks/{add,remove,clear,sync}`.
//! Device cookie (`device_token`) is enforced by
//! [`crate::http::middleware::auth_guard`].
//!
//! Legacy quirks intentionally preserved:
//! * `dump` answers `Content-Type: text/plain; charset=utf-8` while the body is
//!   JSON; `version` is `MAX(bookmark_events.id)` and the bookmark `data`
//!   column is an opaque JSON string returned byte-for-byte;
//! * `changelog` returns the legacy per-action shapes (`add` renames
//!   `entity_id` to `id`; `clear`/`remove` carry only `entity_id`;
//!   `update` adds `updated_at`; an unknown action degrades to `remove`);
//! * `clear` requires `type == "group"` and takes the bookmark type from
//!   `group`;
//! * mutations answer the typo-shaped `{"secuses":true}` and validation
//!   failures `{"secuses":false,"text":"…"}` with the legacy status codes;
//! * `sync` is one transaction (rollback leaves no partial bookmarks/events).
//!
//! Deliberate W1 deviations (plan §3.6 п.6): a database failure is logged
//! server-side and answered with the stable `{"secuses":false,"error":
//! "internal_error"}` — raw SQL/driver text never reaches the client.
//!
//! The repository lives in `src/bookmarks/**` (declared here with `#[path]`
//! because the crate root is outside the T08 write scope).

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Serialize;

use crate::state::AppState;

#[path = "../../bookmarks/mod.rs"]
mod bookmarks_repo;

use bookmarks_repo::json::{self, Json};
use bookmarks_repo::{BookmarkEvent, BookmarksRepo, EntityId};

/// Maximum accepted request body (bookmark sync payloads can be large).
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Mounts the six bookmarks routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/bookmarks/dump", get(dump))
        .route("/api/bookmarks/changelog", get(changelog))
        .route("/api/bookmarks/add", post(add))
        .route("/api/bookmarks/remove", post(remove))
        .route("/api/bookmarks/clear", post(clear))
        .route("/api/bookmarks/sync", post(sync))
}

/// `{"version":N,"bookmarks":[…]}` rendered into a `text/plain` response.
#[derive(Serialize)]
struct BookmarksDump {
    version: i64,
    bookmarks: Vec<BookmarkDto>,
}

/// One bookmark row; field order is the fixture order.
#[derive(Serialize)]
struct BookmarkDto {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
    card_id: i64,
    data: String,
    updated_at: i64,
}

/// `{"version":N,"changelog":[…]}`.
#[derive(Serialize)]
struct Changelog {
    version: i64,
    changelog: Vec<ChangelogEntry>,
}

/// Legacy per-action changelog shape; absent fields are omitted, not null.
#[derive(Serialize)]
struct ChangelogEntry {
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity_id: Option<serde_json::Value>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    card_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<i64>,
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

/// `GET /api/bookmarks/dump`.
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
    let bookmarks = match repo.dump(profile_id).await {
        Ok(bookmarks) => bookmarks,
        Err(error) => return database_error(&error),
    };
    let body = BookmarksDump {
        version,
        bookmarks: bookmarks
            .iter()
            .map(|bookmark| BookmarkDto {
                id: bookmark.id,
                kind: bookmark.kind.clone(),
                card_id: bookmark.card_id,
                data: bookmark.data.clone(),
                updated_at: bookmark.updated_at,
            })
            .collect(),
    };
    dump_response(&body)
}

/// `GET /api/bookmarks/changelog?since=`.
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
    let events = match repo.changelog(profile_id, since).await {
        Ok(events) => events,
        Err(error) => return database_error(&error),
    };
    json_response(Changelog {
        version,
        changelog: events.into_iter().map(changelog_entry).collect(),
    })
}

/// Maps one stored event to the legacy changelog object.
fn changelog_entry(event: BookmarkEvent) -> ChangelogEntry {
    let entity = entity_value(&event.entity_id);
    match event.action.as_str() {
        "add" => ChangelogEntry {
            action: "add".to_owned(),
            id: Some(entity),
            entity_id: None,
            kind: event.kind,
            card_id: event.card_id,
            data: event.data,
            updated_at: Some(event.updated_at),
        },
        "clear" => ChangelogEntry {
            action: "clear".to_owned(),
            id: None,
            entity_id: Some(entity),
            kind: None,
            card_id: None,
            data: None,
            updated_at: None,
        },
        "update" => ChangelogEntry {
            action: "update".to_owned(),
            id: None,
            entity_id: Some(entity),
            kind: None,
            card_id: None,
            data: None,
            updated_at: Some(event.updated_at),
        },
        // Legacy default branch: anything else is reported as a removal.
        _ => ChangelogEntry {
            action: "remove".to_owned(),
            id: None,
            entity_id: Some(entity),
            kind: None,
            card_id: None,
            data: None,
            updated_at: None,
        },
    }
}

fn entity_value(entity: &EntityId) -> serde_json::Value {
    match entity {
        EntityId::Int(value) => serde_json::Value::from(*value),
        EntityId::Text(value) => serde_json::Value::String(value.clone()),
        EntityId::Null => serde_json::Value::Null,
    }
}

/// `POST /api/bookmarks/add`.
async fn add(State(state): State<AppState>, headers: HeaderMap, request: Request) -> Response {
    let Some(repo) = repo(&state) else {
        return database_unavailable();
    };
    let body = match parse_body(request).await {
        Ok(body) => body,
        Err(BodyError::TooLarge) => return payload_too_large(),
        Err(BodyError::Malformed) => return invalid_payload(),
    };
    let profile_id = profile_id(&headers, &body);
    if profile_id == 0 {
        return missing_profile();
    }

    let kind = body
        .get("type")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let Some(card_id) = body.get("card_id").and_then(Json::js_parse_int) else {
        return invalid_payload();
    };
    if kind.is_empty() {
        return invalid_payload();
    }
    let Some(data) = body.get("data").and_then(Json::as_str) else {
        return missing_data();
    };
    if data.is_empty() {
        return missing_data();
    }

    match repo.add(profile_id, &kind, card_id, data).await {
        Ok(()) => secuses_true(),
        Err(error) => database_error(&error),
    }
}

/// `POST /api/bookmarks/remove`.
async fn remove(State(state): State<AppState>, headers: HeaderMap, request: Request) -> Response {
    let Some(repo) = repo(&state) else {
        return database_unavailable();
    };
    let body = match parse_body(request).await {
        Ok(body) => body,
        Err(BodyError::TooLarge) => return payload_too_large(),
        Err(BodyError::Malformed) => return invalid_payload(),
    };
    let profile_id = profile_id(&headers, &body);
    if profile_id == 0 {
        return missing_profile();
    }

    let kind = body
        .get("type")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let Some(card_id) = body.get("card_id").and_then(Json::js_parse_int) else {
        return invalid_payload();
    };
    if kind.is_empty() {
        return invalid_payload();
    }
    let existing = body.get("id").and_then(Json::js_parse_int).unwrap_or(0);
    let existing_id = (existing != 0).then_some(existing);

    match repo.remove(profile_id, &kind, card_id, existing_id).await {
        Ok(()) => secuses_true(),
        Err(error) => database_error(&error),
    }
}

/// `POST /api/bookmarks/clear` (legacy requires `type: "group"`).
async fn clear(State(state): State<AppState>, headers: HeaderMap, request: Request) -> Response {
    let Some(repo) = repo(&state) else {
        return database_unavailable();
    };
    let body = match parse_body(request).await {
        Ok(body) => body,
        Err(BodyError::TooLarge) => return payload_too_large(),
        Err(BodyError::Malformed) => return invalid_payload(),
    };
    let profile_id = profile_id(&headers, &body);
    if profile_id == 0 {
        return missing_profile();
    }

    let kind = body
        .get("type")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .trim();
    let group = body
        .get("group")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .trim();
    if kind != "group" || group.is_empty() {
        return invalid_payload();
    }

    match repo.clear(profile_id, group).await {
        Ok(()) => secuses_true(),
        Err(error) => database_error(&error),
    }
}

/// `POST /api/bookmarks/sync`.
async fn sync(State(state): State<AppState>, headers: HeaderMap, request: Request) -> Response {
    let Some(repo) = repo(&state) else {
        return database_unavailable();
    };
    let body = match parse_body(request).await {
        Ok(body) => body,
        Err(BodyError::TooLarge) => return payload_too_large(),
        Err(BodyError::Malformed) => return invalid_payload(),
    };
    let profile_id = profile_id(&headers, &body);
    if profile_id == 0 {
        return missing_profile();
    }

    // `raw ? JSON.parse(raw) : {}` — only a string `data` field is accepted.
    let raw = body.get("data").and_then(Json::as_str).unwrap_or("");
    let payload = if raw.is_empty() {
        Json::Object(Vec::new())
    } else {
        match Json::parse(raw) {
            Some(payload) => payload,
            None => return invalid_json(),
        }
    };

    match repo.sync(profile_id, &payload).await {
        Ok(()) => secuses_true(),
        Err(error) => database_error(&error),
    }
}

fn repo(state: &AppState) -> Option<BookmarksRepo> {
    state
        .db()
        .database()
        .map(|database| BookmarksRepo::new(database.pool().clone()))
}

fn header_profile_id(headers: &HeaderMap) -> i64 {
    header_value(headers, "profile")
        .or_else(|| header_value(headers, "x-profile"))
        .map(|raw| json::parse_int_str(raw).unwrap_or(0))
        .unwrap_or(0)
}

/// Legacy `profileIdFromReq`: header `profile` → header `x-profile` → `body.profile`.
fn profile_id(headers: &HeaderMap, body: &Json) -> i64 {
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
    0
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
///
/// `Content-Type` is deliberately irrelevant (plan §3.6): the Lampa client sends
/// JSON, jQuery form posts and fixture requests that do not always agree on it,
/// and the legacy body-parser chain accepted both.
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

/// Serialises one bookmarks dump without panicking.
///
/// The plain fields are expected to serialise, but a production response path
/// must never panic: a serializer failure becomes the same stable,
/// sanitised `500 {"secuses":false,"error":"internal_error"}` body as a database
/// failure (no serde/driver text, `Cache-Control: no-store`).
fn dump_response<T: Serialize>(body: &T) -> Response {
    match serialize_dump(body) {
        Some(serialised) => text_plain(serialised),
        None => serialization_error(),
    }
}

/// Non-panicking serialization helper shared by the dump path (unit-tested with
/// a deliberately failing `Serialize` implementation).
fn serialize_dump<T: Serialize>(body: &T) -> Option<String> {
    serde_json::to_string(body).ok()
}

/// Sanitised response for a response-serialization failure.
fn serialization_error() -> Response {
    tracing::error!(
        "bookmarks API response serialization failed (details logged server-side only)"
    );
    json_response_failure(
        StatusCode::INTERNAL_SERVER_ERROR,
        FailureError {
            secuses: false,
            error: "internal_error",
        },
    )
}

fn text_plain(body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
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

fn invalid_payload() -> Response {
    json_response_failure(
        StatusCode::BAD_REQUEST,
        FailureText {
            secuses: false,
            text: "Invalid payload",
        },
    )
}

fn missing_data() -> Response {
    json_response_failure(
        StatusCode::BAD_REQUEST,
        FailureText {
            secuses: false,
            text: "Missing data",
        },
    )
}

fn invalid_json() -> Response {
    json_response_failure(
        StatusCode::BAD_REQUEST,
        FailureText {
            secuses: false,
            text: "Invalid JSON",
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
        "bookmarks API database error (details logged server-side only)"
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// A `Serialize` impl that always fails (only the failure path is exercised).
    struct FailingSerialize;

    impl Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(<S::Error as serde::ser::Error>::custom(
                "intentional serializer failure",
            ))
        }
    }

    fn sample_dump() -> BookmarksDump {
        BookmarksDump {
            version: 3,
            bookmarks: vec![BookmarkDto {
                id: 1,
                kind: "book".to_owned(),
                card_id: 2,
                data: "{}".to_owned(),
                updated_at: 4,
            }],
        }
    }

    #[test]
    fn dump_body_serialises_with_the_fixture_field_order() {
        let serialised = serialize_dump(&sample_dump()).expect("plain fields serialise");
        assert_eq!(
            serialised,
            r#"{"version":3,"bookmarks":[{"id":1,"type":"book","card_id":2,"data":"{}","updated_at":4}]}"#
        );
    }

    #[tokio::test]
    async fn serialization_failure_maps_to_a_sanitized_stable_error() {
        let response = dump_response(&FailingSerialize);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let body = to_bytes(response.into_body(), 1024)
            .await
            .expect("error body");
        assert_eq!(
            body.as_ref(),
            br#"{"secuses":false,"error":"internal_error"}"#.as_slice()
        );
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("intentional"));
        assert!(!text.contains("serialize"));
    }
}
