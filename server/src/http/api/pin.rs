//! PIN verification route group — owner: T10.
//!
//! Frozen path (T03 golden fixture 01): `POST /api/pin/verify`. Public: the
//! guard forwards it without cookies because this endpoint issues the
//! `device_token`.
//!
//! Legacy semantics (`backend/routes/pin.js`):
//! * `429 {ok:false,error:"too_many_attempts",retry_after}` when the IP is
//!   blocked (checked before anything else);
//! * `400 {"error":"device_id required"}` without a `device_id`;
//! * `500 {"error":"Server configuration error"}` when `TOTP_SECRET` is unset;
//! * a blocked device → `200 {ok:false,blocked:true}` (no 403, no TOTP check);
//! * success → a **fresh** 32-byte hex token, `device_access.token_hash`
//!   overwritten (old token invalid), `200 {ok:true,device_token}` and the
//!   transitional C cookies (`lampa_session` HttpOnly + readable `device_token`);
//! * failure → `200 {ok:false,attempts_left,blocked,ip_blocked[,retry_after]}`.
//!
//! The JSON body is parsed regardless of `Content-Type` (our client sends both
//! JSON and `x-www-form-urlencoded`; plan §3.6). No PIN/token value is logged.

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde::Serialize;

use crate::auth::client_ip;
use crate::auth::cookie;
use crate::auth::{PinOutcome, PinRequest, RequestMeta};
use crate::state::AppState;

/// Maximum accepted request body (PIN/device_id are tiny).
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Mounts `POST /api/pin/verify`.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/pin/verify", post(verify))
}

/// `{"ok":true,"device_token":"<64 hex>"}` — field order matches Node.
#[derive(Serialize)]
struct PinSuccess<'a> {
    ok: bool,
    device_token: &'a str,
}

/// `{"ok":false,"attempts_left":N,"blocked":bool,"ip_blocked":bool[,retry_after]}`.
#[derive(Serialize)]
struct PinFailure {
    ok: bool,
    attempts_left: i64,
    blocked: bool,
    ip_blocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after: Option<i64>,
}

/// `{"ok":false,"blocked":true}`.
#[derive(Serialize)]
struct PinBlocked {
    ok: bool,
    blocked: bool,
}

/// `{"ok":false,"error":"too_many_attempts","retry_after":N}`.
#[derive(Serialize)]
struct PinIpBlocked {
    ok: bool,
    error: &'static str,
    retry_after: i64,
}

/// `POST /api/pin/verify` handler.
async fn verify(State(state): State<AppState>, request: Request) -> Response {
    let meta = RequestMeta {
        ip: client_ip::from_request(&request),
        user_agent: user_agent(request.headers()),
    };

    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return error_response(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
    };
    let pin_request = match parse_body(&body) {
        Ok(pin_request) => pin_request,
        Err(()) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };

    let now_ms = crate::auth::now_millis();
    match state.auth().verify_pin(&pin_request, &meta, now_ms).await {
        PinOutcome::IpBlocked { retry_after_secs } => {
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(PinIpBlocked {
                    ok: false,
                    error: "too_many_attempts",
                    retry_after: retry_after_secs,
                }),
            )
                .into_response();
            if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
        PinOutcome::MissingDeviceId => {
            error_response(StatusCode::BAD_REQUEST, "device_id required")
        }
        PinOutcome::ServerConfig => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Server configuration error",
        ),
        PinOutcome::DeviceBlocked => axum::Json(PinBlocked {
            ok: false,
            blocked: true,
        })
        .into_response(),
        PinOutcome::Success { device_token } => {
            let secure = cookie::is_https(&parts.headers);
            let cookies =
                cookie::device_cookies(&device_token, secure, state.auth().legacy_cookie_enabled());
            with_cookies(
                axum::Json(PinSuccess {
                    ok: true,
                    device_token: &device_token,
                })
                .into_response(),
                cookies,
            )
        }
        PinOutcome::Failure {
            attempts_left,
            blocked,
            ip_blocked,
            retry_after_secs,
        } => axum::Json(PinFailure {
            ok: false,
            attempts_left,
            blocked,
            ip_blocked,
            retry_after: retry_after_secs,
        })
        .into_response(),
        PinOutcome::Unavailable => {
            error_response(StatusCode::SERVICE_UNAVAILABLE, "database_unavailable")
        }
        PinOutcome::DatabaseError => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}

