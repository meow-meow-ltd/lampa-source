//! T08 bookmarks API contract tests (golden fixtures 15–20 + legacy quirks).
//!
//! Every test drives the real `lampa-server` binary against a throw-away
//! synthetic v7 database under the OS temp directory (never `/opt/lampa/data`).
//! The fixture seed reproduces `deploy/scripts/make_synthetic_db.py` for the
//! bookmark tables, so the golden bodies are comparable byte-for-byte after
//! timestamp normalisation.

#![allow(dead_code)] // the shared test harness has more helpers than this file uses

#[path = "auth/support.rs"]
mod support;

use sha2::{Digest, Sha256};
use support::{HttpResponse, Server, TestDatabase};

const SECRET: &str = "JBSWY3DPEHPK3PXP";
const DEVICE_TOKEN: &str = "t08-bookmarks-device-token";
const PROFILE: &str = "1";
const T0: i64 = 1_700_000_000_000;

/// Deterministic synthetic bookmarks (same order as `make_synthetic_db.py`).
const BOOKMARKS: [(i64, &str, i64); 12] = [
    (1, "film", 1001),
    (1, "film", 1002),
    (1, "film", 1003),
    (1, "cartoon", 1004),
    (1, "cartoon", 1005),
    (1, "game", 1006),
    (2, "cartoon", 2001),
    (2, "cartoon", 2002),
    (2, "film", 2003),
    (2, "game", 2004),
    (3, "film", 3001),
    (3, "game", 3002),
];

/// Fixture 15/16 bodies in order.
const FIXTURES_15_20: [&str; 6] = [
    include_str!("fixtures/api/15_get_api_bookmarks_dump.json"),
    include_str!("fixtures/api/16_get_api_bookmarks_changelog.json"),
    include_str!("fixtures/api/17_post_api_bookmarks_add.json"),
    include_str!("fixtures/api/18_post_api_bookmarks_remove.json"),
    include_str!("fixtures/api/19_post_api_bookmarks_clear.json"),
    include_str!("fixtures/api/20_post_api_bookmarks_sync.json"),
];

fn card_data(card_id: i64, index: usize) -> String {
    let year = 2018 + (index % 6) as i64;
    format!(
        "{{\"id\":{card_id},\"synthetic\":true,\"title\":\"Golden Fixture Card {card_id:04}\",\"year\":{year}}}"
    )
}

/// Builds the synthetic v7 database with the T03 bookmark seed + one allowed
/// device.
async fn fixture() -> TestDatabase {
    let database = TestDatabase::fixture().await;
    let token_hash = hex::encode(Sha256::digest(DEVICE_TOKEN.as_bytes()));
    let mut sql = format!(
        "INSERT INTO device_access(device_id, status, attempts, token_hash, last_seen_at) \
         VALUES('t08-bookmarks-device', 'allowed', 0, '{token_hash}', {T0});"
    );
    for (index, (profile, kind, card_id)) in BOOKMARKS.iter().enumerate() {
        sql.push_str(&format!(
            "INSERT INTO bookmarks(id, profile_id, type, card_id, data, updated_at) \
             VALUES({}, {profile}, '{kind}', {card_id}, '{}', {});",
            index + 1,
            card_data(*card_id, index),
            T0 + index as i64 * 60_000,
        ));
    }
    for (index, (profile, kind, card_id)) in BOOKMARKS.iter().enumerate() {
        sql.push_str(&format!(
            "INSERT INTO bookmark_events(id, profile_id, action, entity_id, type, card_id, data, \
             updated_at) VALUES({}, {profile}, 'add', {}, '{kind}', {card_id}, '{}', {});",
            index + 1,
            index + 1,
            card_data(*card_id, index),
            T0 + index as i64 * 60_000,
        ));
    }
    sql.push_str(&format!(
        "INSERT INTO bookmark_events(id, profile_id, action, entity_id, type, card_id, data, \
         updated_at) VALUES(13, 1, 'update', 1, 'film', 1001, '{}', {});",
        card_data(1001, 0),
        T0 + 660_000,
    ));
    database.apply_sql(&sql).await;
    database
}

