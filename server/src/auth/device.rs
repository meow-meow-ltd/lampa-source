//! Device access tokens: generation, SHA-256 hashing and `device_access` access.
//!
//! Node parity (`backend/routes/pin.js`, `backend/middleware.js`):
//!
//! * a successful PIN verify generates `crypto.randomBytes(32).toString('hex')`
//!   (64 lowercase hex chars) and stores
//!   `crypto.createHash('sha256').update(token).digest('hex')` — lowercase hex,
//!   byte-compatible with [`hash_token`];
//! * each success overwrites `token_hash`, so the previous token stops working;
//! * failed attempts increment `attempts` and flip `status` to `blocked` at 5;
//! * `last_seen_at` is written on authenticated requests, throttled to at most
//!   once per 30 s (plan §5.3).
//!
//! Secrets never leave this module: [`DeviceRow`] does not carry `token_hash`
//! and nothing here logs a token or its digest.

use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::auth::LAST_SEEN_THROTTLE_MS;

/// Raw device-token length in bytes (Node `crypto.randomBytes(32)`).
pub const TOKEN_BYTES: usize = 32;
/// Hex length of a generated device token.
pub const TOKEN_HEX_CHARS: usize = TOKEN_BYTES * 2;
/// Failed PIN attempts allowed per device before it is blocked (Node `maxAttempts`).
pub const MAX_FAILED_ATTEMPTS: i64 = 5;
/// Legacy `device_access.status` values.
pub const STATUS_ALLOWED: &str = "allowed";
/// Legacy pending state (fresh/re-voked device).
pub const STATUS_PENDING: &str = "pending";
/// Legacy blocked state (5 failed PIN attempts).
pub const STATUS_BLOCKED: &str = "blocked";

/// One `device_access` row without the secret digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    /// Primary key (`device_access.id`).
    pub id: i64,
    /// Client-generated legacy device identifier.
    pub device_id: String,
    /// `allowed` / `pending` / `blocked`.
    pub status: String,
    /// Consecutive failed PIN attempts.
    pub attempts: i64,
    /// Last authenticated request (milliseconds since epoch), if any.
    pub last_seen_at: Option<i64>,
}

/// Result of recording a failed PIN attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureRecord {
    /// Attempt counter after the increment.
    pub attempts: i64,
    /// Whether this attempt blocked the device.
    pub blocked: bool,
}

/// Generates a fresh CSPRNG device token as 64 lowercase hex characters.
pub fn generate_token() -> String {
    let mut bytes = [0_u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    debug_assert_eq!(token.len(), TOKEN_HEX_CHARS);
    token
}

/// Lowercase SHA-256 hex digest of a device token (Node-compatible).
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Loads a device by its legacy `device_id`.
pub async fn find_by_device_id(
    pool: &SqlitePool,
    device_id: &str,
) -> Result<Option<DeviceRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, device_id, status, attempts, last_seen_at \
         FROM device_access WHERE device_id = ?",
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(device_row))
}

/// Loads a device by the SHA-256 hex digest of the presented token.
pub async fn find_by_token_hash(
    pool: &SqlitePool,
    token_hash: &str,
) -> Result<Option<DeviceRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, device_id, status, attempts, last_seen_at \
         FROM device_access WHERE token_hash = ? LIMIT 1",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(device_row))
}

fn device_row(row: sqlx::sqlite::SqliteRow) -> DeviceRow {
    DeviceRow {
        id: row.get("id"),
        device_id: row.get("device_id"),
        status: row.get("status"),
        attempts: row.get::<Option<i64>, _>("attempts").unwrap_or(0),
        last_seen_at: row.get("last_seen_at"),
    }
}