/// Reads the optional legacy `User-Agent` column value.
fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// `{"error":"..."}` response without leaking internals.
fn error_response(status: StatusCode, error: &'static str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": error }))).into_response()
}

/// Appends one `Set-Cookie` header per cookie.
fn with_cookies(mut response: Response, cookies: Vec<String>) -> Response {
    for value in cookies {
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

/// Parses the PIN body: JSON first, then `application/x-www-form-urlencoded`
/// (the legacy jQuery path sends `text/plain`/form bodies).
fn parse_body(body: &[u8]) -> Result<PinRequest, ()> {
    // Content-Type is deliberately irrelevant (plan §3.6).
    let Ok(text) = std::str::from_utf8(body) else {
        return Err(());
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(PinRequest {
            pin: None,
            device_id: None,
        });
    }

    if trimmed.starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|_| ())?;
        let object = value.as_object().ok_or(())?;
        let string_field = |key: &str| {
            object.get(key).and_then(|value| match value {
                serde_json::Value::String(text) => Some(text.clone()),
                _ => None,
            })
        };
        return Ok(PinRequest {
            pin: string_field("pin"),
            device_id: string_field("device_id"),
        });
    }

    let mut pin = None;
    let mut device_id = None;
    for pair in trimmed.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let decoded = percent_decode(value);
        match key.trim() {
            "pin" => pin = Some(decoded),
            "device_id" => device_id = Some(decoded),
            _ => {}
        }
    }
    Ok(PinRequest { pin, device_id })
}

