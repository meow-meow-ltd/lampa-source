//! Profiles route group — owner: T09.
//!
//! Frozen paths (T03 golden fixtures 09–12): `GET /api/profiles/all`,
//! `POST /api/profiles/{create,update,delete}`. Device cookie (`device_token`)
//! is enforced by [`crate::http::middleware::auth_guard`] before any handler
//! here runs; the route module itself never checks credentials.
//!
//! Legacy semantics ported from `backend/routes/profiles.js`:
//!
//! * `GET all` first ensures the `Main` profile exists (only when the table is
//!   empty) and then returns `{secuses:true,profiles:[...]}` ordered by
//!   `is_main DESC, sort_order ASC, id ASC`; the profile view keeps the legacy
//!   names `main`/`child`/`sort` and the `l_1` icon fallback;
//! * `create` trims the name (`400 {"secuses":false,"text":"Name is required"}`
//!   when it is empty), defaults a missing/blank icon to `l_1`, appends after
//!   the current maximum `sort_order` unless `sort` is given, and answers with
//!   the created profile;
//! * `update` patches only the fields the legacy route considers truthy
//!   (`name`/`icon` must be non-empty strings, `child` must be a JSON boolean,
//!   `sort` goes through `parseInt`), answers `{secuses:true}` even when the id
//!   does not exist, and rejects an empty patch with
//!   `400 {"secuses":false,"text":"No fields to update"}`;
//! * `delete` is a **plain** `DELETE FROM profiles WHERE id=?`: schema v7 has no
//!   foreign keys, so `bookmarks`/`profile_settings`/`timeline*` rows become
//!   orphans. That is the legacy contract, not a bug.
//!
//! Bodies are accepted as JSON or `application/x-www-form-urlencoded`
//! (body-parser runs both in the legacy stack); `Content-Type` is ignored, like
//! T10's PIN handler. No raw SQL/driver text is ever serialised: failures map
//! to `{"secuses":false,"error":"database_error"}`.

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::Value;

use crate::db::DbError;
use crate::profiles::{Profile, ProfileUpdate};
use crate::state::AppState;

/// Maximum request body accepted by the legacy profile routes (defensive cap).
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Mounts the four profiles routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/profiles/all", get(all))
        .route("/api/profiles/create", post(create))
        .route("/api/profiles/update", post(update))
        .route("/api/profiles/delete", post(delete))
}

/// Legacy `mapProfile` view (`backend/routes/profiles.js`).
#[derive(Debug, Serialize)]
struct ProfileView {
    id: i64,
    name: String,
    icon: String,
    main: bool,
    child: bool,
    sort: i64,
}

impl From<&Profile> for ProfileView {
    fn from(profile: &Profile) -> Self {
        Self {
            id: profile.id,
            name: profile.name.clone(),
            icon: profile.icon_or_default().to_owned(),
            main: profile.is_main,
            child: profile.is_child,
            sort: profile.sort_order,
        }
    }
}

#[derive(Debug, Serialize)]
struct ProfilesResponse {
    secuses: bool,
    profiles: Vec<ProfileView>,
}

#[derive(Debug, Serialize)]
struct ProfileResponse {
    secuses: bool,
    profile: ProfileView,
}

/// `{secuses:true}` — update/delete success body (legacy spelling kept).
#[derive(Debug, Serialize)]
struct SecusesResponse {
    secuses: bool,
}

/// `{secuses:false,text:"..."}` — legacy 400 body.
#[derive(Debug, Serialize)]
struct LegacyTextError {
    secuses: bool,
    text: &'static str,
}

/// `{secuses:false,error:"..."}` — sanitised 500/503 body (never SQL text).
#[derive(Debug, Serialize)]
struct LegacyError {
    secuses: bool,
    error: &'static str,
}

/// `GET /api/profiles/all`.
async fn all(State(state): State<AppState>) -> Response {
    let Some(database) = state.db().database() else {
        return db_unavailable();
    };
    match database.profiles().list().await {
        Ok(profiles) => json_response(
            StatusCode::OK,
            ProfilesResponse {
                secuses: true,
                profiles: profiles.iter().map(ProfileView::from).collect(),
            },
        ),
        Err(error) => database_error("profiles.list", &error),
    }
}

