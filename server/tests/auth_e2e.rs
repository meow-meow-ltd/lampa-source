//! T10 auth end-to-end tests against the compiled `lampa-server` binary.
//!
//! Each test uses a throw-away synthetic v7 database under `/tmp` (never
//! `/opt/lampa/data`), a random loopback port, the public S8 lab TOTP secret
//! and real HTTP/cookie traffic. Covered: PIN success + fresh token + C
//! cookies, rotation/revocation, device block, IP rate limit, session bootstrap,
//! 30 s `last_seen_at` throttle, B-strict mode and log hygiene.

#[path = "auth/support.rs"]
mod support;

use std::time::{SystemTime, UNIX_EPOCH};

use support::{totp_now, wrong_totp_now, HttpResponse, Server, TestDatabase};

const SECRET: &str = "JBSWY3DPEHPK3PXP";
const ADMIN_TOKEN: &str = "e2e-admin-token";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn pin_body(device_id: &str, pin: &str) -> String {
    serde_json::json!({ "pin": pin, "device_id": device_id }).to_string()
}

fn assert_authorized(response: &HttpResponse) {
    assert!(
        response.status != 401 && response.status != 404,
        "expected an authorized response, got {}: {}",
        response.status,
        response.body
    );
}

async fn fixture() -> TestDatabase {
    let database = TestDatabase::fixture().await;
    database
        .apply_sql(&format!(
            "INSERT INTO settings(key, value, visible) VALUES('admin_token', '{ADMIN_TOKEN}', 0)"
        ))
        .await;
    database
}

fn login_json(response: &HttpResponse) -> serde_json::Value {
    assert_eq!(response.status, 200, "PIN body: {}", response.body);
    response.json()
}

#[tokio::test]
async fn auth_e2e_pin_guard_session_and_log_hygiene() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    let pin = totp_now(SECRET);
    let response = server.post_json(
        "/api/pin/verify",
        &pin_body("e2e-device-1", &pin),
        &[("user-agent", "lampa-e2e/1.0")],
    );
    let json = login_json(&response);
    assert_eq!(json["ok"], true);
    let token = json["device_token"]
        .as_str()
        .expect("device token")
        .to_owned();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(token, token.to_ascii_lowercase());

    // Transitional C cookies: HttpOnly session + readable legacy device cookie.
    let cookies = response.header_values("set-cookie");
    let session_cookie = cookies
        .iter()
        .find(|cookie| cookie.starts_with("lampa_session="))
        .expect("lampa_session cookie");
    let legacy_cookie = cookies
        .iter()
        .find(|cookie| cookie.starts_with("device_token="))
        .expect("legacy device_token cookie");
    assert!(session_cookie.contains("HttpOnly"));
    assert!(
        !legacy_cookie.contains("HttpOnly"),
        "device_token must stay readable"
    );
    assert!(session_cookie.contains(&token));
    assert!(legacy_cookie.contains(&token));

    // Node-compatible lowercase sha256 in device_access.token_hash.
    let stored = database
        .text("SELECT token_hash FROM device_access WHERE device_id = 'e2e-device-1'")
        .await
        .expect("read token_hash")
        .expect("row exists");
    assert_eq!(stored, sha256_hex(&token));
    let status = database
        .text("SELECT status FROM device_access WHERE device_id = 'e2e-device-1'")
        .await
        .expect("read status")
        .expect("row exists");
    assert_eq!(status, "allowed");

    // The device cookie reaches protected routes.
    let device_cookie = format!("device_token={token}");
    let protected = server.request(
        "GET",
        "/api/profiles/all",
        &[("cookie", device_cookie.as_str())],
        None,
    );
    assert_authorized(&protected);

    // Session bootstrap: legacy cookie upgrades to the same-value HttpOnly cookie.
    let session = server.request(
        "GET",
        "/api/auth/session",
        &[("cookie", device_cookie.as_str())],
        None,
    );
    assert_eq!(session.status, 200, "session body: {}", session.body);
    let session_json = session.json();
    assert_eq!(session_json["paired"], true);
    assert_eq!(session_json["device_id"], "e2e-device-1");
    assert_eq!(session_json["admin"], false);
    assert!(session
        .header_values("set-cookie")
        .iter()
        .any(|cookie| cookie.contains(&format!("lampa_session={token}"))
            && cookie.contains("HttpOnly")));

    // Unknown token → exact 401 invalid_token + cookie clearing.
    let bad = server.request(
        "GET",
        "/api/profiles/all",
        &[("cookie", "device_token=not-a-real-token")],
        None,
    );
    assert_eq!(bad.status, 401);
    assert_eq!(bad.json(), serde_json::json!({ "error": "invalid_token" }));
    assert!(bad
        .header_values("set-cookie")
        .iter()
        .any(|cookie| cookie.starts_with("device_token=;")));

    // Admin routes use auth_token against settings.admin_token.
    let admin_cookie = format!("auth_token={ADMIN_TOKEN}");
    let admin = server.request(
        "GET",
        "/api/settings",
        &[("cookie", admin_cookie.as_str())],
        None,
    );
    assert_authorized(&admin);
    let admin_bad = server.request(
        "GET",
        "/api/settings",
        &[("cookie", "auth_token=wrong-token")],
        None,
    );
    assert_eq!(admin_bad.status, 401);
    assert_eq!(
        admin_bad.json(),
        serde_json::json!({ "error": "invalid_token" })
    );
    let admin_session = server.request(
        "GET",
        "/api/auth/session",
        &[("cookie", admin_cookie.as_str())],
        None,
    );
    assert_eq!(admin_session.status, 200);
    assert_eq!(
        admin_session.json(),
        serde_json::json!({ "paired": true, "admin": true })
    );

    // Log hygiene: no token, TOTP code, cookie value or secret in logs.
    let logs = server.stop();
    for forbidden in [
        token.as_str(),
        pin.as_str(),
        SECRET,
        "device_token=",
        "lampa_session=",
    ] {
        assert!(
            !logs.contains(forbidden),
            "logs must not contain {forbidden:?}:\n{logs}"
        );
    }
}

