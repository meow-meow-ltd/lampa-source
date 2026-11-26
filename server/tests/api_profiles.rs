//! T09 end-to-end fixture replay for the legacy profiles/profile-settings routes.
//!
//! Each test builds a throw-away synthetic v7 database under the OS temp
//! directory (never `/opt/lampa/data` or any staging/production file), spawns
//! the real `lampa-server` binary on a random loopback port and replays the
//! committed T03 golden fixtures 09–14 over real HTTP with a seeded, allowed
//! device cookie. The expected `response.body` strings are read from
//! `tests/fixtures/api/*.json`, so the assertion is the byte-exact legacy body
//! (the fixture-parity contract). Extra tests cover the empty-table `Main`
//! fallback, the non-cascading legacy delete and the auth guard.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{ConnectOptions, Connection};

const SCHEMA7_SQL: &str = include_str!("fixtures/schema7.sql");
const DEVICE_TOKEN: &str = "t09-device-token";
const TOTP_SECRET: &str = "JBSWY3DPEHPK3PXP";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Synthetic v7 database (temp only)
// ---------------------------------------------------------------------------

struct TestDatabase {
    dir: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    async fn fixture() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lampa-t09-profiles-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("database.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(false);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("open temp database");
        sqlx::raw_sql(SCHEMA7_SQL)
            .execute(&pool)
            .await
            .expect("apply test-only schema7.sql");
        pool.close().await;
        Self { dir, path }
    }

    async fn apply_sql(&self, sql: &str) {
        let mut connection = self.connect().await;
        sqlx::raw_sql(sql)
            .execute(&mut connection)
            .await
            .unwrap_or_else(|error| panic!("test SQL failed: {error}"));
        connection.close().await.expect("close test connection");
    }

    async fn scalar(&self, sql: &str) -> i64 {
        let mut connection = self.connect().await;
        let value = sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&mut connection)
            .await
            .expect("scalar");
        connection.close().await.expect("close");
        value
    }

    async fn text(&self, sql: &str) -> Option<String> {
        let mut connection = self.connect().await;
        let value = sqlx::query_scalar::<_, Option<String>>(sql)
            .fetch_one(&mut connection)
            .await
            .expect("text");
        connection.close().await.expect("close");
        value
    }

    fn url(&self) -> String {
        format!("sqlite:{}?mode=rw", self.path.display())
    }

    async fn connect(&self) -> sqlx::SqliteConnection {
        SqliteConnectOptions::new()
            .filename(&self.path)
            .create_if_missing(false)
            .busy_timeout(Duration::from_secs(5))
            .connect()
            .await
            .expect("open temp database")
    }

    /// Seeds the three synthetic profiles of the T03 capture database.
    async fn seed_golden_profiles(&self) {
        self.apply_sql(
            "INSERT INTO profiles(id, name, icon, is_main, is_child, sort_order, created_at, updated_at) VALUES \
             (1, 'Main', 'l_1', 1, 0, 1, 1000, 1000), \
             (2, 'Kids', 'l_2', 0, 1, 2, 1000, 1000), \
             (3, 'Guest', 'l_3', 0, 0, 3, 1000, 1000);",
        )
        .await;
    }

    /// Seeds profile_settings for profile 1 (fixture 13) and profile 2.
    async fn seed_golden_settings(&self) {
        self.apply_sql(
            "INSERT INTO profile_settings(profile_id, key, value, updated_at) VALUES \
             (1, 'cinema_mode', 'golden', 1000), \
             (1, 'last_device', 'golden-device-0001', 1000), \
             (1, 'watch_progress', '1234', 1000), \
             (2, 'cinema_mode', 'off', 1000);",
        )
        .await;
    }

    /// Seeds an allowed device whose sha256 token hash matches [`DEVICE_TOKEN`].
    async fn seed_device(&self) {
        let hash = sha256_hex(DEVICE_TOKEN);
        self.apply_sql(&format!(
            "INSERT INTO device_access(device_id, status, attempts, token_hash, last_seen_at) \
             VALUES('t09-device', 'allowed', 0, '{hash}', 1000)"
        ))
        .await;
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value.as_bytes()))
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1 client
// ---------------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }

    fn assert_json_content_type(&self) {
        let content_type = self.header("content-type").unwrap_or_default();
        assert!(
            content_type.starts_with("application/json"),
            "legacy API answers application/json, got {content_type:?}"
        );
    }
}