fn fixture_json(contents: &str) -> serde_json::Value {
    serde_json::from_str(contents).expect("fixture JSON")
}

fn cookie() -> String {
    format!("device_token={DEVICE_TOKEN}")
}

fn request_options<'a>(cookie: &'a str, profile: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("cookie", cookie),
        ("profile", profile),
        ("user-agent", "lampa-golden/1.0"),
    ]
}

fn send_fixture(server: &Server, fixture: &serde_json::Value) -> HttpResponse {
    let request = &fixture["request"];
    let method = request["method"].as_str().expect("fixture method");
    let path = request["path"].as_str().expect("fixture path");
    let cookie = cookie();
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(object) = request["headers"].as_object() {
        for (name, value) in object {
            if name.eq_ignore_ascii_case("cookie") {
                continue;
            }
            if let Some(value) = value.as_str() {
                headers.push((name.clone(), value.to_owned()));
            }
        }
    }
    headers.push(("cookie".to_owned(), cookie));
    let body = match &request["body"] {
        serde_json::Value::Null => None,
        value => Some(serde_json::to_string(value).expect("fixture body")),
    };
    let refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    server.request(method, path, &refs, body.as_deref())
}

/// Normalises `updated_at` numbers/`<ts>` placeholders for comparison (R3).
fn normalize_timestamps(body: &str) -> String {
    const KEY: &str = "\"updated_at\":";
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(position) = rest.find(KEY) {
        let key_end = position + KEY.len();
        out.push_str(&rest[..key_end]);
        let tail = &rest[key_end..];
        if let Some(stripped) = tail.strip_prefix('"') {
            if let Some(end) = stripped.find('"') {
                out.push_str("\"<ts>\"");
                rest = &stripped[end + 1..];
                continue;
            }
        }
        let digits = tail.chars().take_while(char::is_ascii_digit).count();
        assert!(digits > 0, "unexpected updated_at value in {body:?}");
        out.push_str("\"<ts>\"");
        rest = &tail[digits..];
    }
    out.push_str(rest);
    out
}

fn assert_fixture(server: &Server, contents: &str, index: usize) {
    let fixture = fixture_json(contents);
    let response = send_fixture(server, &fixture);
    let expected_status = fixture["response"]["status"]
        .as_u64()
        .expect("fixture status") as u16;
    let expected_body = fixture["response"]["body"]
        .as_str()
        .expect("fixture body")
        .to_owned();
    assert_eq!(
        response.status, expected_status,
        "fixture {index} status (body: {})",
        response.body
    );
    assert_eq!(
        normalize_timestamps(&response.body),
        normalize_timestamps(&expected_body),
        "fixture {index} body"
    );
    // The dump/quirk content types are part of the contract.
    if let Some(expected) = fixture["response"]["headers"]
        .as_array()
        .and_then(|headers| {
            headers
                .iter()
                .find(|pair| pair[0].as_str() == Some("Content-Type"))
        })
        .and_then(|pair| pair[1].as_str())
    {
        assert_eq!(
            response.header("content-type"),
            Some(expected),
            "fixture {index} content-type"
        );
    }
}

fn get_dump(server: &Server, profile: &str) -> HttpResponse {
    let cookie = cookie();
    server.request(
        "GET",
        "/api/bookmarks/dump",
        &request_options(&cookie, profile),
        None,
    )
}

fn post_json(server: &Server, path: &str, body: &serde_json::Value, profile: &str) -> HttpResponse {
    let cookie = cookie();
    let mut headers = request_options(&cookie, profile);
    headers.push(("content-type", "application/json"));
    let body = serde_json::to_string(body).expect("request body");
    server.request("POST", path, &headers, Some(&body))
}

fn post_form(server: &Server, path: &str, body: &str, profile: &str) -> HttpResponse {
    let cookie = cookie();
    let mut headers = request_options(&cookie, profile);
    headers.push((
        "content-type",
        "application/x-www-form-urlencoded; charset=UTF-8",
    ));
    server.request("POST", path, &headers, Some(body))
}