#[tokio::test]
async fn auth_e2e_rotation_invalidates_the_previous_token() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    let first = login_json(&server.post_json(
        "/api/pin/verify",
        &pin_body("e2e-rotate", &totp_now(SECRET)),
        &[],
    ));
    let first_token = first["device_token"].as_str().expect("token").to_owned();

    let second = login_json(&server.post_json(
        "/api/pin/verify",
        &pin_body("e2e-rotate", &totp_now(SECRET)),
        &[],
    ));
    let second_token = second["device_token"].as_str().expect("token").to_owned();
    assert_ne!(first_token, second_token);

    let stale = format!("device_token={first_token}");
    let response = server.request(
        "GET",
        "/api/profiles/all",
        &[("cookie", stale.as_str())],
        None,
    );
    assert_eq!(response.status, 401, "the previous token must stop working");
    assert_eq!(
        response.json(),
        serde_json::json!({ "error": "invalid_token" })
    );

    let fresh = format!("device_token={second_token}");
    assert_authorized(&server.request(
        "GET",
        "/api/profiles/all",
        &[("cookie", fresh.as_str())],
        None,
    ));
    let stored = database
        .text("SELECT token_hash FROM device_access WHERE device_id = 'e2e-rotate'")
        .await
        .expect("read hash")
        .expect("row");
    assert_eq!(stored, sha256_hex(&second_token));
}

#[tokio::test]
async fn auth_e2e_device_blocks_after_five_failed_pins() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);
    let wrong = wrong_totp_now(SECRET);

    for attempt in 1..=5 {
        let response = server.post_json("/api/pin/verify", &pin_body("e2e-blocked", &wrong), &[]);
        assert_eq!(response.status, 200, "PIN failures stay HTTP 200");
        let json = response.json();
        assert_eq!(json["ok"], false);
        assert_eq!(json["attempts_left"], 5 - attempt);
        assert_eq!(json["blocked"], attempt == 5);
    }

    // Blocked devices short-circuit before TOTP and never get a 403.
    let blocked = server.post_json(
        "/api/pin/verify",
        &pin_body("e2e-blocked", &totp_now(SECRET)),
        &[],
    );
    assert_eq!(blocked.status, 200);
    assert_eq!(
        blocked.json(),
        serde_json::json!({ "ok": false, "blocked": true })
    );
    let status = database
        .text("SELECT status FROM device_access WHERE device_id = 'e2e-blocked'")
        .await
        .expect("read status")
        .expect("row");
    assert_eq!(status, "blocked");
}

#[tokio::test]
async fn auth_e2e_ip_rate_limit_blocks_before_totp() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);
    let wrong = wrong_totp_now(SECRET);
    let client_ip = "203.0.113.44";

    for attempt in 1..=10 {
        let response = server.post_json(
            "/api/pin/verify",
            &pin_body(&format!("e2e-ip-{attempt}"), &wrong),
            &[("cf-connecting-ip", client_ip)],
        );
        assert_eq!(response.status, 200);
        let json = response.json();
        assert_eq!(json["ip_blocked"], attempt == 10, "attempt {attempt}");
    }

    // IP check runs before TOTP: even a correct PIN for a new device is 429.
    let limited = server.post_json(
        "/api/pin/verify",
        &pin_body("e2e-ip-new", &totp_now(SECRET)),
        &[("cf-connecting-ip", client_ip)],
    );
    assert_eq!(limited.status, 429);
    assert_eq!(limited.header("retry-after"), Some("900"));
    assert_eq!(
        limited.json(),
        serde_json::json!({
            "ok": false,
            "error": "too_many_attempts",
            "retry_after": 900
        })
    );
    let rows = database
        .scalar("SELECT count(*) FROM device_access WHERE device_id = 'e2e-ip-new'")
        .await
        .expect("count");
    assert_eq!(rows, 0, "the blocked IP must not touch device rows");
}

