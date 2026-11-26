//! T07 API parity: admin login/logout (fixtures 26–27) and admin devices
//! (fixtures 02–03) against the real binary.
//!
//! Throw-away synthetic v7 databases under `/tmp`, the real `lampa-server` on a
//! random loopback port, T03 golden request replay. Response bodies are compared
//! byte-for-byte; device timestamps/ips are normalized with the same R3/R4
//! replacements the T03 capture used.
//!
//! Covered: plaintext login with the legacy `'admin'` fallback, the
//! `auth_token` cookie (`HttpOnly`, `SameSite=Lax`, no `Secure` on LAN,
//! `Secure` behind an HTTPS proxy), logout expiry, the device list and the
//! unblock/revoke/delete semantics (revoke = `status='pending', token_hash=NULL`).

// Reuse the T10 e2e harness read-only; not every helper is needed by T07.
#[allow(dead_code)]
#[path = "auth/support.rs"]
mod support;

use support::{Server, TestDatabase};

const ADMIN_TOKEN: &str = "golden-admin-token-9f3c1d7e";
const ADMIN_PASSWORD: &str = "golden-admin-password";
const DEVICE_TOKEN: &str = "lampa-golden-device-token-0001";
const SECRET: &str = "JBSWY3DPEHPK3PXP";

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn seed_sql() -> String {
    let device_hash = sha256_hex(DEVICE_TOKEN);
    format!(
        r#"
INSERT INTO settings(key, value, visible) VALUES
 ('admin_password', '{ADMIN_PASSWORD}', 0),
 ('admin_token', '{ADMIN_TOKEN}', 0),
 ('torrserver_url', 'http://synthetic.invalid:9091', 1),
 ('jackett_url', 'http://synthetic.invalid:9117', 1),
 ('menu_interface', '1', 1),
 ('menu_channels', '1', 1),
 ('menu_player', '1', 1),
 ('menu_parser', '1', 1),
 ('menu_server', '1', 1),
 ('menu_tmdb', '1', 1),
 ('menu_plugins', '1', 1),
 ('menu_parental_control', '1', 1),
 ('menu_data', '1', 1),
 ('menu_more', '1', 1),
 ('head_search', '1', 1),
 ('head_settings', '1', 1),
 ('head_broadcast', '1', 1),
 ('head_notice', '1', 1),
 ('head_profile', '1', 1),
 ('plugins', '[]', 1);

INSERT INTO device_access(id, device_id, ip, user_agent, status, attempts,
                          last_attempt_at, blocked_until, last_seen_at, token_hash)
VALUES
 (1, 'golden-device-0001', '192.0.2.10', 'lampa-golden/1.0', 'allowed', 0,
  1767225600000, NULL, 1767225600000, '{device_hash}'),
 (2, 'golden-device-0002', '192.0.2.11', 'lampa-golden/1.0', 'blocked', 5,
  1767225600000, 1767315600000, NULL, NULL),
 (3, 'golden-device-0003', '192.0.2.12', 'lampa-golden/1.0', 'pending', 2,
  1767225600000, NULL, 1767225600000, NULL);
"#
    )
}

async fn seeded() -> TestDatabase {
    let database = TestDatabase::fixture().await;
    database.apply_sql(&seed_sql()).await;
    database
}

fn fixture(index: u8) -> serde_json::Value {
    let raw = match index {
        2 => include_str!("fixtures/api/02_get_api_admin_devices.json"),
        3 => include_str!("fixtures/api/03_post_api_admin_device_action.json"),
        26 => include_str!("fixtures/api/26_post_admin_login.json"),
        27 => include_str!("fixtures/api/27_post_admin_logout.json"),
        other => panic!("fixture {other} is not used by api_admin"),
    };
    serde_json::from_str(raw).expect("fixture JSON")
}

fn fixture_body(index: u8) -> String {
    fixture(index)["response"]["body"]
        .as_str()
        .expect("fixture body string")
        .to_owned()
}