/// `POST /api/profiles/create`.
async fn create(State(state): State<AppState>, request: Request) -> Response {
    let Some(database) = state.db().database() else {
        return db_unavailable();
    };
    let fields = read_fields(request).await;

    let name = fields
        .field("name")
        .and_then(|value| value.as_string())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty());
    let Some(name) = name else {
        return legacy_text_error(StatusCode::BAD_REQUEST, "Name is required");
    };
    let icon = fields
        .field("icon")
        .and_then(|value| value.as_string())
        .map(|icon| icon.trim().to_owned())
        .filter(|icon| !icon.is_empty());
    let sort = fields.field("sort").and_then(|value| value.as_js_int());

    match database
        .profiles()
        .create(&name, icon.as_deref(), sort)
        .await
    {
        Ok(profile) => json_response(
            StatusCode::OK,
            ProfileResponse {
                secuses: true,
                profile: ProfileView::from(&profile),
            },
        ),
        Err(error) => database_error("profiles.create", &error),
    }
}

/// `POST /api/profiles/update`.
async fn update(State(state): State<AppState>, request: Request) -> Response {
    let Some(database) = state.db().database() else {
        return db_unavailable();
    };
    let fields = read_fields(request).await;

    let Some(id) = fields
        .field("id")
        .and_then(|value| value.as_js_int())
        .filter(|id| *id != 0)
    else {
        return legacy_text_error(StatusCode::BAD_REQUEST, "Invalid id");
    };

    let name = fields
        .field("name")
        .and_then(|value| value.as_string())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty());
    let icon = fields
        .field("icon")
        .and_then(|value| value.as_string())
        .map(|icon| icon.trim().to_owned())
        .filter(|icon| !icon.is_empty());
    let is_child = fields.field("child").and_then(|value| value.as_bool());
    let sort_order = fields.field("sort").and_then(|value| value.as_js_int());

    if name.is_none() && icon.is_none() && is_child.is_none() && sort_order.is_none() {
        return legacy_text_error(StatusCode::BAD_REQUEST, "No fields to update");
    }
    let patch = ProfileUpdate {
        name,
        icon,
        is_child,
        sort_order,
    };

    match database.profiles().update(id, patch).await {
        Ok(_) => json_response(StatusCode::OK, SecusesResponse { secuses: true }),
        Err(error) => database_error("profiles.update", &error),
    }
}

/// `POST /api/profiles/delete` — plain delete, no cascade.
async fn delete(State(state): State<AppState>, request: Request) -> Response {
    let Some(database) = state.db().database() else {
        return db_unavailable();
    };
    let fields = read_fields(request).await;

    let Some(id) = fields
        .field("id")
        .and_then(|value| value.as_js_int())
        .filter(|id| *id != 0)
    else {
        return legacy_text_error(StatusCode::BAD_REQUEST, "Invalid id");
    };

    match database.profiles().delete(id).await {
        Ok(_) => json_response(StatusCode::OK, SecusesResponse { secuses: true }),
        Err(error) => database_error("profiles.delete", &error),
    }
}

// ---------------------------------------------------------------------------
// Shared request-body helpers (also used by the sibling T09 handlers)
// ---------------------------------------------------------------------------

/// Parsed legacy request body: a JSON object or urlencoded form fields.
pub(crate) enum Fields {
    /// `application/json` object (typed values preserved).
    Json(serde_json::Map<String, Value>),
    /// `application/x-www-form-urlencoded` fields (all values are strings).
    Form(Vec<(String, String)>),
}

impl Fields {
    /// Last value of `key` (legacy `qs` keeps the last duplicate for objects).
    pub(crate) fn field(&self, key: &str) -> Option<FieldValue<'_>> {
        match self {
            Self::Json(object) => object.get(key).map(FieldValue::Json),
            Self::Form(pairs) => pairs
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| FieldValue::Text(value)),
        }
    }
}