impl HttpResponse {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|_| panic!("expected JSON body, got: {}", self.body))
    }
}

struct Server {
    child: Option<Child>,
    port: u16,
}

impl Server {
    fn spawn(database_url: &str) -> Self {
        let port = reserve_port();
        let child = Command::new(env!("CARGO_BIN_EXE_lampa-server"))
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .env("DATABASE_URL", database_url)
            .env("TOTP_SECRET", TOTP_SECRET)
            .env("DEVICE_LEGACY_COOKIE", "1")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn lampa-server binary");
        let mut server = Self {
            child: Some(child),
            port,
        };
        server.wait_until_ready();
        server
    }

    fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if self
                .try_request("GET", "/readyz", &[], None)
                .map(|response| response.status == 200)
                .unwrap_or(false)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("lampa-server did not become ready on port {}", self.port);
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> HttpResponse {
        self.try_request(method, path, headers, body)
            .expect("lampa-server request")
    }

    /// Device-authenticated request (the T03 fixtures use cookie `device_token`).
    fn device_request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> HttpResponse {
        let cookie = format!("device_token={DEVICE_TOKEN}");
        let mut all: Vec<(&str, &str)> = vec![("cookie", cookie.as_str())];
        all.extend_from_slice(headers);
        self.request(method, path, &all, body)
    }

