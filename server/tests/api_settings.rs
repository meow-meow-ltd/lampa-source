//! T07 API parity: settings routes (fixtures 04–06) against the real binary.
//!
//! Every test builds a throw-away synthetic v7 database under `/tmp` (never
//! `/opt/lampa/data` or staging), spawns `lampa-server` on a random loopback
//! port and replays the T03 golden requests. Response bodies are compared
//! byte-for-byte with the captured fixture bytes; headers are compared for the
//! contract-relevant `Content-Type` (hop-by-hop legacy headers are not emitted
//! by the Rust server).
//!
//! Covered: `GET` masking of `admin_password`/`admin_token`, `PUT` upsert and
//! the `"***"` no-op, `DELETE`, stable sanitised errors and the proof that no
//! SQL text or secret value ever reaches a response.

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

/// The deterministic synthetic rows of `deploy/scripts/make_synthetic_db.py`
/// (20 settings + 3 devices), so the responses line up with the T03 fixtures.
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
        4 => include_str!("fixtures/api/04_get_api_settings.json"),
        5 => include_str!("fixtures/api/05_put_api_settings.json"),
        6 => include_str!("fixtures/api/06_delete_api_settings_key.json"),
        other => panic!("fixture {other} is not used by api_settings"),
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

fn admin_cookie() -> String {
    format!("auth_token={ADMIN_TOKEN}")
}

fn assert_no_sql_text(body: &str) {
    let lowered = body.to_ascii_lowercase();
    for needle in [
        "select ",
        "insert ",
        "update ",
        "delete ",
        "sqlite",
        "no such table",
        "syntax error",
    ] {
        assert!(
            !lowered.contains(needle),
            "response body leaks SQL/diagnostics ({needle}): {body}"
        );
    }
}