/// One typed field value.
pub(crate) enum FieldValue<'a> {
    /// JSON value (keeps the number/bool/string types).
    Json(&'a Value),
    /// urlencoded text (legacy body-parser turns every form field into a string).
    Text(&'a str),
}

impl FieldValue<'_> {
    /// Whether this is the JSON literal `null` (legacy `value === null`).
    pub(crate) fn is_json_null(&self) -> bool {
        matches!(self, Self::Json(Value::Null))
    }

    /// JSON string or form text; non-string JSON values are `None`
    /// (legacy `typeof x === 'string'`).
    pub(crate) fn as_string(&self) -> Option<String> {
        match self {
            Self::Json(Value::String(text)) => Some(text.clone()),
            Self::Json(_) => None,
            Self::Text(text) => Some((*text).to_owned()),
        }
    }

    /// JSON boolean only (legacy `typeof x === 'boolean'`).
    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Json(Value::Bool(value)) => Some(*value),
            Self::Json(_) | Self::Text(_) => None,
        }
    }

    /// Legacy `parseInt` result (`None` where JS produces `NaN`).
    pub(crate) fn as_js_int(&self) -> Option<i64> {
        match self {
            Self::Json(Value::Number(number)) => number
                .as_i64()
                .or_else(|| number.as_f64().map(|value| value.trunc() as i64)),
            Self::Json(Value::String(text)) => js_parse_int(text),
            Self::Json(_) => None,
            Self::Text(text) => js_parse_int(text),
        }
    }

    /// Plain JSON value (form text becomes a JSON string).
    pub(crate) fn as_json_owned(&self) -> Value {
        match self {
            Self::Json(value) => (*value).clone(),
            Self::Text(text) => Value::String((*text).to_owned()),
        }
    }
}

/// Reads and parses the request body (`Content-Type` is ignored, like the
/// legacy body-parser stack: JSON object first, urlencoded form otherwise).
pub(crate) async fn read_fields(request: Request) -> Fields {
    fields_from_body(request.into_body()).await
}

/// Reads and parses an already-split request body.
pub(crate) async fn fields_from_body(body: Body) -> Fields {
    match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => parse_fields(&bytes),
        // Unreadable/oversized bodies degrade to "no fields": every legacy
        // route then answers its own 400 (`Name is required`, `Invalid id`, …).
        Err(_) => Fields::Form(Vec::new()),
    }
}

/// Parses raw body bytes: JSON object, otherwise urlencoded fields.
pub(crate) fn parse_fields(body: &[u8]) -> Fields {
    let Ok(text) = std::str::from_utf8(body) else {
        return Fields::Form(Vec::new());
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Fields::Form(Vec::new());
    }
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(trimmed) {
            return Fields::Json(object);
        }
    }
    Fields::Form(parse_form(trimmed))
}

/// Minimal `application/x-www-form-urlencoded` parser (`a=1&b=two+words`).
fn parse_form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key.trim()), percent_decode(value))
        })
        .collect()
}

/// Legacy `parseInt` for strings: optional whitespace/sign, leading digits;
/// `None` when no digit is present (`NaN`).
pub(crate) fn js_parse_int(text: &str) -> Option<i64> {
    let trimmed = text.trim_start();
    let (negative, rest) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let value: i64 = digits.parse().ok()?;
    Some(if negative { -value } else { value })
}

/// Minimal `application/x-www-form-urlencoded` value decoder.
pub(crate) fn percent_decode(value: &str) -> String {
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
fn database_error(operation: &'static str, error: &DbError) -> Response {
    tracing::error!(
        operation,
        code = error.code(),
        error = ?error,
        "profiles API database failure"
    );
    json_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        LegacyError {
            secuses: false,
            error: "database_error",
        },
    )
}

