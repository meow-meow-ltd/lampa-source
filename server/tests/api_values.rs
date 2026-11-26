//! T07 API parity: values routes (fixtures 07–08) against the real binary.
//!
//! Throw-away synthetic v7 databases under `/tmp`, the real `lampa-server` on a
//! random loopback port, T03 golden request replay. Bodies are compared
//! byte-for-byte with the fixtures; `Content-Type` is checked explicitly.
//!
//! Covered: fixture 07 (`POST /api/values` after the fixture-05 settings update),
//! fixture 08 (`GET /api/value?key=admin_password` => JSON `null`), opaque
//! keys/values, invisible-key masking and the legacy 400 error bodies.

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
        5 => include_str!("fixtures/api/05_put_api_settings.json"),
        7 => include_str!("fixtures/api/07_post_api_values.json"),
        8 => include_str!("fixtures/api/08_get_api_value.json"),
        other => panic!("fixture {other} is not used by api_values"),
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

fn device_cookie() -> String {
    format!("device_token={DEVICE_TOKEN}")
}

#[tokio::test]
async fn api_values_fixtures_07_08_parity() {
    let database = seeded().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    // Capture order: fixture 05 mutates torrserver_url before fixture 07 reads it.
    let update = server.request(
        "PUT",
        "/api/settings",
        &[
            ("content-type", "application/json"),
            ("cookie", admin_cookie().as_str()),
        ],
        Some(r#"{"key":"torrserver_url","value":"http://synthetic-updated.invalid:9091"}"#),
    );
    assert_eq!(update.status, fixture_status(5));

    // Fixture 07 — POST /api/values (device auth).
    let values = server.request(
        "POST",
        "/api/values",
        &[
            ("content-type", "application/json"),
            ("cookie", device_cookie().as_str()),
            ("user-agent", "lampa-golden/1.0"),
        ],
        Some(r#"{"keys":["menu_data","admin_password","torrserver_url"]}"#),
    );
    assert_eq!(values.status, fixture_status(7));
    assert_eq!(values.header("content-type"), Some("application/json"));
    assert_eq!(values.body, fixture_body(7), "fixture 07 body");
    assert!(!values.body.contains(ADMIN_PASSWORD));
    assert!(!values.body.contains(ADMIN_TOKEN));

    // Fixture 08 — GET /api/value?key=admin_password => the JSON literal null.
    let value = server.request(
        "GET",
        "/api/value?key=admin_password",
        &[
            ("cookie", device_cookie().as_str()),
            ("user-agent", "lampa-golden/1.0"),
        ],
        None,
    );
    assert_eq!(value.status, fixture_status(8));
    assert_eq!(value.header("content-type"), Some("application/json"));
    assert_eq!(value.body, fixture_body(8), "fixture 08 body");
    assert_eq!(value.body, "null");
}

#[tokio::test]
async fn api_values_keep_opaque_keys_and_visibility() {
    let database = seeded().await;
    database
        .apply_sql(
            "INSERT INTO settings(key, value, visible) VALUES \
             ('opaque / ключ', 'välue', 1), \
             ('hidden_key', 'hidden-value', 0)",
        )
        .await;
    let server = Server::spawn(&database.url(), SECRET, true);
    let cookie = device_cookie();

    // Opaque keys survive unchanged; invisible and missing keys are null/absent.
    let values = server.request(
        "POST",
        "/api/values",
        &[
            ("content-type", "application/json"),
            ("cookie", cookie.as_str()),
        ],
        Some(r#"{"keys":["opaque / ключ","hidden_key","missing_key","menu_data"]}"#),
    );
    assert_eq!(values.status, 200);
    assert_eq!(
        values.json(),
        serde_json::json!({
            "opaque / ключ": "välue",
            "hidden_key": null,
            "menu_data": "1"
        }),
        "invisible/missing keys must never leak values"
    );
    assert!(!values.body.contains("hidden-value"));

    // The query parameter decoder keeps the opaque key intact.
    let encoded = server.request(
        "GET",
        "/api/value?key=opaque%20%2F%20%D0%BA%D0%BB%D1%8E%D1%87",
        &[("cookie", cookie.as_str())],
        None,
    );
    assert_eq!(encoded.status, 200);
    assert_eq!(encoded.body, r#""välue""#);

    // Invisible and missing keys answer the JSON literal null.
    for key in ["hidden_key", "missing_key", "admin_password"] {
        let response = server.request(
            "GET",
            &format!("/api/value?key={key}"),
            &[("cookie", cookie.as_str())],
            None,
        );
        assert_eq!(response.status, 200, "{key}");
        assert_eq!(response.body, "null", "{key}");
    }

    // A visible key returns the stored JSON string.
    let visible = server.request(
        "GET",
        "/api/value?key=torrserver_url",
        &[("cookie", cookie.as_str())],
        None,
    );
    assert_eq!(visible.status, 200);
    assert_eq!(visible.body, r#""http://synthetic.invalid:9091""#);
}

#[tokio::test]
async fn api_values_errors_are_legacy_shaped_and_sanitised() {
    let database = seeded().await;
    let server = Server::spawn(&database.url(), SECRET, true);
    let cookie = device_cookie();

    for body in [r#"{}"#, r#"{"keys":[]}"#] {
        let response = server.request(
            "POST",
            "/api/values",
            &[
                ("content-type", "application/json"),
                ("cookie", cookie.as_str()),
            ],
            Some(body),
        );
        assert_eq!(response.status, 400, "{body}");
        assert_eq!(
            response.body,
            r#"{"error":"Missing or invalid keys parameter"}"#
        );
        assert!(!response.body.to_ascii_lowercase().contains("select "));
    }

    for path in ["/api/value", "/api/value?key="] {
        let response = server.request("GET", path, &[("cookie", cookie.as_str())], None);
        assert_eq!(response.status, 400, "{path}");
        assert_eq!(response.body, r#"{"error":"Missing key parameter"}"#);
    }

    // Device routes require the T10 credential before any handler work.
    let unauthenticated = server.request("POST", "/api/values", &[], None);
    assert_eq!(unauthenticated.status, 401);
    assert_eq!(unauthenticated.body, r#"{"error":"token_required"}"#);

    // Malformed JSON is a sanitised 400, never a driver message.
    let malformed = server.request(
        "POST",
        "/api/values",
        &[
            ("content-type", "application/json"),
            ("cookie", cookie.as_str()),
        ],
        Some(r#"{"keys":["menu_data""#),
    );
    assert_eq!(malformed.status, 400);
    assert_eq!(
        malformed.body,
        r#"{"error":"Missing or invalid keys parameter"}"#
    );
}
