//! Binary-level DB-layer integration test (T04).
//!
//! Unlike the unit tests inside `src/db/**`, these tests drive the *real*
//! composition root: the compiled `lampa-server` binary opens a temp synthetic v7
//! database, passes (or fails) the startup guard, and exposes `/healthz` and
//! `/readyz`. Only throw-away databases under `/tmp` are involved — never
//! `/opt/lampa/data` or `/opt/lampa/staging-data`.

#[path = "db/support.rs"]
mod support;

use support::{ServerProcess, TestDatabase};

#[tokio::test]
async fn ready_v7_database_serves_readyz_200_and_healthz_200() {
    let database = TestDatabase::fixture().await;
    let server = ServerProcess::spawn(&database.url());

    let (health_status, health_body) = server.get("/healthz");
    assert_eq!(health_status, 200, "healthz body: {health_body}");
    assert_eq!(health_body, r#"{"status":"ok"}"#);

    let (ready_status, ready_body) = server.get("/readyz");
    assert_eq!(ready_status, 200, "readyz body: {ready_body}");
    let ready: serde_json::Value = serde_json::from_str(&ready_body).expect("readyz JSON body");
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["db"], "ok");
    assert_eq!(ready["schema_version"], 7);

    let logs = server.stop();
    assert!(
        logs.contains("database ready (schema guard passed)"),
        "startup log must report the passed guard:\n{logs}"
    );
    assert!(
        !logs.contains("CRITICAL"),
        "healthy startup must not log CRITICAL:\n{logs}"
    );

    // The fixture file is still v7/delete after the server's read-only guard and
    // the server created no migration bookkeeping table.
    let objects = database
        .scalar("SELECT count(*) FROM sqlite_master")
        .await
        .expect("count objects");
    assert_eq!(objects, 23);
    let migrations = database
        .scalar("SELECT count(*) FROM sqlite_master WHERE name = '_sqlx_migrations'")
        .await
        .expect("look for migration bookkeeping");
    assert_eq!(migrations, 0, "no migration bookkeeping is allowed");
    assert!(!database.sidecar("-wal").exists());
    assert!(!database.sidecar("-shm").exists());
}

#[tokio::test]
async fn schema_mismatch_is_503_with_critical_log_and_zero_writes() {
    let database = TestDatabase::fixture().await;
    database
        .apply_sql("UPDATE schema_version SET version = 6")
        .await;
    let before = database.bytes();

    let server = ServerProcess::spawn(&database.url());

    let (health_status, _) = server.get("/healthz");
    assert_eq!(health_status, 200, "liveness must stay DB-independent");

    let (ready_status, ready_body) = server.get("/readyz");
    assert_eq!(ready_status, 503, "readyz body: {ready_body}");
    let ready: serde_json::Value = serde_json::from_str(&ready_body).expect("readyz JSON body");
    assert_eq!(ready["status"], "not_ready");
    assert_eq!(ready["db"], "unavailable");
    assert_eq!(ready["reason"], "schema_version_mismatch");
    assert!(!ready_body.contains("SELECT"));
    assert!(!ready_body.contains("schema_version="));

    let logs = server.stop();
    assert!(
        logs.contains("CRITICAL"),
        "failed guard must log CRITICAL:\n{logs}"
    );
    assert!(
        logs.contains("schema_version_mismatch"),
        "log must carry the sanitised reason code:\n{logs}"
    );

    assert_eq!(
        database.bytes(),
        before,
        "a refused schema must not be modified (zero writes)"
    );
    assert!(!database.sidecar("-journal").exists());
    assert!(!database.sidecar("-wal").exists());
    assert!(!database.sidecar("-shm").exists());
}

#[tokio::test]
async fn missing_database_is_503_and_is_never_created() {
    let database = TestDatabase::empty();
    assert!(!database.database_path.exists());
    let url = database.url();

    let server = ServerProcess::spawn(&url);
    let (health_status, _) = server.get("/healthz");
    assert_eq!(health_status, 200);

    let (ready_status, ready_body) = server.get("/readyz");
    assert_eq!(ready_status, 503, "readyz body: {ready_body}");
    let ready: serde_json::Value = serde_json::from_str(&ready_body).expect("readyz JSON body");
    assert_eq!(ready["reason"], "missing_file");

    let logs = server.stop();
    assert!(
        logs.contains("CRITICAL"),
        "missing DB must log CRITICAL:\n{logs}"
    );

    assert!(
        !database.database_path.exists(),
        "mode=rw must never create the database file"
    );
    assert!(!database.sidecar("-wal").exists());
    assert!(!database.sidecar("-shm").exists());
}
