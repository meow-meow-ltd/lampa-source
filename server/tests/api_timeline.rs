//! T08 timeline API contract tests (golden fixtures 21–23 + legacy quirks).
//!
//! Every test drives the real `lampa-server` binary against a throw-away
//! synthetic v7 database under the OS temp directory (never `/opt/lampa/data`).
//! The fixture seed reproduces `deploy/scripts/make_synthetic_db.py` for the
//! timeline tables, so the golden bodies are comparable byte-for-byte.

#![allow(dead_code)] // the shared test harness has more helpers than this file uses

#[path = "auth/support.rs"]
mod support;

use sha2::{Digest, Sha256};
use support::{HttpResponse, Server, TestDatabase};

const SECRET: &str = "JBSWY3DPEHPK3PXP";
const DEVICE_TOKEN: &str = "t08-timeline-device-token";
const PROFILE: &str = "1";
const T0: i64 = 1_700_000_000_000;

/// Deterministic synthetic timeline rows (same order as `make_synthetic_db.py`).
const TIMELINES: [(i64, &str, i64, i64, i64); 10] = [
    (1, "golden-hash-000a", 10, 600, 6000),
    (1, "golden-hash-000b", 25, 1500, 6000),
    (1, "golden-hash-000c", 40, 2400, 6000),
    (1, "golden-hash-000d", 55, 3300, 6000),
    (1, "golden-hash-000e", 70, 4200, 6000),
    (2, "golden-hash-001a", 5, 150, 3000),
    (2, "golden-hash-001b", 20, 600, 3000),
    (2, "golden-hash-001c", 35, 1050, 3000),
    (2, "golden-hash-001d", 50, 1500, 3000),
    (2, "golden-hash-001e", 65, 1950, 3000),
];

/// Fixtures 21–23 in request order.
const FIXTURES_21_23: [&str; 3] = [
    include_str!("fixtures/api/21_get_api_timeline_dump.json"),
    include_str!("fixtures/api/22_get_api_timeline_changelog.json"),
    include_str!("fixtures/api/23_post_api_timeline_update.json"),
];

fn timeline_data(profile: i64, hash: &str, percent: i64, time: i64, duration: i64) -> String {
    // `json.dumps(..., sort_keys=True, separators=(",", ":"))` as seeded.
    format!(
        "{{\"duration\":{duration},\"hash\":\"{hash}\",\"percent\":{percent},\"profile\":{profile},\"time\":{time}}}"
    )
}

/// Builds the synthetic v7 database with the T03 timeline seed + one allowed
/// device.
async fn fixture() -> TestDatabase {
    let database = TestDatabase::fixture().await;
    let token_hash = hex::encode(Sha256::digest(DEVICE_TOKEN.as_bytes()));
    let mut sql = format!(
        "INSERT INTO device_access(device_id, status, attempts, token_hash, last_seen_at) \
         VALUES('t08-timeline-device', 'allowed', 0, '{token_hash}', {T0});"
    );
    for (index, (profile, hash, percent, time, duration)) in TIMELINES.iter().enumerate() {
        let data = timeline_data(*profile, hash, *percent, *time, *duration);
        sql.push_str(&format!(
            "INSERT INTO timeline(profile_id, hash, data, updated_at) \
             VALUES({profile}, '{hash}', '{data}', {});",
            T0 + index as i64 * 60_000,
        ));
        sql.push_str(&format!(
            "INSERT INTO timeline_events(id, profile_id, hash, data, updated_at) \
             VALUES({}, {profile}, '{hash}', '{data}', {});",
            index + 1,
            T0 + index as i64 * 60_000,
        ));
    }
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

/// Normalises `updated_at` numbers/`<ts>` placeholders (no timeline field uses
/// it today, but the helper keeps the comparison rule uniform with fixtures 15–20).
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

fn get(server: &Server, path: &str, profile: &str) -> HttpResponse {
    let cookie = cookie();
    server.request("GET", path, &request_options(&cookie, profile), None)
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
async fn api_timeline_fixtures_21_to_23_match_the_golden_bodies() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    for (offset, contents) in FIXTURES_21_23.iter().enumerate() {
        assert_fixture(&server, contents, 21 + offset);
    }

    // Fixture 23 upserted profile 1's first hash and appended event id 11.
    let stored = database
        .text("SELECT data FROM timeline WHERE profile_id = 1 AND hash = 'golden-hash-000a'")
        .await
        .expect("read data")
        .expect("row exists");
    assert_eq!(
        stored,
        r#"{"hash":"golden-hash-000a","percent":80,"time":4800,"duration":6000,"profile":1}"#
    );
    assert_eq!(
        db_int(&database, "SELECT count(*) FROM timeline_events").await,
        11
    );
    let event = database
        .text("SELECT data FROM timeline_events WHERE id = 11 AND profile_id = 1")
        .await
        .expect("event data")
        .expect("event exists");
    assert_eq!(event, stored);

    let dump = get(&server, "/api/timeline/dump", PROFILE);
    assert_eq!(dump.status, 200);
    let body = dump.json();
    assert_eq!(body["version"], 11);
    assert_eq!(body["timelines"].as_object().expect("timelines").len(), 5);
    assert_eq!(body["timelines"]["golden-hash-000a"]["percent"], 80);
    assert!(
        dump.body.contains(
            r#""golden-hash-000a":{"hash":"golden-hash-000a","percent":80,"time":4800,"duration":6000,"profile":1}"#
        ),
        "updated row keeps the legacy field order: {}",
        dump.body
    );
    server.stop();
}

#[tokio::test]
async fn api_timeline_dump_and_changelog_content_types_versions_and_since() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    let dump = get(&server, "/api/timeline/dump", PROFILE);
    assert_eq!(dump.status, 200);
    assert_eq!(
        dump.header("content-type"),
        Some("text/plain; charset=utf-8"),
        "legacy dump quirk: text/plain content type with a JSON body"
    );
    let body = dump.json();
    assert_eq!(body["version"], 5, "version = MAX(timeline_events.id)");
    let timelines = body["timelines"].as_object().expect("timelines");
    assert_eq!(timelines.len(), 5);
    assert_eq!(
        timelines["golden-hash-000a"],
        serde_json::json!({
            "duration": 6000,
            "hash": "golden-hash-000a",
            "percent": 10,
            "profile": 1,
            "time": 600,
        }),
        "stored JSON text is parsed and returned with its hash"
    );

    let changelog = get(&server, "/api/timeline/changelog?since=0", PROFILE);
    assert_eq!(changelog.status, 200);
    assert_eq!(
        changelog.header("content-type"),
        Some("application/json"),
        "changelog is a JSON (not text/plain) response"
    );
    assert_eq!(
        changelog.json()["timelines"]
            .as_object()
            .expect("map")
            .len(),
        5
    );

    let after = get(&server, "/api/timeline/changelog?since=5", PROFILE);
    assert_eq!(after.status, 200);
    assert_eq!(after.json()["version"], 5);
    assert_eq!(
        after.json()["timelines"].as_object().expect("map").len(),
        0,
        "events at or below `since` are excluded"
    );

    // Profile scoping: profile 2 owns the other five seeded rows.
    let second = get(&server, "/api/timeline/dump", "2");
    let body = second.json();
    assert_eq!(body["version"], 10);
    assert_eq!(body["timelines"].as_object().expect("map").len(), 5);
    server.stop();
}