fn fixture_status(index: u8) -> u16 {
    fixture(index)["response"]["status"]
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .expect("fixture status")
}

/// R3/R4 normalization of the device list body (timestamps/ips -> placeholders)
/// so the response can be compared with the captured fixture bytes.
fn normalize_devices(body: &str) -> String {
    body.replace("\"192.0.2.10\"", "\"<ip>\"")
        .replace("\"192.0.2.11\"", "\"<ip>\"")
        .replace("\"192.0.2.12\"", "\"<ip>\"")
        .replace("1767225600000", "\"<ts>\"")
        .replace("1767315600000", "\"<ts>\"")
}

fn login_json(password: &str) -> String {
    format!(r#"{{"password":"{password}"}}"#)
}

fn cookie(name: &str, value: &str) -> String {
    format!("{name}={value}")
}

fn admin_cookie() -> String {
    cookie("auth_token", ADMIN_TOKEN)
}

fn device_cookie() -> String {
    cookie("device_token", DEVICE_TOKEN)
}

/// Splits a `Set-Cookie` value into `(name, value, lowercased attributes)`.
fn cookie_parts(value: &str) -> (String, String, Vec<String>) {
    let mut parts = value.split(';');
    let pair = parts.next().expect("cookie name=value");
    let (name, value) = pair.split_once('=').expect("cookie pair");
    let attributes = parts
        .map(|attribute| attribute.trim().to_ascii_lowercase())
        .collect();
    (name.trim().to_owned(), value.trim().to_owned(), attributes)
}

fn assert_lan_cookie(cookie: &str, expected_value: &str) {
    let (name, value, attributes) = cookie_parts(cookie);
    assert_eq!(name, "auth_token");
    assert_eq!(value, expected_value);
    assert!(attributes.iter().any(|attribute| attribute == "path=/"));
    assert!(attributes.iter().any(|attribute| attribute == "httponly"));
    assert!(attributes
        .iter()
        .any(|attribute| attribute == "samesite=lax"));
    assert!(
        !attributes
            .iter()
            .any(|attribute| attribute.starts_with("secure")),
        "Secure must not be set on LAN HTTP: {cookie}"
    );
}

#[tokio::test]
async fn api_admin_fixtures_02_03_26_27_parity() {
    let database = seeded().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    // Fixture 26 — POST /admin/login sets the legacy auth_token cookie.
    let login = server.request(
        "POST",
        "/admin/login",
        &[
            ("content-type", "application/json"),
            ("user-agent", "lampa-golden/1.0"),
        ],
        Some(&login_json(ADMIN_PASSWORD)),
    );
    assert_eq!(login.status, fixture_status(26));
    assert_eq!(login.header("content-type"), Some("application/json"));
    assert_eq!(login.body, fixture_body(26), "fixture 26 body");
    let set_cookie = login.header("set-cookie").expect("login Set-Cookie");
    assert_lan_cookie(set_cookie, ADMIN_TOKEN);

    // Fixture 02 — GET /api/admin/devices (list).
    let devices = server.request(
        "GET",
        "/api/admin/devices",
        &[
            ("cookie", admin_cookie().as_str()),
            ("user-agent", "lampa-golden/1.0"),
        ],
        None,
    );
    assert_eq!(devices.status, fixture_status(2));
    assert_eq!(devices.header("content-type"), Some("application/json"));
    assert_eq!(
        normalize_devices(&devices.body),
        fixture_body(2),
        "fixture 02 body"
    );
    assert!(
        !devices.body.contains("token_hash"),
        "the digest column must never be selected"
    );

    // Fixture 03 — POST /api/admin/device/action {id: 2, action: "unblock"}.
    let unblock = server.request(
        "POST",
        "/api/admin/device/action",
        &[
            ("content-type", "application/json"),
            ("cookie", admin_cookie().as_str()),
            ("user-agent", "lampa-golden/1.0"),
        ],
        Some(r#"{"id":2,"action":"unblock"}"#),
    );
    assert_eq!(unblock.status, fixture_status(3));
    assert_eq!(unblock.body, fixture_body(3), "fixture 03 body");
    assert_eq!(
        database
            .text("SELECT status FROM device_access WHERE id = 2")
            .await
            .expect("read unblocked status")
            .as_deref(),
        Some("pending")
    );
    assert_eq!(
        database
            .scalar("SELECT attempts FROM device_access WHERE id = 2")
            .await
            .expect("read unblocked attempts"),
        0
    );
    assert_eq!(
        database
            .scalar("SELECT COALESCE(blocked_until, -1) FROM device_access WHERE id = 2")
            .await
            .expect("read cleared blocked_until"),
        -1
    );

    // Fixture 27 — POST /admin/logout clears the cookie.
    let logout = server.request(
        "POST",
        "/admin/logout",
        &[("user-agent", "lampa-golden/1.0")],
        None,
    );
    assert_eq!(logout.status, fixture_status(27));
    assert_eq!(logout.header("content-type"), Some("application/json"));
    assert_eq!(logout.body, fixture_body(27), "fixture 27 body");
    let (name, value, attributes) =
        cookie_parts(logout.header("set-cookie").expect("logout cookie"));
    assert_eq!(name, "auth_token");
    assert!(value.is_empty(), "logout must clear the value");
    assert!(attributes.iter().any(|attribute| attribute == "path=/"));
    assert!(attributes.iter().any(|attribute| attribute == "httponly"));
    assert!(attributes.iter().any(|attribute| attribute == "max-age=0"));
    assert!(attributes
        .iter()
        .any(|attribute| attribute == "samesite=lax"));
    assert!(
        !attributes
            .iter()
            .any(|attribute| attribute.starts_with("secure")),
        "Secure must not be set on LAN HTTP"
    );

    let logs = server.stop();
    assert!(!logs.contains(ADMIN_PASSWORD), "password leaked to logs");
    assert!(!logs.contains(DEVICE_TOKEN), "device token leaked to logs");
}

#[tokio::test]
async fn api_admin_device_actions_implement_unblock_revoke_delete() {
    let database = seeded().await;
    let server = Server::spawn(&database.url(), SECRET, true);
    let cookie = admin_cookie();

    // revoke: status='pending', token_hash=NULL, the live token stops working.
    let revoke = server.request(
        "POST",
        "/api/admin/device/action",
        &[
            ("content-type", "application/json"),
            ("cookie", cookie.as_str()),
        ],
        Some(r#"{"id":1,"action":"revoke"}"#),
    );
    assert_eq!(revoke.status, 200);
    assert_eq!(revoke.body, r#"{"success":true}"#);
    assert_eq!(
        database
            .text("SELECT status FROM device_access WHERE id = 1")
            .await
            .expect("read revoked status")
            .as_deref(),
        Some("pending")
    );
    assert_eq!(
        database
            .text("SELECT token_hash FROM device_access WHERE id = 1")
            .await
            .expect("read revoked token_hash"),
        None,
        "revoke must NULL token_hash"
    );

    let denied = server.request(
        "GET",
        "/api/value?key=menu_data",
        &[("cookie", device_cookie().as_str())],
        None,
    );
    assert_eq!(denied.status, 401, "a revoked device must re-pair");
    assert_eq!(denied.body, r#"{"error":"invalid_token"}"#);

    // delete: remove the row entirely.
    let delete = server.request(
        "POST",
        "/api/admin/device/action",
        &[
            ("content-type", "application/json"),
            ("cookie", cookie.as_str()),
        ],
        Some(r#"{"id":3,"action":"delete"}"#),
    );
    assert_eq!(delete.status, 200);
    assert_eq!(delete.body, r#"{"success":true}"#);
    assert_eq!(
        database
            .scalar("SELECT count(*) FROM device_access WHERE id = 3")
            .await
            .expect("count deleted device"),
        0
    );

    // Unknown/missing action: the preserved empty-body 400 admin quirk.
    for body in [r#"{"id":1,"action":"unknown"}"#, r#"{"id":1}"#] {
        let response = server.request(
            "POST",
            "/api/admin/device/action",
            &[
                ("content-type", "application/json"),
                ("cookie", cookie.as_str()),
            ],
            Some(body),
        );
        assert_eq!(response.status, 400, "{body}");
        assert!(response.body.is_empty(), "legacy empty 400 body: {body}");
    }

    // A recognized action without an id still answers success (no row matched).
    let no_id = server.request(
        "POST",
        "/api/admin/device/action",
        &[
            ("content-type", "application/json"),
            ("cookie", cookie.as_str()),
        ],
        Some(r#"{"action":"delete"}"#),
    );
    assert_eq!(no_id.status, 200);
    assert_eq!(no_id.body, r#"{"success":true}"#);
    assert_eq!(
        database
            .scalar("SELECT count(*) FROM device_access")
            .await
            .expect("count devices after no-op"),
        2
    );

    // Admin routes stay gated by the T10 guard.
    let unauthenticated = server.request("GET", "/api/admin/devices", &[], None);
    assert_eq!(unauthenticated.status, 401);
    assert_eq!(unauthenticated.body, r#"{"error":"token_required"}"#);
}

#[tokio::test]
async fn api_admin_login_fallback_failures_and_https_secure() {
    let database = seeded().await;
    database
        .apply_sql("DELETE FROM settings WHERE key = 'admin_password'")
        .await;
    let server = Server::spawn(&database.url(), SECRET, true);

    // Legacy fallback: a missing admin_password row accepts the literal 'admin'.
    let login = server.request(
        "POST",
        "/admin/login",
        &[("content-type", "application/json")],
        Some(&login_json("admin")),
    );
    assert_eq!(login.status, 200);
    assert_eq!(login.body, r#"{"success":true}"#);
    let set_cookie = login.header("set-cookie").expect("fallback cookie");
    assert_lan_cookie(set_cookie, ADMIN_TOKEN);

    // The seeded password is no longer valid once the row is gone.
    let wrong = server.request(
        "POST",
        "/admin/login",
        &[("content-type", "application/json")],
        Some(&login_json(ADMIN_PASSWORD)),
    );
    assert_eq!(wrong.status, 401);
    assert_eq!(wrong.body, r#"{"success":false}"#);
    assert!(wrong.header("set-cookie").is_none());

    // Missing password and malformed bodies never panic or leak diagnostics.
    let missing = server.request(
        "POST",
        "/admin/login",
        &[("content-type", "application/json")],
        Some("{}"),
    );
    assert_eq!(missing.status, 401);
    assert_eq!(missing.body, r#"{"success":false}"#);

    let malformed = server.request(
        "POST",
        "/admin/login",
        &[("content-type", "application/json")],
        Some(r#"{"password":"#),
    );
    assert_eq!(malformed.status, 400);
    assert_eq!(malformed.body, r#"{"error":"invalid_request"}"#);

    // Behind an HTTPS proxy the same cookie gains Secure (plan §5.2).
    database
        .apply_sql(&format!(
            "INSERT INTO settings(key, value, visible) VALUES('admin_password', '{ADMIN_PASSWORD}', 0)"
        ))
        .await;
    let secure_login = server.request(
        "POST",
        "/admin/login",
        &[
            ("content-type", "application/json"),
            ("x-forwarded-proto", "https"),
        ],
        Some(&login_json(ADMIN_PASSWORD)),
    );
    assert_eq!(secure_login.status, 200);
    let (_, value, attributes) =
        cookie_parts(secure_login.header("set-cookie").expect("https cookie"));
    assert_eq!(value, ADMIN_TOKEN);
    assert!(attributes.iter().any(|attribute| attribute == "secure"));
    assert!(attributes.iter().any(|attribute| attribute == "httponly"));
    assert!(attributes
        .iter()
        .any(|attribute| attribute == "samesite=lax"));
}