async fn db_int(database: &TestDatabase, sql: &str) -> i64 {
    database.scalar(sql).await.expect("scalar query")
}

#[tokio::test]
async fn api_bookmarks_fixtures_15_to_20_match_the_golden_bodies() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    for (offset, contents) in FIXTURES_15_20.iter().enumerate() {
        assert_fixture(&server, contents, 15 + offset);
    }

    // Post-sequence state: sync replaced profile 1 with four bookmarks and
    // appended one `add` event per accepted card. Event ids: seed max 13, add
    // 14, remove 15, clear 16, sync 17–20 → version 20.
    let dump = get_dump(&server, PROFILE);
    assert_eq!(dump.status, 200);
    let body = dump.json();
    assert_eq!(body["version"], 20);
    assert_eq!(body["bookmarks"].as_array().expect("bookmarks").len(), 4);
    assert_eq!(
        body["bookmarks"]
            .as_array()
            .expect("bookmarks")
            .iter()
            .map(|row| row["card_id"].as_i64().expect("card_id"))
            .collect::<Vec<_>>(),
        vec![1004, 1001, 1002, 9007]
    );
    assert_eq!(
        db_int(
            &database,
            "SELECT count(*) FROM bookmark_events WHERE profile_id = 1 AND action = 'add' \
             AND id > 16",
        )
        .await,
        4
    );
    assert_eq!(
        db_int(
            &database,
            "SELECT count(*) FROM bookmarks WHERE profile_id = 1",
        )
        .await,
        4
    );
    let logs = server.stop();
    assert!(
        !logs.contains("bookmarks API database error"),
        "no database errors expected:\n{logs}"
    );
}

#[tokio::test]
async fn api_bookmarks_dump_content_type_version_and_raw_data() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    let response = get_dump(&server, PROFILE);
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("content-type"),
        Some("text/plain; charset=utf-8"),
        "legacy dump quirk: text/plain content type with a JSON body"
    );
    let body: serde_json::Value = response.json();
    assert_eq!(body["version"], 13, "version = MAX(bookmark_events.id)");
    let rows = body["bookmarks"].as_array().expect("bookmarks array");
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0]["type"], "film");
    assert_eq!(rows[0]["card_id"], 1001);
    assert_eq!(
        rows[0]["data"].as_str().expect("data string"),
        card_data(1001, 0),
        "the stored JSON text is returned byte-for-byte"
    );
    assert_eq!(
        rows.iter()
            .map(|row| row["id"].as_i64().expect("id"))
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        database
            .text("SELECT data FROM bookmarks WHERE id = 1")
            .await
            .expect("read data")
            .as_deref(),
        Some(card_data(1001, 0).as_str())
    );

    // The X-Profile fallback is part of the legacy profile resolution.
    let cookie = cookie();
    let response = server.request(
        "GET",
        "/api/bookmarks/dump",
        &[("cookie", cookie.as_str()), ("x-profile", "1")],
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["version"], 13);
}

#[tokio::test]
async fn api_bookmarks_sync_is_transactional_and_leaves_no_partial_events() {
    let database = fixture().await;
    // Fail only the second card's event insert, after the first bookmark and
    // event were written inside the transaction.
    database
        .apply_sql(
            "CREATE TRIGGER t08_fail_bookmark_event BEFORE INSERT ON bookmark_events \
             WHEN NEW.card_id = 9008 BEGIN SELECT RAISE(ABORT, 'forced t08 rollback'); END",
        )
        .await;
    let server = Server::spawn(&database.url(), SECRET, true);

    let response = post_json(
        &server,
        "/api/bookmarks/sync",
        &serde_json::json!({
            "data": "{\"film\":[{\"id\":9001}],\"game\":[{\"id\":9008}]}"
        }),
        PROFILE,
    );
    assert_eq!(response.status, 500);
    assert_eq!(
        response.body, r#"{"secuses":false,"error":"internal_error"}"#,
        "raw SQL/driver text must never be serialised"
    );
    assert!(!response.body.contains("forced"));

    assert_eq!(
        db_int(&database, "SELECT count(*) FROM bookmarks").await,
        12,
        "the DELETE must be rolled back"
    );
    assert_eq!(
        db_int(&database, "SELECT count(*) FROM bookmark_events").await,
        13,
        "no partial events may survive the rollback"
    );
    assert_eq!(
        db_int(
            &database,
            "SELECT count(*) FROM bookmarks WHERE card_id IN (9001, 9008)"
        )
        .await,
        0
    );
    assert_eq!(get_dump(&server, PROFILE).json()["version"], 13);
    server.stop();
}