    fn try_request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> std::io::Result<HttpResponse> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(body) = body {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        if let Some(body) = body {
            request.push_str(body);
        }
        stream.write_all(request.as_bytes())?;

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        Ok(parse_response(&raw))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn parse_response(raw: &[u8]) -> HttpResponse {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP header terminator");
    let head = String::from_utf8_lossy(&raw[..split]);
    let body_bytes = &raw[split + 4..];

    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .expect("HTTP status line");
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let chunked = headers.iter().any(|(name, value)| {
        name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
    });
    let body = if chunked {
        decode_chunked(body_bytes)
    } else {
        String::from_utf8_lossy(body_bytes).into_owned()
    };
    HttpResponse {
        status,
        headers,
        body,
    }
}

fn decode_chunked(raw: &[u8]) -> String {
    let mut decoded = Vec::new();
    let mut rest = raw;
    while let Some(split) = rest.windows(2).position(|window| window == b"\r\n") {
        let Ok(size) = usize::from_str_radix(String::from_utf8_lossy(&rest[..split]).trim(), 16)
        else {
            break;
        };
        if size == 0 {
            break;
        }
        let data_start = split + 2;
        if rest.len() < data_start + size {
            decoded.extend_from_slice(&rest[data_start..]);
            break;
        }
        decoded.extend_from_slice(&rest[data_start..data_start + size]);
        rest = &rest[data_start + size..];
        rest = rest.strip_prefix(b"\r\n").unwrap_or(rest);
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn reserve_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
    listener.local_addr().expect("local addr").port()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> Value {
    serde_json::from_str(name).expect("fixture JSON")
}

fn fixture_body(fixture: &Value) -> String {
    fixture["response"]["body"]
        .as_str()
        .expect("fixture body string")
        .to_owned()
}

fn fixture_request_body(fixture: &Value) -> Option<String> {
    let body = &fixture["request"]["body"];
    if body.is_null() {
        None
    } else {
        Some(serde_json::to_string(body).expect("request body JSON"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Fixtures 09–12 (profiles) and 13–14 (profile settings), replayed in the
/// fixed T03 order against the compiled server: byte-exact body parity.
#[tokio::test]
async fn api_profiles_fixtures_09_to_14_are_byte_identical() {
    let database = TestDatabase::fixture().await;
    database.seed_golden_profiles().await;
    database.seed_golden_settings().await;
    database.seed_device().await;
    let server = Server::spawn(&database.url());

    // 09 GET /api/profiles/all
    let fixture_09 = fixture(include_str!("fixtures/api/09_get_api_profiles_all.json"));
    let response = server.device_request("GET", "/api/profiles/all", &[], None);
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, fixture_body(&fixture_09));
    response.assert_json_content_type();

    // 10 POST /api/profiles/create
    let fixture_10 = fixture(include_str!(
        "fixtures/api/10_post_api_profiles_create.json"
    ));
    let response = server.device_request(
        "POST",
        "/api/profiles/create",
        &[("content-type", "application/json")],
        fixture_request_body(&fixture_10).as_deref(),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, fixture_body(&fixture_10));
    response.assert_json_content_type();

    // 11 POST /api/profiles/update
    let fixture_11 = fixture(include_str!(
        "fixtures/api/11_post_api_profiles_update.json"
    ));
    let response = server.device_request(
        "POST",
        "/api/profiles/update",
        &[("content-type", "application/json")],
        fixture_request_body(&fixture_11).as_deref(),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, fixture_body(&fixture_11));
    response.assert_json_content_type();

    // 12 POST /api/profiles/delete
    let fixture_12 = fixture(include_str!(
        "fixtures/api/12_post_api_profiles_delete.json"
    ));
    let response = server.device_request(
        "POST",
        "/api/profiles/delete",
        &[("content-type", "application/json")],
        fixture_request_body(&fixture_12).as_deref(),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, fixture_body(&fixture_12));
    response.assert_json_content_type();

    // 13 GET /api/profile/settings (profile from the `profile` header)
    let fixture_13 = fixture(include_str!(
        "fixtures/api/13_get_api_profile_settings.json"
    ));
    let response = server.device_request("GET", "/api/profile/settings", &[("profile", "1")], None);
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, fixture_body(&fixture_13));
    response.assert_json_content_type();

    // 14 POST /api/profile/settings
    let fixture_14 = fixture(include_str!(
        "fixtures/api/14_post_api_profile_settings.json"
    ));
    let response = server.device_request(
        "POST",
        "/api/profile/settings",
        &[("profile", "1"), ("content-type", "application/json")],
        fixture_request_body(&fixture_14).as_deref(),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, fixture_body(&fixture_14));
    response.assert_json_content_type();

    // Database state after the replay: profile 4 was deleted again (back to 3),
    // profile 3 got the update, and the new setting is keyed by (1, golden_flag).
    assert_eq!(database.scalar("SELECT count(*) FROM profiles").await, 3);
    assert_eq!(
        database
            .text("SELECT name FROM profiles WHERE id = 3")
            .await
            .as_deref(),
        Some("Guest")
    );
    assert_eq!(
        database
            .text("SELECT icon FROM profiles WHERE id = 3")
            .await
            .as_deref(),
        Some("l_7")
    );
    assert_eq!(
        database
            .scalar("SELECT is_child FROM profiles WHERE id = 3")
            .await,
        1
    );
    assert_eq!(
        database
            .text("SELECT value FROM profile_settings WHERE profile_id = 1 AND key = 'golden_flag'")
            .await
            .as_deref(),
        Some("on")
    );
}

/// Empty `profiles` table: `GET all` creates exactly one `Main` row and a
/// second read does not duplicate it.
#[tokio::test]
async fn api_profiles_empty_table_creates_exactly_one_main() {
    let database = TestDatabase::fixture().await;
    database.seed_device().await;
    assert_eq!(database.scalar("SELECT count(*) FROM profiles").await, 0);
    let server = Server::spawn(&database.url());

    let response = server.device_request("GET", "/api/profiles/all", &[], None);
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        response.body,
        r#"{"secuses":true,"profiles":[{"id":1,"name":"Main","icon":"l_1","main":true,"child":false,"sort":1}]}"#
    );
    assert_eq!(database.scalar("SELECT count(*) FROM profiles").await, 1);
    assert_eq!(
        database
            .text("SELECT name FROM profiles WHERE id = 1")
            .await
            .as_deref(),
        Some("Main")
    );
    assert_eq!(
        database
            .scalar("SELECT is_main FROM profiles WHERE id = 1")
            .await,
        1
    );

    let response = server.device_request("GET", "/api/profiles/all", &[], None);
    assert_eq!(response.status, 200);
    assert_eq!(database.scalar("SELECT count(*) FROM profiles").await, 1);
}

/// Legacy `POST /api/profiles/delete` is a plain DELETE: child rows survive as
/// orphans (no FK cascade).
#[tokio::test]
async fn api_profiles_delete_is_plain_without_cascade() {
    let database = TestDatabase::fixture().await;
    database.seed_golden_profiles().await;
    database.seed_device().await;
    let server = Server::spawn(&database.url());

    let created = server.device_request(
        "POST",
        "/api/profiles/create",
        &[("content-type", "application/json")],
        Some(r#"{"name":"Orphaned","icon":"l_8"}"#),
    );
    assert_eq!(created.status, 200, "{}", created.body);
    let created: Value = created.json();
    assert_eq!(created["profile"]["id"], 4);
    assert_eq!(created["profile"]["sort"], 4);

    database
        .apply_sql(
            "INSERT INTO bookmarks(profile_id, type, card_id, data, updated_at) \
             VALUES(4, 'book', 42, '{}', 1); \
             INSERT INTO profile_settings(profile_id, key, value, updated_at) \
             VALUES(4, 'menu', '1', 1); \
             INSERT INTO timeline_events(profile_id, hash, data, updated_at) \
             VALUES(4, 'hash', '{}', 1);",
        )
        .await;

    let response = server.device_request(
        "POST",
        "/api/profiles/delete",
        &[("content-type", "application/json")],
        Some(r#"{"id":4}"#),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, r#"{"secuses":true}"#);

    assert_eq!(database.scalar("SELECT count(*) FROM profiles").await, 3);
    assert_eq!(
        database
            .scalar("SELECT count(*) FROM profiles WHERE id = 4")
            .await,
        0
    );
    for table in ["bookmarks", "profile_settings", "timeline_events"] {
        assert_eq!(
            database
                .scalar(&format!(
                    "SELECT count(*) FROM {table} WHERE profile_id = 4"
                ))
                .await,
            1,
            "{table} orphan must survive the legacy delete"
        );
    }
    assert_eq!(
        database
            .scalar("SELECT count(*) FROM pragma_foreign_key_check")
            .await,
        0
    );
}

/// The real T10 auth guard rejects missing/unknown device cookies before any
/// profiles handler runs, and legacy 400s use the captured `{secuses,text}`
/// shape.
#[tokio::test]
async fn api_profiles_guard_and_legacy_400s() {
    let database = TestDatabase::fixture().await;
    database.seed_golden_profiles().await;
    database.seed_device().await;
    let server = Server::spawn(&database.url());

    let response = server.request("GET", "/api/profiles/all", &[], None);
    assert_eq!(response.status, 401);
    assert_eq!(response.body, r#"{"error":"token_required"}"#);

    let response = server.request(
        "GET",
        "/api/profiles/all",
        &[("cookie", "device_token=unknown")],
        None,
    );
    assert_eq!(response.status, 401);
    assert_eq!(response.body, r#"{"error":"invalid_token"}"#);

    let response = server.device_request(
        "POST",
        "/api/profiles/create",
        &[("content-type", "application/json")],
        Some(r#"{"name":"  "}"#),
    );
    assert_eq!(response.status, 400);
    assert_eq!(
        response.body,
        r#"{"secuses":false,"text":"Name is required"}"#
    );

    let response = server.device_request(
        "POST",
        "/api/profiles/update",
        &[("content-type", "application/json")],
        Some(r#"{"id":0}"#),
    );
    assert_eq!(response.status, 400);
    assert_eq!(response.body, r#"{"secuses":false,"text":"Invalid id"}"#);

    // The failed requests wrote nothing.
    assert_eq!(database.scalar("SELECT count(*) FROM profiles").await, 3);
}
