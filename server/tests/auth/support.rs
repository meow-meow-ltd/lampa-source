//! Helpers for the T10 auth end-to-end test (TEST-ONLY).
//!
//! Builds throw-away synthetic v7 databases from `tests/fixtures/schema7.sql`,
//! spawns the real `lampa-server` binary on a random loopback port and speaks
//! minimal HTTP/1.1 with cookies. Only `/tmp` databases are involved — never
//! `/opt/lampa/data` or any staging/production file.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{ConnectOptions, Connection};

const SCHEMA7_SQL: &str = include_str!("../fixtures/schema7.sql");
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Temporary directory holding a synthetic database.
pub struct TestDatabase {
    dir: PathBuf,
    /// Path to `<dir>/database.sqlite`.
    pub database_path: PathBuf,
}

impl TestDatabase {
    /// Applies `schema7.sql` to a fresh database in a temp directory.
    pub async fn fixture() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lampa-auth-e2e-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create auth e2e temp dir");
        let database_path = dir.join("database.sqlite");

        let options = SqliteConnectOptions::new()
            .filename(&database_path)
            .create_if_missing(true);
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

        let database = Self { dir, database_path };
        let objects = database
            .scalar("SELECT count(*) FROM sqlite_master")
            .await
            .expect("count sqlite_schema objects");
        assert_eq!(objects, 23, "fixture must reproduce 23 objects");
        database
    }

    /// Applies arbitrary test SQL to the existing database (own connection).
    pub async fn apply_sql(&self, sql: &str) {
        let mut connection = self.connect().await;
        sqlx::raw_sql(sql)
            .execute(&mut connection)
            .await
            .unwrap_or_else(|error| panic!("test SQL failed: {error}"));
        connection.close().await.expect("close test connection");
    }

    /// Reads one integer scalar.
    pub async fn scalar(&self, sql: &str) -> Result<i64, sqlx::Error> {
        let mut connection = self.connect().await;
        let value = sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&mut connection)
            .await;
        connection.close().await?;
        value
    }

    /// Reads one optional text scalar.
    pub async fn text(&self, sql: &str) -> Result<Option<String>, sqlx::Error> {
        let mut connection = self.connect().await;
        let value = sqlx::query_scalar::<_, Option<String>>(sql)
            .fetch_one(&mut connection)
            .await;
        connection.close().await?;
        value
    }

    /// `sqlite:<path>?mode=rw` URL.
    pub fn url(&self) -> String {
        format!("sqlite:{}?mode=rw", self.database_path.display())
    }

    async fn connect(&self) -> sqlx::SqliteConnection {
        SqliteConnectOptions::new()
            .filename(&self.database_path)
            .create_if_missing(false)
            .busy_timeout(Duration::from_secs(5))
            .connect()
            .await
            .expect("open temp database")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One parsed HTTP/1.1 response.
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Response headers with lowercase names.
    pub headers: Vec<(String, String)>,
    /// Decoded body (chunked transfer encoding is unwrapped).
    pub body: String,
}

impl HttpResponse {
    /// All values of a header (e.g. multiple `set-cookie`).
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// First value of a header.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.header_values(name).into_iter().next()
    }

    /// Parses the body as JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|_| panic!("expected JSON body, got: {}", self.body))
    }
}

/// A running `lampa-server` child process with captured logs.
pub struct Server {
    child: Option<Child>,
    port: u16,
}

impl Server {
    /// Spawns the compiled binary against `database_url` and waits for readiness.
    pub fn spawn(database_url: &str, totp_secret: &str, legacy_device_cookie: bool) -> Self {
        let port = reserve_port();
        let child = Command::new(env!("CARGO_BIN_EXE_lampa-server"))
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .env("DATABASE_URL", database_url)
            .env("TOTP_SECRET", totp_secret)
            .env(
                "DEVICE_LEGACY_COOKIE",
                if legacy_device_cookie { "1" } else { "0" },
            )
            .env("RUST_LOG", "info")
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
            if let Ok(response) = self.try_get("/readyz") {
                if response.status == 200 {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("lampa-server did not become ready on port {}", self.port);
    }

    fn try_get(&self, path: &str) -> std::io::Result<HttpResponse> {
        self.try_request("GET", path, &[], None)
    }

    /// Sends a request; panics on transport errors.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> HttpResponse {
        self.try_request(method, path, headers, body)
            .expect("lampa-server request")
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

    /// Convenience: JSON POST.
    pub fn post_json(&self, path: &str, body: &str, headers: &[(&str, &str)]) -> HttpResponse {
        let mut all = vec![("content-type", "application/json")];
        all.extend_from_slice(headers);
        self.request("POST", path, &all, Some(body))
    }

    /// Kills the child and returns its combined stdout+stderr.
    pub fn stop(mut self) -> String {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        self.collect_output()
    }

    fn collect_output(&mut self) -> String {
        match self.child.take() {
            Some(child) => {
                let output = child.wait_with_output().expect("wait for lampa-server");
                let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
                combined.push_str(&String::from_utf8_lossy(&output.stderr));
                combined
            }
            None => String::new(),
        }
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

/// Parses a raw HTTP/1.1 response (content-length or chunked).
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

/// Decodes an HTTP/1.1 chunked body into text.
fn decode_chunked(raw: &[u8]) -> String {
    let mut decoded = Vec::new();
    let mut rest = raw;
    while let Some(split) = rest.windows(2).position(|window| window == b"\r\n") {
        let size_text = String::from_utf8_lossy(&rest[..split]);
        let Ok(size) = usize::from_str_radix(size_text.trim(), 16) else {
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

/// Current TOTP code for `secret` (SHA-1/6 digits/30 s), like speakeasy.
pub fn totp_now(secret: &str) -> String {
    totp_at(secret, unix_seconds())
}

/// TOTP code that does not match any step accepted by `window = 1` right now.
pub fn wrong_totp_now(secret: &str) -> String {
    let now = unix_seconds();
    let accepted = [
        totp_at(secret, now.saturating_sub(30)),
        totp_at(secret, now),
        totp_at(secret, now + 30),
    ];
    for candidate in 0..1_000_000_u32 {
        let code = format!("{candidate:06}");
        if !accepted.contains(&code) {
            return code;
        }
    }
    unreachable!("all six-digit codes accepted cannot happen")
}

/// Deterministic TOTP code generation for a Unix second count.
fn totp_at(secret: &str, unix_seconds: u64) -> String {
    use totp_rs::{Algorithm, Secret, TOTP};
    let bytes = Secret::Encoded(secret.to_owned())
        .to_bytes()
        .expect("base32 test secret");
    let totp = TOTP::new_unchecked(Algorithm::SHA1, 6, 1, 30, bytes);
    totp.generate(unix_seconds)
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// Reserves an ephemeral loopback port (bound then released for the child).
pub fn reserve_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
    listener.local_addr().expect("local addr").port()
}
