//! Helpers for the binary-level DB integration test (TEST-ONLY).
//!
//! Builds synthetic v7 databases from `tests/fixtures/schema7.sql` (never a
//! production/staging file), spawns the real `lampa-server` binary on a random
//! loopback port, and speaks minimal HTTP/1.1 over `std::net`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Connection;

const SCHEMA7_SQL: &str = include_str!("../fixtures/schema7.sql");
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Temporary directory holding a synthetic database.
pub struct TestDatabase {
    dir: PathBuf,
    /// Path to `<dir>/database.sqlite`.
    pub database_path: PathBuf,
}

impl TestDatabase {
    /// Empty temp directory; the database file does not exist yet.
    pub fn empty() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lampa-db-layer-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create integration temp dir");
        let database_path = dir.join("database.sqlite");
        Self { dir, database_path }
    }

    /// Applies `schema7.sql` to a fresh database in a temp directory.
    pub async fn fixture() -> Self {
        let test = Self::empty();
        let options = SqliteConnectOptions::new()
            .filename(&test.database_path)
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

        let objects = test
            .scalar("SELECT count(*) FROM sqlite_master")
            .await
            .expect("count sqlite_schema objects");
        assert_eq!(
            objects, 23,
            "fixture must reproduce 23 sqlite_schema objects"
        );
        test
    }

    /// Applies arbitrary test SQL to the existing database.
    pub async fn apply_sql(&self, sql: &str) {
        let options = SqliteConnectOptions::new()
            .filename(&self.database_path)
            .create_if_missing(false);
        let mut connection = sqlx::SqliteConnection::connect_with(&options)
            .await
            .expect("open temp database for SQL");
        sqlx::raw_sql(sql)
            .execute(&mut connection)
            .await
            .expect("test SQL");
        connection.close().await.expect("close temp connection");
    }

    /// Reads one integer scalar.
    pub async fn scalar(&self, sql: &str) -> Result<i64, sqlx::Error> {
        let options = SqliteConnectOptions::new()
            .filename(&self.database_path)
            .create_if_missing(false);
        let mut connection = sqlx::SqliteConnection::connect_with(&options).await?;
        let value = sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&mut connection)
            .await?;
        connection.close().await?;
        Ok(value)
    }

    /// `sqlite:<path>?mode=rw` URL.
    pub fn url(&self) -> String {
        format!("sqlite:{}?mode=rw", self.database_path.display())
    }

    /// Sidecar path (`<db>-wal` etc.).
    pub fn sidecar(&self, suffix: &str) -> PathBuf {
        let mut path = self.database_path.as_os_str().to_os_string();
        path.push(suffix);
        PathBuf::from(path)
    }

    /// Whole-file bytes (for zero-write assertions).
    pub fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.database_path).expect("read temp database")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A running `lampa-server` child process with captured logs.
pub struct ServerProcess {
    child: Option<Child>,
    port: u16,
}

impl ServerProcess {
    /// Spawns the compiled binary against `database_url` and waits for the socket.
    pub fn spawn(database_url: &str) -> Self {
        let port = reserve_port();
        let child = Command::new(env!("CARGO_BIN_EXE_lampa-server"))
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .env("DATABASE_URL", database_url)
            .env("RUST_LOG", "info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn lampa-server binary");

        let mut server = Self {
            child: Some(child),
            port,
        };
        server.wait_until_listening();
        server
    }

    fn wait_until_listening(&mut self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("lampa-server did not start listening on port {}", self.port);
    }

    /// GET `path`, returning `(status, body)`.
    pub fn get(&self, path: &str) -> (u16, String) {
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.port)).expect("connect to lampa-server");
        stream
            .set_read_timeout(Some(HTTP_TIMEOUT))
            .expect("set read timeout");
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        .expect("write request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("read HTTP response");

        let status_line = response.lines().next().expect("status line");
        let status = status_line
            .split_whitespace()
            .nth(1)
            .expect("status code")
            .parse::<u16>()
            .expect("numeric status");
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default();
        (status, body)
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

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Reserves an ephemeral loopback port (bound then released for the child).
pub fn reserve_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
    listener.local_addr().expect("local addr").port()
}