/// Fail-closed server (startup guard failed): no writes, no reads.
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

    const FORM: &str = "application/x-www-form-urlencoded";

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

    /// `response.body` of a committed T03 fixture (exact legacy bytes as a string).
    fn fixture_body(fixture: &str) -> String {
        let fixture: Value = serde_json::from_str(fixture).expect("fixture JSON");
        fixture["response"]["body"]
            .as_str()
            .expect("fixture body string")
            .to_owned()
    }

    /// The three synthetic profiles of the T03 capture database.
    async fn seed_golden_profiles(database: &Database) {
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
    }

    async fn count(database: &Database) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM profiles")
            .fetch_one(database.pool())
            .await
            .expect("count profiles")
    }

    #[tokio::test]
    async fn empty_profiles_table_creates_exactly_one_main() {
        let (_temp, database, state) = ready().await;
        assert_eq!(count(&database).await, 0);

        let (status, body) = send(state.clone(), "GET", "/api/profiles/all", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"secuses":true,"profiles":[{"id":1,"name":"Main","icon":"l_1","main":true,"child":false,"sort":1}]}"#
        );

        let rows: Vec<(i64, String, String, i64, i64)> =
            sqlx::query_as("SELECT id, name, icon, is_main, sort_order FROM profiles ORDER BY id")
                .fetch_all(database.pool())
                .await
                .expect("read Main");
        assert_eq!(rows.len(), 1, "exactly one Main row is created");
        assert_eq!(
            rows[0],
            (1, "Main".to_owned(), "l_1".to_owned(), 1, 1),
            "Main(id=1, icon=l_1, is_main=1, sort=1)"
        );

        // A second read must not duplicate the fallback profile.
        let (status, again) = send(state, "GET", "/api/profiles/all", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(again, body);
        assert_eq!(count(&database).await, 1);
        database.close().await;
    }

    #[tokio::test]
    async fn fixture_09_profiles_all_is_byte_identical() {
        let (_temp, database, state) = ready().await;
        seed_golden_profiles(&database).await;

        let (status, body) = send(state, "GET", "/api/profiles/all", &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            fixture_body(include_str!(
                "../../../tests/fixtures/api/09_get_api_profiles_all.json"
            ))
        );
        database.close().await;
    }

    #[tokio::test]
    async fn fixture_10_create_is_byte_identical_and_appends() {
        let (_temp, database, state) = ready().await;
        seed_golden_profiles(&database).await;

        let (status, body) = send(
            state,
            "POST",
            "/api/profiles/create",
            &[("content-type", "application/json")],
            Some(r#"{"name":"Golden Created","icon":"l_9"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            fixture_body(include_str!(
                "../../../tests/fixtures/api/10_post_api_profiles_create.json"
            ))
        );

        let row: (String, String, i64, i64, i64) = sqlx::query_as(
            "SELECT name, icon, is_main, is_child, sort_order FROM profiles WHERE id = 4",
        )
        .fetch_one(database.pool())
        .await
        .expect("created profile");
        assert_eq!(
            row,
            ("Golden Created".to_owned(), "l_9".to_owned(), 0, 0, 4)
        );
        database.close().await;
    }

    #[tokio::test]
    async fn fixture_11_update_is_byte_identical_and_maps_child_to_is_child() {
        let (_temp, database, state) = ready().await;
        seed_golden_profiles(&database).await;

        let (status, body) = send(
            state,
            "POST",
            "/api/profiles/update",
            &[("content-type", "application/json")],
            Some(r#"{"id":3,"icon":"l_7","child":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            fixture_body(include_str!(
                "../../../tests/fixtures/api/11_post_api_profiles_update.json"
            ))
        );

        let row: (String, i64, String, i64) =
            sqlx::query_as("SELECT name, is_child, icon, updated_at FROM profiles WHERE id = 3")
                .fetch_one(database.pool())
                .await
                .expect("updated profile");
        assert_eq!(row.0, "Guest", "name is untouched");
        assert_eq!(row.1, 1, "legacy boolean `child` maps to is_child");
        assert_eq!(row.2, "l_7");
        assert!(row.3 > 1000, "update refreshes updated_at");
        database.close().await;
    }

    #[tokio::test]
    async fn fixture_12_delete_is_byte_identical_and_restores_three() {
        let (_temp, database, state) = ready().await;
        seed_golden_profiles(&database).await;
        let created = database
            .profiles()
            .create("Golden Created", Some("l_9"), None)
            .await
            .expect("create profile 4");
        assert_eq!(created.id, 4);

        let (status, body) = send(
            state,
            "POST",
            "/api/profiles/delete",
            &[("content-type", "application/json")],
            Some(r#"{"id":4}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            fixture_body(include_str!(
                "../../../tests/fixtures/api/12_post_api_profiles_delete.json"
            ))
        );
        assert_eq!(count(&database).await, 3);
        database.close().await;
    }

    #[tokio::test]
    async fn delete_is_plain_without_cascade_and_orphans_survive() {
        let (_temp, database, state) = ready().await;
        seed_golden_profiles(&database).await;
        let created = database
            .profiles()
            .create("Orphaned", None, None)
            .await
            .expect("create profile");
        let orphan_id = created.id;

        for sql in [
            "INSERT INTO bookmarks(profile_id, type, card_id, data, updated_at) \
             VALUES(?, 'book', 42, '{}', 1)",
            "INSERT INTO profile_settings(profile_id, key, value, updated_at) \
             VALUES(?, 'menu', '1', 1)",
            "INSERT INTO timeline_events(profile_id, hash, data, updated_at) \
             VALUES(?, 'hash', '{}', 1)",
        ] {
            sqlx::query(sql)
                .bind(orphan_id)
                .execute(database.pool())
                .await
                .expect("seed child row");
        }

        let (status, body) = send(
            state,
            "POST",
            "/api/profiles/delete",
            &[("content-type", "application/json")],
            Some(&format!(r#"{{"id":{orphan_id}}}"#)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true}"#);

        let still_there: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE id = ?")
            .bind(orphan_id)
            .fetch_one(database.pool())
            .await
            .expect("profile count");
        assert_eq!(still_there, 0, "profile row is deleted");

        for (table, sql) in [
            (
                "bookmarks",
                "SELECT count(*) FROM bookmarks WHERE profile_id = ?",
            ),
            (
                "profile_settings",
                "SELECT count(*) FROM profile_settings WHERE profile_id = ?",
            ),
            (
                "timeline_events",
                "SELECT count(*) FROM timeline_events WHERE profile_id = ?",
            ),
        ] {
            let rows: i64 = sqlx::query_scalar(sql)
                .bind(orphan_id)
                .fetch_one(database.pool())
                .await
                .expect("orphan count");
            assert_eq!(rows, 1, "{table} rows must survive the profile delete");
        }
        let fk_errors: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_foreign_key_check")
            .fetch_one(database.pool())
            .await
            .expect("foreign_key_check");
        assert_eq!(fk_errors, 0, "schema v7 declares no foreign keys");
        database.close().await;
    }

    #[tokio::test]
    async fn create_requires_a_non_empty_name() {
        let (_temp, database, state) = ready().await;
        let (status, body) = send(
            state,
            "POST",
            "/api/profiles/create",
            &[("content-type", "application/json")],
            Some(r#"{"name":"   ","icon":"l_2"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, r#"{"secuses":false,"text":"Name is required"}"#);
        assert_eq!(count(&database).await, 0);
        database.close().await;
    }

    #[tokio::test]
    async fn update_and_delete_reject_an_invalid_id() {
        let (_temp, database, state) = ready().await;
        seed_golden_profiles(&database).await;

        for route in ["/api/profiles/update", "/api/profiles/delete"] {
            for body in [r#"{}"#, r#"{"id":0}"#, r#"{"id":"abc"}"#] {
                let (status, response) = send(
                    state.clone(),
                    "POST",
                    route,
                    &[("content-type", "application/json")],
                    Some(body),
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::BAD_REQUEST,
                    "{route} {body} must be a legacy 400"
                );
                assert_eq!(response, r#"{"secuses":false,"text":"Invalid id"}"#);
            }
        }
        database.close().await;
    }

    #[tokio::test]
    async fn update_without_legacy_fields_is_a_400() {
        let (_temp, database, state) = ready().await;
        seed_golden_profiles(&database).await;

        for body in [
            r#"{"id":1}"#,
            r#"{"id":1,"name":"   "}"#,
            r#"{"id":1,"icon":""}"#,
            r#"{"id":1,"child":"true"}"#,
            r#"{"id":1,"sort":"not-a-number"}"#,
        ] {
            let (status, response) = send(
                state.clone(),
                "POST",
                "/api/profiles/update",
                &[("content-type", "application/json")],
                Some(body),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(
                response,
                r#"{"secuses":false,"text":"No fields to update"}"#
            );
        }

        // Updating a missing id is still a legacy success (no row affected).
        let (status, response) = send(
            state,
            "POST",
            "/api/profiles/update",
            &[("content-type", "application/json")],
            Some(r#"{"id":999,"name":"Ghost"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response, r#"{"secuses":true}"#);
        database.close().await;
    }

    #[tokio::test]
    async fn create_accepts_a_legacy_sort_override() {
        let (_temp, database, state) = ready().await;
        database.profiles().list().await.expect("Main");

        let (status, body) = send(
            state,
            "POST",
            "/api/profiles/create",
            &[("content-type", "application/json")],
            Some(r#"{"name":"Sorted","sort":"10"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"secuses":true,"profile":{"id":2,"name":"Sorted","icon":"l_1","main":false,"child":false,"sort":10}}"#
        );
        database.close().await;
    }

    #[tokio::test]
    async fn urlencoded_bodies_are_accepted_like_the_legacy_stack() {
        let (_temp, database, state) = ready().await;
        database.profiles().list().await.expect("Main");

        let (status, body) = send(
            state.clone(),
            "POST",
            "/api/profiles/create",
            &[("content-type", FORM)],
            Some("name=Formed+Profile&icon=l_4"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            r#"{"secuses":true,"profile":{"id":2,"name":"Formed Profile","icon":"l_4","main":false,"child":false,"sort":2}}"#
        );

        let (status, body) = send(
            state,
            "POST",
            "/api/profiles/update",
            &[("content-type", FORM)],
            Some("id=1&name=Renamed&child=true"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"secuses":true}"#);
        let name: String = sqlx::query_scalar("SELECT name FROM profiles WHERE id = 1")
            .fetch_one(database.pool())
            .await
            .expect("updated name");
        assert_eq!(name, "Renamed");
        let is_child: i64 = sqlx::query_scalar("SELECT is_child FROM profiles WHERE id = 1")
            .fetch_one(database.pool())
            .await
            .expect("child flag");
        assert_eq!(is_child, 0, "form `child=true` is a string, not a boolean");
        database.close().await;
    }

    #[tokio::test]
    async fn fail_closed_state_answers_503_without_sql_text() {
        let (_temp, state) = unavailable().await;
        for (method, uri) in [
            ("GET", "/api/profiles/all"),
            ("POST", "/api/profiles/create"),
            ("POST", "/api/profiles/update"),
            ("POST", "/api/profiles/delete"),
        ] {
            let (status, body) = send(state.clone(), method, uri, &[], Some("{}")).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(body, r#"{"secuses":false,"error":"db_unavailable"}"#);
            for forbidden in ["SELECT", "INSERT", "DELETE FROM", "sqlite", "PRAGMA"] {
                assert!(!body.contains(forbidden), "no SQL text in {body}");
            }
        }
    }

    #[test]
    fn js_parse_int_matches_the_legacy_parse_int() {
        assert_eq!(js_parse_int("42"), Some(42));
        assert_eq!(js_parse_int("  42"), Some(42));
        assert_eq!(js_parse_int("42abc"), Some(42));
        assert_eq!(js_parse_int("-7"), Some(-7));
        assert_eq!(js_parse_int("+7"), Some(7));
        assert_eq!(js_parse_int("3.9"), Some(3));
        assert_eq!(js_parse_int("abc"), None);
        assert_eq!(js_parse_int(""), None);
        assert_eq!(js_parse_int("   "), None);
        assert_eq!(js_parse_int("0x10"), Some(0), "parseInt prefix only");
    }

    #[test]
    fn field_values_keep_legacy_js_types() {
        let fields = parse_fields(br#"{"name":"x","child":true,"sort":"10","value":null}"#);
        assert_eq!(
            fields.field("name").and_then(|value| value.as_string()),
            Some("x".to_owned())
        );
        assert_eq!(
            fields.field("child").and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            fields.field("sort").and_then(|value| value.as_js_int()),
            Some(10)
        );
        assert!(fields
            .field("value")
            .map(|value| value.is_json_null())
            .unwrap_or(false));

        let form = parse_fields(b"name=Formed+Profile&icon=l_4&empty=");
        assert_eq!(
            form.field("name").and_then(|value| value.as_string()),
            Some("Formed Profile".to_owned())
        );
        assert_eq!(
            form.field("child").and_then(|value| value.as_bool()),
            None,
            "form fields are strings, never booleans"
        );
        assert_eq!(
            form.field("empty").and_then(|value| value.as_string()),
            Some(String::new())
        );
        assert!(form.field("missing").is_none());
    }
}