#[tokio::test]
async fn api_bookmarks_clear_requires_the_literal_group_type() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    // `type` must be the literal "group"; `group` carries the bookmark type.
    for body in [
        serde_json::json!({"type": "film", "group": "film"}),
        serde_json::json!({"type": "group"}),
        serde_json::json!({"group": "film"}),
    ] {
        let response = post_json(&server, "/api/bookmarks/clear", &body, PROFILE);
        assert_eq!(response.status, 400, "body: {}", response.body);
        assert_eq!(
            response.body, r#"{"secuses":false,"text":"Invalid payload"}"#,
            "body: {body}"
        );
    }
    assert_eq!(
        db_int(&database, "SELECT count(*) FROM bookmarks").await,
        12
    );
    assert_eq!(
        db_int(&database, "SELECT count(*) FROM bookmark_events").await,
        13
    );

    let response = post_json(
        &server,
        "/api/bookmarks/clear",
        &serde_json::json!({"type": "group", "group": "film"}),
        PROFILE,
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.body, r#"{"secuses":true}"#);

    assert_eq!(
        db_int(
            &database,
            "SELECT count(*) FROM bookmarks WHERE profile_id = 1 AND type = 'film'",
        )
        .await,
        0
    );
    assert_eq!(
        db_int(
            &database,
            "SELECT count(*) FROM bookmarks WHERE profile_id = 1",
        )
        .await,
        3,
        "only the film group is cleared"
    );
    assert_eq!(
        database
            .text(
                "SELECT typeof(entity_id) FROM bookmark_events \
                 WHERE id = (SELECT MAX(id) FROM bookmark_events)",
            )
            .await
            .expect("typeof entity_id")
            .as_deref(),
        Some("text"),
        "clear stores the group name in the typeless entity_id column"
    );
    assert_eq!(
        database
            .text(
                "SELECT CAST(entity_id AS TEXT) FROM bookmark_events \
                 WHERE id = (SELECT MAX(id) FROM bookmark_events)",
            )
            .await
            .expect("entity_id")
            .as_deref(),
        Some("film")
    );
    let dump = get_dump(&server, PROFILE).json();
    assert_eq!(
        dump["version"], 14,
        "the clear event increments the version"
    );
    assert_eq!(dump["bookmarks"].as_array().expect("bookmarks").len(), 3);
    server.stop();
}

