//! Admin login/logout route group — owner: T07.
//!
//! Frozen paths (T03 golden fixtures 26–27): `POST /admin/login`,
//! `POST /admin/logout`. Both are public (no cookie); the fixture capture
//! calls them without credentials.
//!
//! Legacy semantics (`backend/routes/auth.js`, plan §3.5):
//! * `POST /admin/login {password}` compares the **plaintext** password with
//!   `settings.admin_password`; a missing `admin_password` row falls back to
//!   the legacy literal `'admin'`;
//! * success answers `{"success":true}` and sets the legacy `auth_token` cookie
//!   to the stored `settings.admin_token`, `HttpOnly`, `SameSite=Lax`, `Secure`
//!   only for HTTPS origins (a LAN HTTP browser must keep the cookie);
//! * a wrong/missing password answers `401 {"success":false}`;
//! * `POST /admin/logout` clears the cookie (`Max-Age=0`) and answers
//!   `{"success":true}` without touching the database.
//!
//! The password and the token are never logged and the error bodies never
//! contain database diagnostics.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde::Serialize;
use serde_json::Value;

use crate::auth::constant_time;
use crate::auth::cookie;
use crate::state::AppState;

/// Legacy password fallback when the `admin_password` row is absent.
const LEGACY_FALLBACK_PASSWORD: &str = "admin";

/// Mounts `POST /admin/login` and `POST /admin/logout`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/login", post(login))
        .route("/admin/logout", post(logout))
}

/// `{"success":true}` / `{"success":false}` — the legacy shapes.
#[derive(Serialize)]
struct LoginBody {
    success: bool,
}

/// Submitted password, preserving the difference between absent, JSON `null`
/// and a string (Node compares `req.body.password === row.value`, so `null`
/// only matches a stored SQL `NULL`).
enum SubmittedPassword {
    Missing,
    Null,
    Text(String),
}

/// The stored password the submitted value is compared against.
enum StoredPassword {
    Null,
    Text(String),
}

impl StoredPassword {
    fn matches(&self, submitted: &SubmittedPassword) -> bool {
        match (self, submitted) {
            (StoredPassword::Text(stored), SubmittedPassword::Text(submitted)) => {
                constant_time::eq(stored.as_bytes(), submitted.as_bytes())
            }
            (StoredPassword::Null, SubmittedPassword::Null) => true,
            _ => false,
        }
    }
}

/// `POST /admin/login`.
async fn login(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(database) = state.db().database() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable");
    };
    let submitted = match parse_password(&body) {
        Some(password) => password,
        None => return error(StatusCode::BAD_REQUEST, "invalid_request"),
    };

    let stored = match database.settings().get_raw("admin_password").await {
        Ok(Some(setting)) => match setting.value {
            Some(value) => StoredPassword::Text(value),
            None => StoredPassword::Null,
        },
        // Node: `const password = row ? row.value : 'admin'` — only a missing
        // row uses the fallback; the seeded database always has the row.
        Ok(None) => StoredPassword::Text(LEGACY_FALLBACK_PASSWORD.to_owned()),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    };

    if !stored.matches(&submitted) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(LoginBody { success: false }),
        )
            .into_response();
    }

    // Node stringifies `tokenRow?.value`: a SQL NULL token becomes "null", a
    // missing row becomes "undefined". Keep the same cookie value payload.
    let token = match database.settings().get_raw("admin_token").await {
        Ok(Some(setting)) => setting.value.unwrap_or_else(|| "null".to_owned()),
        Ok(None) => "undefined".to_owned(),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    };

    let mut cookie_header = format!("auth_token={token}; Path=/; HttpOnly; SameSite=Lax");
    if cookie::is_https(&headers) {
        cookie_header.push_str("; Secure");
    }
    // A stored token with control characters must fail closed rather than emit
    // a partially-built header (the token is admin-set but still untrusted).
    let Ok(cookie_value) = HeaderValue::from_str(&cookie_header) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    };
    let mut response = axum::Json(LoginBody { success: true }).into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, cookie_value);
    response
}

/// `POST /admin/logout` — clears `auth_token`, no database access.
async fn logout() -> Response {
    with_cookie(
        axum::Json(LoginBody { success: true }).into_response(),
        cookie::clear_admin_cookie(),
    )
}

/// Parses `{password}` from a JSON body, ignoring `Content-Type` like the PIN
/// handler (the legacy client sends JSON; form input is not part of fixture 26).
fn parse_password(body: &[u8]) -> Option<SubmittedPassword> {
    let text = std::str::from_utf8(body).ok()?.trim();
    if text.is_empty() {
        return Some(SubmittedPassword::Missing);
    }
    let document: Value = match serde_json::from_str(text) {
        Ok(document) => document,
        Err(_) => return None,
    };
    let object = document.as_object()?;
    Some(match object.get("password") {
        None => SubmittedPassword::Missing,
        Some(Value::Null) => SubmittedPassword::Null,
        Some(Value::String(password)) => SubmittedPassword::Text(password.clone()),
        // Numbers/bools/containers never equal a stored string in Node either.
        Some(_) => SubmittedPassword::Missing,
    })
}

/// `{"error":"..."}` with a stable, non-SQL code.
fn error(status: StatusCode, code: &'static str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": code }))).into_response()
}

fn with_cookie(mut response: Response, cookie: String) -> Response {
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_password_distinguishes_missing_null_and_text() {
        assert!(matches!(
            parse_password(br#"{"password":"golden-admin-password"}"#),
            Some(SubmittedPassword::Text(password)) if password == "golden-admin-password"
        ));
        assert!(matches!(
            parse_password(br#"{"password":null}"#),
            Some(SubmittedPassword::Null)
        ));
        assert!(matches!(
            parse_password(br#"{}"#),
            Some(SubmittedPassword::Missing)
        ));
        assert!(matches!(
            parse_password(b""),
            Some(SubmittedPassword::Missing)
        ));
        assert!(matches!(
            parse_password(br#"{"password":123}"#),
            Some(SubmittedPassword::Missing)
        ));
        assert!(parse_password(br#"{"password":"#).is_none());
        assert!(parse_password(b"[]").is_none());
    }

    #[test]
    fn stored_password_matching_is_plaintext_and_null_aware() {
        let stored = StoredPassword::Text("admin".to_owned());
        assert!(stored.matches(&SubmittedPassword::Text("admin".to_owned())));
        assert!(!stored.matches(&SubmittedPassword::Text("Admin".to_owned())));
        assert!(!stored.matches(&SubmittedPassword::Null));
        assert!(!stored.matches(&SubmittedPassword::Missing));
        assert!(StoredPassword::Null.matches(&SubmittedPassword::Null));
        assert!(!StoredPassword::Null.matches(&SubmittedPassword::Text(String::new())));
    }
}