#[tokio::test]
async fn api_timeline_update_defaults_params_upsert_and_text_hash() {
    let database = fixture().await;
    let server = Server::spawn(&database.url(), SECRET, true);

    // Missing values default through `value || 0`; a 4-character hash is valid.
    let response = post_json(
        &server,
        "/api/timeline/update",
        &serde_json::json!({"hash": "abcd"}),
        PROFILE,
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.body, r#"{"secuses":true}"#);
    assert_eq!(
        database
            .text("SELECT data FROM timeline WHERE profile_id = 1 AND hash = 'abcd'")
            .await
            .expect("data")
            .as_deref(),
        Some(r#"{"hash":"abcd","percent":0,"time":0,"duration":0,"profile":1}"#)
    );
    assert_eq!(
        db_int(&database, "SELECT MAX(id) FROM timeline_events").await,
        11
    );

    // `body.params` wins over the top-level body; the stored `profile` field is
    // the payload value even when it differs from the row's profile.
    let response = post_json(
        &server,
        "/api/timeline/update",
        &serde_json::json!({
            "hash": "ignored-top-level",
            "params": {
                "hash": "abcdefghij",
                "percent": 25,
                "time": 10,
                "duration": 40,
                "profile": 2,
            }
        }),
        PROFILE,
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        database
            .text("SELECT data FROM timeline WHERE profile_id = 1 AND hash = 'abcdefghij'")
            .await
            .expect("data")
            .as_deref(),
        Some(r#"{"hash":"abcdefghij","percent":25,"time":10,"duration":40,"profile":2}"#)
    );
    assert_eq!(
        db_int(
            &database,
            "SELECT count(*) FROM timeline WHERE hash = 'ignored-top-level'",
        )
        .await,
        0
    );

    // The jQuery/urlencoded client path stores form values as strings, exactly
    // like the legacy body-parser.
    let response = post_form(
        &server,
        "/api/timeline/update",
        "hash=form-hash&percent=33&time=5&duration=15",
        PROFILE,
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        database
            .text("SELECT data FROM timeline WHERE hash = 'form-hash'")
            .await
            .expect("data")
            .as_deref(),
        Some(r#"{"hash":"form-hash","percent":"33","time":"5","duration":"15","profile":1}"#)
    );

    // Missing/blank/non-string hash is the legacy 400.
    for body in [
        serde_json::json!({}),
        serde_json::json!({"hash": ""}),
        serde_json::json!({"hash": 5}),
    ] {
        let response = post_json(&server, "/api/timeline/update", &body, PROFILE);
        assert_eq!(response.status, 400, "body: {body}");
        assert_eq!(
            response.body, r#"{"secuses":false,"text":"Missing hash"}"#,
            "body: {body}"
        );
    }

    // `hash` is a TEXT column and is never truncated: the fixture hash is 16
    // characters and the 10-character hash above is stored verbatim.
    for table in ["timeline", "timeline_events"] {
        assert_eq!(
            database
                .text(&format!(
                    "SELECT type FROM pragma_table_info('{table}') WHERE name = 'hash'"
                ))
                .await
                .expect("pragma_table_info")
                .as_deref(),
            Some("TEXT"),
            "{table}.hash must be TEXT"
        );
    }
    assert_eq!(
        db_int(
            &database,
            "SELECT length(hash) FROM timeline WHERE hash = 'abcdefghij'",
        )
        .await,
        10
    );
    assert_eq!(
        db_int(
            &database,
            "SELECT length(hash) FROM timeline WHERE hash = 'golden-hash-000a'",
        )
        .await,
        16,
        "16-character content hashes are not truncated"
    );
    server.stop();
}

#[tokio::test]
async fn api_timeline_never_truncates_dump_or_changelog_rows() {
    let database = fixture().await;
    // 250 additional rows/events for profile 7 — well past any implicit limit.
    let mut sql = String::new();
    for index in 0..250_i64 {
        let hash = format!("bulk-{index:03}");
        let data = format!(
            "{{\"duration\":100,\"hash\":\"{hash}\",\"percent\":{},\"profile\":7,\"time\":1}}",
            index % 100
        );
        sql.push_str(&format!(
            "INSERT INTO timeline(profile_id, hash, data, updated_at) \
             VALUES(7, '{hash}', '{data}', {});",
            T0 + index,
        ));
        sql.push_str(&format!(
            "INSERT INTO timeline_events(profile_id, hash, data, updated_at) \
             VALUES(7, '{hash}', '{data}', {});",
            T0 + index,
        ));
    }
    database.apply_sql(&sql).await;
    let server = Server::spawn(&database.url(), SECRET, true);

    let dump = get(&server, "/api/timeline/dump", "7").json();
    assert_eq!(dump["version"], 260);
    let timelines = dump["timelines"].as_object().expect("timelines");
    assert_eq!(timelines.len(), 250, "no row limit on the dump");
    let keys: Vec<&str> = timelines.keys().map(String::as_str).collect();
    assert_eq!(keys.first().copied(), Some("bulk-000"));
    assert_eq!(keys.last().copied(), Some("bulk-249"));

    let changelog = get(&server, "/api/timeline/changelog?since=0", "7").json();
    assert_eq!(changelog["version"], 260);
    assert_eq!(
        changelog["timelines"].as_object().expect("timelines").len(),
        250,
        "no event limit on the changelog"
    );
    server.stop();
}

#[tokio::test]
async fn api_timeline_parse_data_overrides_hash_and_falls_back() {
    let database = fixture().await;
    database
        .apply_sql(&format!(
            "INSERT INTO timeline(profile_id, hash, data, updated_at) VALUES \
             (1, 'late-hash', '{{\"percent\":5}}', {}), \
             (1, 'bad-json', 'not-json', {}), \
             (1, 'arr-hash', '[1,2]', {}), \
             (1, 'num-hash', '5', {});",
            T0 + 900_000,
            T0 + 900_001,
            T0 + 900_002,
            T0 + 900_003,
        ))
        .await;
    let server = Server::spawn(&database.url(), SECRET, true);

    let response = get(&server, "/api/timeline/dump", PROFILE);
    assert_eq!(response.status, 200);
    let body = response.json();
    let timelines = body["timelines"].as_object().expect("timelines");
    assert_eq!(
        timelines["late-hash"],
        serde_json::json!({"percent": 5, "hash": "late-hash"}),
        "a missing hash member is appended"
    );
    assert_eq!(
        timelines["bad-json"],
        serde_json::json!({"hash": "bad-json"}),
        "invalid JSON degrades to a hash-only object"
    );
    assert_eq!(
        timelines["arr-hash"],
        serde_json::json!([1, 2]),
        "arrays keep their parsed value (JS array property is ignored)"
    );
    assert_eq!(timelines["num-hash"], serde_json::json!(5));

    // A mismatched stored hash is replaced by the row hash.
    database
        .apply_sql(
            "UPDATE timeline SET data = '{\"hash\":\"stale\",\"percent\":7}' \
             WHERE profile_id = 1 AND hash = 'golden-hash-000b'",
        )
        .await;
    // Read again on a fresh server (the pooled connection is read-only for the
    // test's own UPDATE, but a restart guarantees a fresh schema view).
    server.stop();
    let server = Server::spawn(&database.url(), SECRET, true);
    let body = get(&server, "/api/timeline/dump", PROFILE).json();
    assert_eq!(
        body["timelines"]["golden-hash-000b"],
        serde_json::json!({"hash": "golden-hash-000b", "percent": 7})
    );

    // Missing profile is the legacy 400.
    let cookie = cookie();
    let response = server.request(
        "GET",
        "/api/timeline/dump",
        &[("cookie", cookie.as_str())],
        None,
    );
    assert_eq!(response.status, 400);
    assert_eq!(
        response.body,
        r#"{"secuses":false,"text":"Missing profile"}"#
    );
    server.stop();
}