/// Minimal `application/x-www-form-urlencoded` value decoder.
fn percent_decode(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    use crate::auth::device::hash_token;
    use crate::auth::totp::TotpVerifier;
    use crate::auth::{CookieValue, DeviceAuth};
    use crate::config::Config;
    use crate::db::test_support::TempDb;
    use crate::db::Database;
    use crate::state::DbState;

    const SECRET: &str = "JBSWY3DPEHPK3PXP";
    const NOW_MS: i64 = 1_700_000_000_000;

    fn config(secret: Option<&str>, legacy: bool) -> Config {
        let secret = secret.map(str::to_owned);
        let legacy = if legacy { "1" } else { "0" }.to_owned();
        Config::from_lookup(move |key| match key {
            "TOTP_SECRET" => secret.clone(),
            "DEVICE_LEGACY_COOKIE" => Some(legacy.clone()),
            _ => None,
        })
        .expect("test config")
    }

    async fn ready_state(secret: Option<&str>, legacy: bool) -> (TempDb, Database, AppState) {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        let state = AppState::new(config(secret, legacy), DbState::Ready(database.clone()));
        (temp, database, state)
    }

    fn current_pin(state: &AppState, now_ms: i64) -> String {
        state
            .auth()
            .totp_verifier()
            .expect("configured secret")
            .generate_at(u64::try_from(now_ms / 1000).unwrap_or(0))
    }

    fn wrong_pin(valid: &str) -> String {
        if valid == "000000" {
            "111111".to_owned()
        } else {
            "000000".to_owned()
        }
    }

    async fn send(
        state: AppState,
        content_type: &str,
        body: &str,
        user_agent: Option<&str>,
    ) -> Response {
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri("/api/pin/verify")
            .header(header::CONTENT_TYPE, content_type);
        if let Some(user_agent) = user_agent {
            builder = builder.header(header::USER_AGENT, user_agent);
        }
        router()
            .with_state(state)
            .oneshot(builder.body(Body::from(body.to_owned())).expect("request"))
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

    fn json_body(device_id: &str, pin: &str) -> String {
        serde_json::json!({ "pin": pin, "device_id": device_id }).to_string()
    }

    #[tokio::test]
    async fn missing_device_id_is_a_legacy_400() {
        let (_temp, _database, state) = ready_state(Some(SECRET), true).await;
        let response = send(state, "application/json", r#"{"pin":"123456"}"#, None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "device_id required" })
        );
    }

    #[tokio::test]
    async fn malformed_json_is_a_sanitised_400() {
        let (_temp, _database, state) = ready_state(Some(SECRET), true).await;
        let response = send(state, "application/json", "{not-json", None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "invalid_request" })
        );
    }

    #[tokio::test]
    async fn success_issues_fresh_token_hashes_it_and_sets_c_cookies() {
        let (_temp, database, state) = ready_state(Some(SECRET), true).await;
        let pin = current_pin(&state, crate::auth::now_millis());
        let response = send(
            state.clone(),
            "application/json",
            &json_body("device-1", &pin),
            Some("lampa-test/1.0"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let cookies = set_cookies(&response);
        assert_eq!(cookies.len(), 2, "transitional C sets both cookies");
        let session_cookie = cookies
            .iter()
            .find(|cookie| cookie.starts_with("lampa_session="))
            .expect("lampa_session cookie");
        let legacy_cookie = cookies
            .iter()
            .find(|cookie| cookie.starts_with("device_token="))
            .expect("legacy device_token cookie");
        assert!(session_cookie.contains("; HttpOnly"));
        assert!(legacy_cookie.contains("; SameSite=Lax"));
        assert!(
            !legacy_cookie.contains("HttpOnly"),
            "device_token must stay readable during C"
        );

        let body = to_bytes(response.into_body(), 4096).await.expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(
            text.starts_with(r#"{"ok":true,"device_token":""#),
            "Node field order: {text}"
        );
        let json: serde_json::Value = serde_json::from_str(&text).expect("json");
        let token = json["device_token"].as_str().expect("token").to_owned();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(token, token.to_ascii_lowercase());
        assert!(session_cookie.contains(&token));
        assert!(legacy_cookie.contains(&token));

        let stored: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM device_access WHERE device_id = 'device-1'")
                .fetch_one(database.pool())
                .await
                .expect("read hash");
        assert_eq!(stored.as_deref(), Some(hash_token(&token).as_str()));
        assert_eq!(stored.as_deref().map(str::len), Some(64));
        assert_eq!(
            stored.as_deref(),
            Some(hash_token(&token).to_ascii_lowercase().as_str())
        );

        let (status, attempts, ip, last_seen, user_agent): (
            String,
            Option<i64>,
            Option<String>,
            Option<i64>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, attempts, ip, last_seen_at, user_agent FROM device_access \
             WHERE device_id = 'device-1'",
        )
        .fetch_one(database.pool())
        .await
        .expect("read row");
        assert_eq!(status, "allowed");
        assert_eq!(attempts, Some(0));
        assert_eq!(ip.as_deref(), Some("unknown"));
        assert!(last_seen.is_some());
        assert_eq!(user_agent.as_deref(), Some("lampa-test/1.0"));

        // A second verify rotates the token; the previous one stops working.
        let second = send(
            state.clone(),
            "application/json",
            &json_body("device-1", &pin),
            None,
        )
        .await;
        let second_json = body_json(second).await;
        let second_token = second_json["device_token"].as_str().expect("second token");
        assert_ne!(second_token, token);
        assert_eq!(
            state
                .auth()
                .authenticate_device(CookieValue::Value(&token), crate::auth::now_millis())
                .await,
            DeviceAuth::Invalid,
            "the previous token must be invalidated"
        );
        assert!(matches!(
            state
                .auth()
                .authenticate_device(CookieValue::Value(second_token), crate::auth::now_millis())
                .await,
            DeviceAuth::Allowed(_)
        ));
    }

    #[tokio::test]
    async fn b_strict_sets_only_the_session_cookie() {
        let (_temp, _database, state) = ready_state(Some(SECRET), false).await;
        let pin = current_pin(&state, crate::auth::now_millis());
        let response = send(
            state,
            "application/json",
            &json_body("device-b", &pin),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookies = set_cookies(&response);
        assert_eq!(cookies.len(), 1);
        assert!(cookies[0].starts_with("lampa_session="));
        assert!(cookies[0].contains("HttpOnly"));
    }

    #[tokio::test]
    async fn wrong_pin_returns_the_legacy_failure_shape() {
        let (_temp, _database, state) = ready_state(Some(SECRET), true).await;
        let valid = current_pin(&state, NOW_MS);
        let response = send(
            state,
            "application/json",
            &json_body("device-2", &wrong_pin(&valid)),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({
                "ok": false,
                "attempts_left": 4,
                "blocked": false,
                "ip_blocked": false
            })
        );
    }

    #[tokio::test]
    async fn five_wrong_pins_block_the_device_and_never_return_403() {
        let (_temp, database, state) = ready_state(Some(SECRET), true).await;
        let valid = current_pin(&state, NOW_MS);
        let wrong = wrong_pin(&valid);
        for attempt in 1..=5 {
            let response = send(
                state.clone(),
                "application/json",
                &json_body("device-3", &wrong),
                None,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let json = body_json(response).await;
            assert_eq!(json["attempts_left"], 5 - attempt);
            assert_eq!(json["blocked"], attempt == 5);
        }
        let status: String =
            sqlx::query_scalar("SELECT status FROM device_access WHERE device_id = 'device-3'")
                .fetch_one(database.pool())
                .await
                .expect("status");
        assert_eq!(status, "blocked");

        // Even the correct PIN now yields the legacy blocked body (200, no 403
        // and no TOTP work needed).
        let blocked = send(
            state,
            "application/json",
            &json_body("device-3", &valid),
            None,
        )
        .await;
        assert_eq!(blocked.status(), StatusCode::OK);
        assert_eq!(
            body_json(blocked).await,
            serde_json::json!({ "ok": false, "blocked": true })
        );
    }

    #[tokio::test]
    async fn ten_wrong_pins_block_the_ip_with_429_and_retry_after() {
        let (_temp, _database, state) = ready_state(Some(SECRET), true).await;
        let valid = current_pin(&state, NOW_MS);
        let wrong = wrong_pin(&valid);
        for attempt in 1..=10 {
            let response = send(
                state.clone(),
                "application/json",
                &json_body(&format!("device-ip-{attempt}"), &wrong),
                None,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let json = body_json(response).await;
            assert_eq!(json["ip_blocked"], attempt == 10);
        }

        let limited = send(
            state.clone(),
            "application/json",
            &json_body("device-ip-11", &valid),
            None,
        )
        .await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            limited
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("900")
        );
        assert_eq!(
            body_json(limited).await,
            serde_json::json!({
                "ok": false,
                "error": "too_many_attempts",
                "retry_after": 900
            })
        );
    }

    #[tokio::test]
    async fn missing_totp_secret_is_a_500_server_configuration_error() {
        let (_temp, _database, state) = ready_state(None, true).await;
        let response = send(
            state,
            "application/json",
            &json_body("device-4", "123456"),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "error": "Server configuration error" })
        );
    }

    #[tokio::test]
    async fn form_encoded_bodies_are_accepted_regardless_of_content_type() {
        let (_temp, _database, state) = ready_state(Some(SECRET), true).await;
        let pin = current_pin(&state, crate::auth::now_millis());
        let body = format!("pin={pin}&device_id=device-form");
        for content_type in ["application/x-www-form-urlencoded", "text/plain"] {
            let response = send(state.clone(), content_type, &body, None).await;
            assert_eq!(response.status(), StatusCode::OK, "{content_type}");
            assert_eq!(body_json(response).await["ok"], true, "{content_type}");
        }
    }

    #[test]
    fn percent_decoding_handles_hex_and_plus() {
        assert_eq!(percent_decode("123456"), "123456");
        assert_eq!(percent_decode("dev%2D1"), "dev-1");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        assert_eq!(percent_decode("%"), "%");
    }

    #[test]
    fn totp_profile_is_frozen() {
        let verifier = TotpVerifier::from_base32(SECRET).expect("secret");
        assert!(verifier.verify(
            &verifier.generate_at(NOW_MS as u64 / 1000),
            NOW_MS as u64 / 1000
        ));
    }
}
