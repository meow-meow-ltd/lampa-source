//! T09 end-to-end fixture replay for the legacy user-backup routes.
//!
//! The legacy API is inverted (T03 fixtures 24-25): `POST /api/users/backup/export`
//! is the **write** side and `GET /api/users/backup/import` the **read** side.
//! Each test builds a throw-away synthetic v7 database under the OS temp
//! directory (never `/opt/lampa/data`), spawns the real `lampa-server` binary on
//! a random loopback port and replays the committed golden fixtures over HTTP
//! with a seeded, allowed device cookie, asserting the byte-exact legacy body.

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

/// Fixtures 24-25 replayed in order: `export` writes, `import` reads, both
/// bodies byte-identical to the committed fixtures.
#[tokio::test]
async fn api_users_backup_fixtures_24_25_are_byte_identical() {
    let database = TestDatabase::fixture().await;
    database.seed_device().await;
    let server = Server::spawn(&database.url());

    let fixture_24 = fixture(include_str!(
        "fixtures/api/24_post_api_users_backup_export.json"
    ));
    let fixture_25 = fixture(include_str!(
        "fixtures/api/25_get_api_users_backup_import.json"
    ));

    // 24 POST /api/users/backup/export - write side.
    let response = server.device_request(
        "POST",
        "/api/users/backup/export",
        &[("content-type", "application/json")],
        fixture_request_body(&fixture_24).as_deref(),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, fixture_body(&fixture_24));
    response.assert_json_content_type();

    // The client blob is stored verbatim in settings.backup_latest.
    let payload = fixture_24["request"]["body"]["data"]
        .as_str()
        .expect("fixture data string")
        .to_owned();
    assert_eq!(
        database
            .text("SELECT value FROM settings WHERE key = 'backup_latest'")
            .await
            .as_deref(),
        Some(payload.as_str())
    );
    assert_eq!(
        database
            .scalar("SELECT visible FROM settings WHERE key = 'backup_latest'")
            .await,
        0
    );

    // 25 GET /api/users/backup/import - read side.
    let response = server.device_request("GET", "/api/users/backup/import", &[], None);
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, fixture_body(&fixture_25));
    response.assert_json_content_type();
}

/// The direction is inverted: `import` alone never writes and answers `null`.
#[tokio::test]
async fn api_users_backup_direction_is_inverted() {
    let database = TestDatabase::fixture().await;
    database.seed_device().await;
    let server = Server::spawn(&database.url());

    let response = server.device_request("GET", "/api/users/backup/import", &[], None);
    assert_eq!(response.status, 200);
    assert_eq!(response.body, r#"{"secuses":true,"data":null}"#);
    assert_eq!(
        database
            .scalar("SELECT count(*) FROM settings WHERE key = 'backup_latest'")
            .await,
        0,
        "import (read) must not create the backup row"
    );

    let exported = r#"{"data":"{\"direction\":\"export-writes\"}"}"#;
    let response = server.device_request(
        "POST",
        "/api/users/backup/export",
        &[("content-type", "application/json")],
        Some(exported),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, r#"{"secuses":true}"#);

    let response = server.device_request("GET", "/api/users/backup/import", &[], None);
    assert_eq!(response.status, 200);
    assert_eq!(
        response.body,
        r#"{"secuses":true,"data":{"direction":"export-writes"}}"#
    );
}

/// Export validation keeps the captured legacy 400 bodies and writes nothing.
#[tokio::test]
async fn api_users_backup_export_validation() {
    let database = TestDatabase::fixture().await;
    database.seed_device().await;
    let server = Server::spawn(&database.url());

    for (body, expected) in [
        (r#"{}"#, r#"{"secuses":false,"text":"Missing data"}"#),
        (
            r#"{"data":""}"#,
            r#"{"secuses":false,"text":"Missing data"}"#,
        ),
        (
            r#"{"data":123}"#,
            r#"{"secuses":false,"text":"Missing data"}"#,
        ),
        (
            r#"{"data":"not json"}"#,
            r#"{"secuses":false,"text":"Invalid JSON"}"#,
        ),
    ] {
        let response = server.device_request(
            "POST",
            "/api/users/backup/export",
            &[("content-type", "application/json")],
            Some(body),
        );
        assert_eq!(response.status, 400, "{body}: {}", response.body);
        assert_eq!(response.body, expected, "{body}");
    }
    assert_eq!(
        database
            .scalar("SELECT count(*) FROM settings WHERE key = 'backup_latest'")
            .await,
        0,
        "rejected exports must not write"
    );
}

/// `import` answers `data:null` for corrupt or empty stored blobs (legacy
/// JSON.parse guard) and never leaks an error.
#[tokio::test]
async fn api_users_backup_import_tolerates_corrupt_blobs() {
    let database = TestDatabase::fixture().await;
    database.seed_device().await;
    let server = Server::spawn(&database.url());

    for stored in ["", "not json at all", "{broken"] {
        database
            .apply_sql(&format!(
                "INSERT INTO settings(key, value, visible) VALUES('backup_latest', '{}', 0) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
                stored.replace('\'', "''")
            ))
            .await;
        let response = server.device_request("GET", "/api/users/backup/import", &[], None);
        assert_eq!(response.status, 200, "{stored:?}: {}", response.body);
        assert_eq!(
            response.body, r#"{"secuses":true,"data":null}"#,
            "{stored:?}"
        );
    }
}

/// Export overwrites the previous blob and forces `visible = 0` (legacy upsert).
#[tokio::test]
async fn api_users_backup_export_overwrites_and_resets_visible() {
    let database = TestDatabase::fixture().await;
    database.seed_device().await;
    database
        .apply_sql(
            "INSERT INTO settings(key, value, visible) VALUES('backup_latest', '{\"old\":true}', 1);",
        )
        .await;
    let server = Server::spawn(&database.url());

    let response = server.device_request(
        "POST",
        "/api/users/backup/export",
        &[("content-type", "application/json")],
        Some(r#"{"data":"{\"new\":true}"}"#),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        database
            .text("SELECT value FROM settings WHERE key = 'backup_latest'")
            .await
            .as_deref(),
        Some(r#"{"new":true}"#)
    );
    assert_eq!(
        database
            .scalar("SELECT visible FROM settings WHERE key = 'backup_latest'")
            .await,
        0
    );
    assert_eq!(
        database
            .scalar("SELECT count(*) FROM settings WHERE key = 'backup_latest'")
            .await,
        1,
        "one upserted row, not an append"
    );
}

/// The real T10 auth guard rejects missing/unknown device cookies first.
#[tokio::test]
async fn api_users_backup_guard() {
    let database = TestDatabase::fixture().await;
    database.seed_device().await;
    let server = Server::spawn(&database.url());

    for (method, path) in [
        ("POST", "/api/users/backup/export"),
        ("GET", "/api/users/backup/import"),
    ] {
        let response = server.request(method, path, &[], None);
        assert_eq!(response.status, 401, "{method} {path}");
        assert_eq!(response.body, r#"{"error":"token_required"}"#);

        let response = server.request(method, path, &[("cookie", "device_token=unknown")], None);
        assert_eq!(response.status, 401, "{method} {path}");
        assert_eq!(response.body, r#"{"error":"invalid_token"}"#);
    }
}