/// Records a successful PIN verify exactly like the legacy route.
///
/// Both branches reset `attempts`, allow the device, clear `blocked_until`,
/// stamp `last_attempt_at`/`last_seen_at`, and overwrite `token_hash` — the old
/// token is therefore invalid from this statement on.
pub async fn record_success(
    pool: &SqlitePool,
    device_id: &str,
    existing: Option<&DeviceRow>,
    token_hash: &str,
    ip: &str,
    user_agent: Option<&str>,
    now_ms: i64,
) -> Result<(), sqlx::Error> {
    if existing.is_some() {
        sqlx::query(
            "UPDATE device_access SET status = 'allowed', attempts = 0, \
             last_attempt_at = ?, last_seen_at = ?, token_hash = ?, ip = ?, \
             user_agent = ?, blocked_until = NULL WHERE device_id = ?",
        )
        .bind(now_ms)
        .bind(now_ms)
        .bind(token_hash)
        .bind(ip)
        .bind(user_agent)
        .bind(device_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO device_access \
             (last_attempt_at, last_seen_at, token_hash, ip, user_agent, device_id, status, attempts) \
             VALUES (?, ?, ?, ?, ?, ?, 'allowed', 0)",
        )
        .bind(now_ms)
        .bind(now_ms)
        .bind(token_hash)
        .bind(ip)
        .bind(user_agent)
        .bind(device_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Records a failed PIN attempt; at [`MAX_FAILED_ATTEMPTS`] the device is blocked.
pub async fn record_failure(
    pool: &SqlitePool,
    device_id: &str,
    existing: Option<&DeviceRow>,
    ip: &str,
    user_agent: Option<&str>,
    now_ms: i64,
) -> Result<FailureRecord, sqlx::Error> {
    let attempts = existing.map_or(0, |row| row.attempts) + 1;
    let status = if attempts >= MAX_FAILED_ATTEMPTS {
        STATUS_BLOCKED
    } else {
        STATUS_PENDING
    };
    if existing.is_some() {
        sqlx::query(
            "UPDATE device_access SET attempts = ?, last_attempt_at = ?, status = ?, \
             ip = ?, user_agent = ? WHERE device_id = ?",
        )
        .bind(attempts)
        .bind(now_ms)
        .bind(status)
        .bind(ip)
        .bind(user_agent)
        .bind(device_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO device_access \
             (attempts, last_attempt_at, status, ip, user_agent, device_id) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(attempts)
        .bind(now_ms)
        .bind(status)
        .bind(ip)
        .bind(user_agent)
        .bind(device_id)
        .execute(pool)
        .await?;
    }
    Ok(FailureRecord {
        attempts,
        blocked: status == STATUS_BLOCKED,
    })
}

/// Whether `last_seen_at` is old enough to be written again (≤1 write / 30 s).
pub fn should_touch_last_seen(last_seen_at: Option<i64>, now_ms: i64) -> bool {
    match last_seen_at {
        Some(seen) => now_ms.saturating_sub(seen) >= LAST_SEEN_THROTTLE_MS,
        None => true,
    }
}

/// Throttled `last_seen_at` update; returns whether a row was written.
///
/// The throttle is enforced in SQL as well (`last_seen_at <= now - 30 s`) so a
/// race cannot produce two writes for the same window.
pub async fn touch_last_seen(
    pool: &SqlitePool,
    row_id: i64,
    now_ms: i64,
) -> Result<bool, sqlx::Error> {
    let cutoff = now_ms.saturating_sub(LAST_SEEN_THROTTLE_MS);
    let result = sqlx::query(
        "UPDATE device_access SET last_seen_at = ? \
         WHERE id = ? AND (last_seen_at IS NULL OR last_seen_at <= ?)",
    )
    .bind(now_ms)
    .bind(row_id)
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::TempDb;
    use crate::db::Database;

    /// Hash recorded by Node:
    /// `node -e "require('crypto').createHash('sha256').update('known-token').digest('hex')"`.
    const NODE_KNOWN_TOKEN_HASH: &str =
        "49e2e40e591e61357758299c8cee170fb9fa7da160ec8acf110a4a409d905aaf";

    async fn fixture() -> (TempDb, Database) {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        (temp, database)
    }

    #[test]
    fn sha256_matches_node_recorded_hex() {
        assert_eq!(hash_token("known-token"), NODE_KNOWN_TOKEN_HASH);
        // lowercase output, 64 hex chars, byte-compatible with Node
        assert_eq!(hash_token("known-token").len(), 64);
        assert_eq!(
            hash_token("known-token"),
            hash_token("known-token").to_ascii_lowercase()
        );
        assert_eq!(
            hash_token(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn generated_tokens_are_fresh_32_byte_lowercase_hex() {
        let first = generate_token();
        let second = generate_token();
        assert_eq!(first.len(), TOKEN_HEX_CHARS);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(first, first.to_ascii_lowercase());
        assert_eq!(hex::decode(&first).expect("valid hex").len(), TOKEN_BYTES);
        assert_ne!(first, second, "every verify must mint a new token");
        assert_ne!(first, "0".repeat(64));
        assert_ne!(hash_token(&first), hash_token(&second));
    }

    #[tokio::test]
    async fn success_inserts_then_rotates_the_token_hash() {
        let (_temp, database) = fixture().await;
        let pool = database.pool();

        let first_token = generate_token();
        record_success(
            pool,
            "device-1",
            None,
            &hash_token(&first_token),
            "127.0.0.1",
            Some("ua/1"),
            1_000,
        )
        .await
        .expect("insert success");
        let row = find_by_device_id(pool, "device-1")
            .await
            .expect("read")
            .expect("row exists");
        assert_eq!(row.status, STATUS_ALLOWED);
        assert_eq!(row.attempts, 0);
        assert_eq!(row.last_seen_at, Some(1_000));

        let second_token = generate_token();
        record_success(
            pool,
            "device-1",
            Some(&row),
            &hash_token(&second_token),
            "127.0.0.2",
            None,
            2_000,
        )
        .await
        .expect("update success");
        assert!(
            find_by_token_hash(pool, &hash_token(&first_token))
                .await
                .expect("lookup old")
                .is_none(),
            "the previous token must stop working"
        );
        let current = find_by_token_hash(pool, &hash_token(&second_token))
            .await
            .expect("lookup new")
            .expect("new token row");
        assert_eq!(current.device_id, "device-1");
        assert_eq!(current.status, STATUS_ALLOWED);
        assert_eq!(current.last_seen_at, Some(2_000));
    }

    #[tokio::test]
    async fn five_failures_block_the_device_and_success_resets_attempts() {
        let (_temp, database) = fixture().await;
        let pool = database.pool();

        let mut existing = None;
        for attempt in 1..=MAX_FAILED_ATTEMPTS {
            let record = record_failure(
                pool,
                "device-2",
                existing.as_ref(),
                "10.0.0.1",
                None,
                attempt,
            )
            .await
            .expect("record failure");
            assert_eq!(record.attempts, attempt);
            assert_eq!(record.blocked, attempt >= MAX_FAILED_ATTEMPTS);
            existing = find_by_device_id(pool, "device-2").await.expect("read");
        }
        let blocked = existing.expect("row exists");
        assert_eq!(blocked.status, STATUS_BLOCKED);
        assert_eq!(blocked.attempts, MAX_FAILED_ATTEMPTS);

        // A later success (not reachable through the handler for blocked
        // devices, but exercised directly) resets counter and status.
        record_success(
            pool,
            "device-2",
            Some(&blocked),
            &hash_token("fresh-token"),
            "10.0.0.1",
            None,
            10,
        )
        .await
        .expect("success resets");
        let allowed = find_by_device_id(pool, "device-2")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(allowed.status, STATUS_ALLOWED);
        assert_eq!(allowed.attempts, 0);
    }

    #[tokio::test]
    async fn last_seen_writes_at_most_once_per_thirty_seconds() {
        let (_temp, database) = fixture().await;
        let pool = database.pool();
        record_success(
            pool,
            "device-3",
            None,
            &hash_token("token-3"),
            "10.0.0.1",
            None,
            1_000_000,
        )
        .await
        .expect("insert");
        let row = find_by_device_id(pool, "device-3")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.last_seen_at, Some(1_000_000));

        // 29.999 s later: no write.
        assert!(!should_touch_last_seen(row.last_seen_at, 1_029_999));
        assert!(!touch_last_seen(pool, row.id, 1_029_999)
            .await
            .expect("throttled touch"));
        assert_eq!(
            find_by_device_id(pool, "device-3")
                .await
                .expect("read")
                .expect("row")
                .last_seen_at,
            Some(1_000_000)
        );

        // Exactly 30 s later: one write.
        assert!(should_touch_last_seen(row.last_seen_at, 1_030_000));
        assert!(touch_last_seen(pool, row.id, 1_030_000)
            .await
            .expect("allowed touch"));
        assert_eq!(
            find_by_device_id(pool, "device-3")
                .await
                .expect("read")
                .expect("row")
                .last_seen_at,
            Some(1_030_000)
        );

        // Immediately after: throttled again.
        assert!(!touch_last_seen(pool, row.id, 1_030_001)
            .await
            .expect("throttled touch"));
        assert!(should_touch_last_seen(None, 1));
    }
}