#[tokio::test]
async fn auth_e2e_last_seen_is_throttled_to_thirty_seconds() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);
    let token = login_json(&server.post_json(
        "/api/pin/verify",
        &pin_body("e2e-seen", &totp_now(SECRET)),
        &[],
    ))["device_token"]
        .as_str()
        .expect("token")
        .to_owned();
    let cookie = format!("device_token={token}");
    let headers = [("cookie", cookie.as_str())];

    let initial = database
        .scalar("SELECT last_seen_at FROM device_access WHERE device_id = 'e2e-seen'")
        .await
        .expect("last_seen");
    assert!(initial > 0);

    // Two requests within the same 30 s window: no write, no drift.
    assert_authorized(&server.request("GET", "/api/profiles/all", &headers, None));
    assert_authorized(&server.request("GET", "/api/profiles/all", &headers, None));
    let unchanged = database
        .scalar("SELECT last_seen_at FROM device_access WHERE device_id = 'e2e-seen'")
        .await
        .expect("last_seen");
    assert_eq!(unchanged, initial, "last_seen_at must be throttled");

    // Age the row past the throttle; the next request writes once.
    let stale = now_ms() - 31_000;
    database
        .apply_sql(&format!(
            "UPDATE device_access SET last_seen_at = {stale} WHERE device_id = 'e2e-seen'"
        ))
        .await;
    assert_authorized(&server.request("GET", "/api/profiles/all", &headers, None));
    let refreshed = database
        .scalar("SELECT last_seen_at FROM device_access WHERE device_id = 'e2e-seen'")
        .await
        .expect("last_seen");
    assert!(
        refreshed >= stale + 30_000,
        "last_seen must advance after the 30 s window: {refreshed} vs {stale}"
    );
}

#[tokio::test]
async fn auth_e2e_revoked_device_is_401_everywhere() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);
    let token = login_json(&server.post_json(
        "/api/pin/verify",
        &pin_body("e2e-revoked", &totp_now(SECRET)),
        &[],
    ))["device_token"]
        .as_str()
        .expect("token")
        .to_owned();
    let cookie = format!("device_token={token}");

    // Legacy admin `revoke`: status='pending', token_hash=NULL.
    database
        .apply_sql(
            "UPDATE device_access SET status = 'pending', token_hash = NULL \
             WHERE device_id = 'e2e-revoked'",
        )
        .await;

    let api = server.request(
        "GET",
        "/api/profiles/all",
        &[("cookie", cookie.as_str())],
        None,
    );
    assert_eq!(api.status, 401);
    assert_eq!(api.json(), serde_json::json!({ "error": "invalid_token" }));
    assert!(api
        .header_values("set-cookie")
        .iter()
        .any(|value| value.starts_with("lampa_session=;")));

    let session = server.request(
        "GET",
        "/api/auth/session",
        &[("cookie", cookie.as_str())],
        None,
    );
    assert_eq!(session.status, 401);
    assert_eq!(
        session.json(),
        serde_json::json!({ "paired": false, "error": "invalid_token" })
    );

    let logs = server.stop();
    assert!(!logs.contains(&token), "revoked token must not be logged");
}

#[tokio::test]
async fn auth_e2e_b_strict_rejects_legacy_cookie_and_https_sets_secure() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, false);

    let response = server.post_json(
        "/api/pin/verify",
        &pin_body("e2e-strict", &totp_now(SECRET)),
        &[("x-forwarded-proto", "https")],
    );
    let json = login_json(&response);
    let token = json["device_token"].as_str().expect("token").to_owned();
    let cookies = response.header_values("set-cookie");
    assert_eq!(cookies.len(), 1, "B-strict sets only lampa_session");
    assert!(cookies[0].starts_with("lampa_session="));
    assert!(cookies[0].contains("HttpOnly"));
    assert!(cookies[0].contains("Secure"), "HTTPS sets Secure");

    let legacy = format!("device_token={token}");
    let rejected = server.request(
        "GET",
        "/api/profiles/all",
        &[("cookie", legacy.as_str())],
        None,
    );
    assert_eq!(rejected.status, 401);

    let session_cookie = format!("lampa_session={token}");
    assert_authorized(&server.request(
        "GET",
        "/api/profiles/all",
        &[("cookie", session_cookie.as_str())],
        None,
    ));
    let session = server.request(
        "GET",
        "/api/auth/session",
        &[("cookie", session_cookie.as_str())],
        None,
    );
    assert_eq!(session.status, 200);
    assert_eq!(session.json()["device_id"], "e2e-strict");
}