#[tokio::test]
async fn api_bookmarks_add_update_remove_events_and_raw_json_data() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    // `add` stores the data string exactly as sent (member order/spacing kept).
    let raw = r#"{"z":1, "a":[2,3], "title":"Кіно"}"#;
    let response = post_json(
        &server,
        "/api/bookmarks/add",
        &serde_json::json!({"type": "film", "card_id": 9001, "data": raw}),
        PROFILE,
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.body, r#"{"secuses":true}"#);
    assert_eq!(
        database
            .text("SELECT data FROM bookmarks WHERE card_id = 9001")
            .await
            .expect("data")
            .as_deref(),
        Some(raw),
        "the data column is opaque JSON text"
    );
    let added_id = db_int(
        &database,
        "SELECT id FROM bookmarks WHERE profile_id = 1 AND card_id = 9001",
    )
    .await;
    let added_id_text = added_id.to_string();
    assert_eq!(
        db_int(
            &database,
            "SELECT count(*) FROM bookmark_events WHERE action = 'add' AND entity_id = \
             (SELECT id FROM bookmarks WHERE card_id = 9001)",
        )
        .await,
        1,
        "add event entity_id is the new bookmark row id"
    );

    // A second add for the same card updates the row and appends `update`.
    let response = post_json(
        &server,
        "/api/bookmarks/add",
        &serde_json::json!({"type": "film", "card_id": 9001, "data": "{\"z\":2}"}),
        PROFILE,
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        db_int(
            &database,
            "SELECT count(*) FROM bookmarks WHERE card_id = 9001"
        )
        .await,
        1
    );
    assert_eq!(
        database
            .text("SELECT data FROM bookmarks WHERE card_id = 9001")
            .await
            .expect("data")
            .as_deref(),
        Some("{\"z\":2}")
    );
    assert_eq!(
        database
            .text("SELECT CAST(entity_id AS TEXT) FROM bookmark_events ORDER BY id DESC LIMIT 1",)
            .await
            .expect("entity")
            .as_deref(),
        Some(added_id_text.as_str())
    );
    assert_eq!(
        database
            .text("SELECT action FROM bookmark_events ORDER BY id DESC LIMIT 1")
            .await
            .expect("action")
            .as_deref(),
        Some("update")
    );

    // Validation order matches the legacy route.
    let response = post_json(
        &server,
        "/api/bookmarks/add",
        &serde_json::json!({"type": "film", "card_id": 9001}),
        PROFILE,
    );
    assert_eq!(response.status, 400);
    assert_eq!(response.body, r#"{"secuses":false,"text":"Missing data"}"#);
    let response = post_json(
        &server,
        "/api/bookmarks/add",
        &serde_json::json!({"type": "film", "card_id": "abc", "data": "{}"}),
        PROFILE,
    );
    assert_eq!(response.status, 400);
    assert_eq!(
        response.body,
        r#"{"secuses":false,"text":"Invalid payload"}"#
    );

    // Remove by the legacy `id` field always appends the `remove` event.
    let response = post_json(
        &server,
        "/api/bookmarks/remove",
        &serde_json::json!({"type": "film", "card_id": 9001, "id": added_id}),
        PROFILE,
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        db_int(
            &database,
            "SELECT count(*) FROM bookmarks WHERE card_id = 9001"
        )
        .await,
        0
    );
    assert_eq!(
        database
            .text("SELECT action FROM bookmark_events ORDER BY id DESC LIMIT 1")
            .await
            .expect("action")
            .as_deref(),
        Some("remove")
    );
    assert_eq!(
        database
            .text("SELECT data FROM bookmark_events ORDER BY id DESC LIMIT 1")
            .await
            .expect("data"),
        None
    );
    assert_eq!(
        database
            .text("SELECT CAST(entity_id AS TEXT) FROM bookmark_events ORDER BY id DESC LIMIT 1")
            .await
            .expect("entity")
            .as_deref(),
        Some(added_id_text.as_str())
    );

    // Removing a card that is not bookmarked is a silent success (no event).
    let events_before = db_int(&database, "SELECT count(*) FROM bookmark_events").await;
    let response = post_json(
        &server,
        "/api/bookmarks/remove",
        &serde_json::json!({"type": "film", "card_id": 123456}),
        PROFILE,
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        db_int(&database, "SELECT count(*) FROM bookmark_events").await,
        events_before
    );

    // jQuery/urlencoded client path: form fields and a JSON string `data`.
    let response = post_form(
        &server,
        "/api/bookmarks/add",
        "type=film&card_id=9002&data=%7B%22id%22%3A9002%7D",
        PROFILE,
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        database
            .text("SELECT data FROM bookmarks WHERE card_id = 9002")
            .await
            .expect("form data")
            .as_deref(),
        Some("{\"id\":9002}")
    );

    // Missing profile (header and body) is the legacy 400.
    let cookie = cookie();
    let response = server.request(
        "POST",
        "/api/bookmarks/add",
        &[
            ("cookie", cookie.as_str()),
            ("content-type", "application/json"),
        ],
        Some(r#"{"type":"film","card_id":1,"data":"{}"}"#),
    );
    assert_eq!(response.status, 400);
    assert_eq!(
        response.body,
        r#"{"secuses":false,"text":"Missing profile"}"#
    );
    server.stop();
}