#[tokio::test]
async fn api_settings_fixtures_04_05_06_parity() {
    let database = seeded().await;
    let server = Server::spawn(&database.url(), SECRET, true);
    let cookie = admin_cookie();

    // Fixture 04 — GET /api/settings (all rows, secrets masked).
    let get = server.request(
        "GET",
        "/api/settings",
        &[
            ("cookie", cookie.as_str()),
            ("user-agent", "lampa-golden/1.0"),
        ],
        None,
    );
    assert_eq!(get.status, fixture_status(4));
    assert_eq!(
        get.header("content-type"),
        Some("application/json"),
        "fixture 04 content type"
    );
    assert_eq!(get.body, fixture_body(4), "fixture 04 body");
    assert!(!get.body.contains(ADMIN_PASSWORD));
    assert!(!get.body.contains(ADMIN_TOKEN));

    // Fixture 05 — PUT /api/settings {key, value} => {"success":true}.
    let put = server.request(
        "PUT",
        "/api/settings",
        &[
            ("content-type", "application/json"),
            ("cookie", cookie.as_str()),
        ],
        Some(r#"{"key":"torrserver_url","value":"http://synthetic-updated.invalid:9091"}"#),
    );
    assert_eq!(put.status, fixture_status(5));
    assert_eq!(put.header("content-type"), Some("application/json"));
    assert_eq!(put.body, fixture_body(5), "fixture 05 body");
    assert_eq!(
        database
            .text("SELECT value FROM settings WHERE key = 'torrserver_url'")
            .await
            .expect("read updated setting")
            .as_deref(),
        Some("http://synthetic-updated.invalid:9091")
    );

    // Fixture 06 — DELETE /api/settings/{key} => {"success":true}.
    let delete = server.request(
        "DELETE",
        "/api/settings/head_notice",
        &[("cookie", cookie.as_str())],
        None,
    );
    assert_eq!(delete.status, fixture_status(6));
    assert_eq!(delete.header("content-type"), Some("application/json"));
    assert_eq!(delete.body, fixture_body(6), "fixture 06 body");
    assert_eq!(
        database
            .scalar("SELECT count(*) FROM settings WHERE key = 'head_notice'")
            .await
            .expect("count deleted setting"),
        0
    );
}

#[tokio::test]
async fn api_settings_masks_secrets_and_ignores_mask_put() {
    let database = seeded().await;
    let server = Server::spawn(&database.url(), SECRET, true);
    let cookie = admin_cookie();

    let listed = server.request("GET", "/api/settings", &[("cookie", cookie.as_str())], None);
    let rows = listed.json();
    let rows = rows.as_array().expect("settings array");
    for secret_key in ["admin_password", "admin_token"] {
        let row = rows
            .iter()
            .find(|row| row["key"] == secret_key)
            .unwrap_or_else(|| panic!("{secret_key} row present"));
        assert_eq!(row["value"], "***", "{secret_key} must be masked");
        assert_eq!(row["visible"], 0, "{secret_key} stays invisible");
    }

    // Echoing the mask back must not overwrite the stored secret.
    for (key, stored) in [
        ("admin_password", ADMIN_PASSWORD),
        ("admin_token", ADMIN_TOKEN),
    ] {
        let body = format!(r#"{{"key":"{key}","value":"***"}}"#);
        let response = server.request(
            "PUT",
            "/api/settings",
            &[
                ("content-type", "application/json"),
                ("cookie", cookie.as_str()),
            ],
            Some(&body),
        );
        assert_eq!(response.status, 200, "{key} mask PUT");
        assert_eq!(response.body, r#"{"success":true}"#);
        assert_eq!(
            database
                .text(&format!("SELECT value FROM settings WHERE key = '{key}'"))
                .await
                .expect("read stored secret")
                .as_deref(),
            Some(stored),
            "{key} must keep its value after a masked PUT"
        );
    }

    // A real secret update is stored but still masked on every read.
    let update = server.request(
        "PUT",
        "/api/settings",
        &[
            ("content-type", "application/json"),
            ("cookie", cookie.as_str()),
        ],
        Some(r#"{"key":"admin_password","value":"rotated-secret"}"#),
    );
    assert_eq!(update.status, 200);
    assert_eq!(
        database
            .text("SELECT value FROM settings WHERE key = 'admin_password'")
            .await
            .expect("read rotated secret")
            .as_deref(),
        Some("rotated-secret")
    );
    let listed = server.request("GET", "/api/settings", &[("cookie", cookie.as_str())], None);
    assert!(listed
        .body
        .contains(r#"{"key":"admin_password","value":"***","visible":0}"#));
    assert!(!listed.body.contains("rotated-secret"));

    // The legacy contract is an upsert: a new opaque key appears with visible=0.
    let upsert = server.request(
        "PUT",
        "/api/settings",
        &[
            ("content-type", "application/json"),
            ("cookie", cookie.as_str()),
        ],
        Some(r#"{"key":"brand_new_key","value":"opaque value"}"#),
    );
    assert_eq!(upsert.status, 200);
    assert_eq!(upsert.body, r#"{"success":true}"#);
    assert_eq!(
        database
            .text("SELECT value FROM settings WHERE key = 'brand_new_key'")
            .await
            .expect("read upserted key")
            .as_deref(),
        Some("opaque value")
    );
    let visible = database
        .scalar("SELECT visible FROM settings WHERE key = 'brand_new_key'")
        .await
        .expect("read upserted visible flag");
    assert_eq!(visible, 0, "new settings keep the legacy visible=0 default");
}

#[tokio::test]
async fn api_settings_delete_and_error_bodies_are_sanitised() {
    let database = seeded().await;
    let server = Server::spawn(&database.url(), SECRET, true);
    let cookie = admin_cookie();

    // Deleting a missing key still answers the legacy success body.
    let missing = server.request(
        "DELETE",
        "/api/settings/never_existed",
        &[("cookie", cookie.as_str())],
        None,
    );
    assert_eq!(missing.status, 200);
    assert_eq!(missing.body, r#"{"success":true}"#);

    // A malformed PUT gets a stable JSON error, never a driver message.
    let malformed = server.request(
        "PUT",
        "/api/settings",
        &[
            ("content-type", "application/json"),
            ("cookie", cookie.as_str()),
        ],
        Some(r#"{"key":"torrserver_url","#),
    );
    assert_eq!(malformed.status, 400);
    assert_eq!(malformed.body, r#"{"error":"invalid_request"}"#);
    assert_no_sql_text(&malformed.body);
    assert_eq!(
        database
            .text("SELECT value FROM settings WHERE key = 'torrserver_url'")
            .await
            .expect("read untouched setting")
            .as_deref(),
        Some("http://synthetic.invalid:9091"),
        "a malformed PUT must not write"
    );

    // Missing admin credential is rejected by the T10 guard with the frozen
    // sanitised body (no SQL text either).
    let unauthenticated = server.request("GET", "/api/settings", &[], None);
    assert_eq!(unauthenticated.status, 401);
    assert_eq!(unauthenticated.body, r#"{"error":"token_required"}"#);
    assert_no_sql_text(&unauthenticated.body);

    // The server never logs the masked secret values.
    let logs = server.stop();
    assert!(!logs.contains(ADMIN_PASSWORD), "password leaked to logs");
}
