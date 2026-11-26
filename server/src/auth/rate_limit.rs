//! IP rate limiting for `POST /api/pin/verify` (legacy `ip_rate_limits`).
//!
//! Node parity (`backend/rate-limit.js`): 10 failed attempts per IP inside a
//! 15-minute window → the IP is blocked for 15 minutes. The limit is checked
//! **before** the TOTP comparison; a successful verify clears the row.
//!
//! The window is anchored at the first failed attempt of the current window.
//! Unlike the legacy implementation, starting a new window also refreshes
//! `first_attempt_at` (the legacy `UPDATE` branch left the stale anchor behind,
//! which could silently drop a fresh window after the next expiry).

use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// Sliding window length (15 minutes).
pub const WINDOW_MS: i64 = 15 * 60 * 1000;
/// Failed attempts allowed per IP per window.
pub const MAX_ATTEMPTS: i64 = 10;
/// Block duration once [`MAX_ATTEMPTS`] is reached (15 minutes).
pub const BLOCK_MS: i64 = 15 * 60 * 1000;

/// Result of the pre-verify IP check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpStatus {
    /// Whether the IP is currently blocked.
    pub blocked: bool,
    /// Whole seconds until the block expires (`Retry-After`), when blocked.
    pub retry_after: Option<i64>,
}

/// Result of recording one failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureStatus {
    /// Whether this attempt started an IP block.
    pub blocked: bool,
    /// Attempts left before the IP block (`0` once blocked).
    pub attempts_left: i64,
    /// Whole seconds of the block (`Retry-After`), when blocked.
    pub retry_after: Option<i64>,
}

/// Checks the current IP state; an expired window is deleted lazily.
pub async fn check(pool: &SqlitePool, ip: &str, now_ms: i64) -> Result<IpStatus, sqlx::Error> {
    let row = sqlx::query(
        "SELECT attempts, first_attempt_at, blocked_until FROM ip_rate_limits WHERE ip = ?",
    )
    .bind(ip)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(IpStatus {
            blocked: false,
            retry_after: None,
        });
    };

    let blocked_until: Option<i64> = row.get("blocked_until");
    if let Some(until) = blocked_until {
        if until > now_ms {
            return Ok(IpStatus {
                blocked: true,
                retry_after: Some(seconds_until(until, now_ms)),
            });
        }
    }

    let first_attempt_at: i64 = row.get("first_attempt_at");
    if now_ms.saturating_sub(first_attempt_at) > WINDOW_MS {
        sqlx::query("DELETE FROM ip_rate_limits WHERE ip = ?")
            .bind(ip)
            .execute(pool)
            .await?;
    }
    Ok(IpStatus {
        blocked: false,
        retry_after: None,
    })
}

/// Records one failed attempt and starts a 15-minute block at 10.
pub async fn record_failure(
    pool: &SqlitePool,
    ip: &str,
    now_ms: i64,
) -> Result<FailureStatus, sqlx::Error> {
    let row = sqlx::query(
        "SELECT attempts, first_attempt_at, blocked_until FROM ip_rate_limits WHERE ip = ?",
    )
    .bind(ip)
    .fetch_optional(pool)
    .await?;

    let (attempts, first_attempt_at) = match &row {
        None => (1, now_ms),
        Some(existing) => {
            let first_attempt_at: i64 = existing.get("first_attempt_at");
            let previous: i64 = existing.get::<Option<i64>, _>("attempts").unwrap_or(0);
            if now_ms.saturating_sub(first_attempt_at) > WINDOW_MS {
                (1, now_ms)
            } else {
                (previous.saturating_add(1), first_attempt_at)
            }
        }
    };
    let blocked_until = if attempts >= MAX_ATTEMPTS {
        Some(now_ms.saturating_add(BLOCK_MS))
    } else {
        None
    };

    if row.is_some() {
        sqlx::query(
            "UPDATE ip_rate_limits SET attempts = ?, first_attempt_at = ?, blocked_until = ? \
             WHERE ip = ?",
        )
        .bind(attempts)
        .bind(first_attempt_at)
        .bind(blocked_until)
        .bind(ip)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO ip_rate_limits (attempts, first_attempt_at, blocked_until, ip) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(attempts)
        .bind(first_attempt_at)
        .bind(blocked_until)
        .bind(ip)
        .execute(pool)
        .await?;
    }

    Ok(FailureStatus {
        blocked: blocked_until.is_some(),
        attempts_left: (MAX_ATTEMPTS - attempts).max(0),
        retry_after: blocked_until.map(|_| BLOCK_MS / 1000),
    })
}

/// Clears the IP row (legacy success path: `clearIpRateLimit`).
pub async fn clear(pool: &SqlitePool, ip: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM ip_rate_limits WHERE ip = ?")
        .bind(ip)
        .execute(pool)
        .await?;
    Ok(())
}

/// `ceil((deadline - now) / 1000)` with a non-negative floor.
fn seconds_until(deadline_ms: i64, now_ms: i64) -> i64 {
    let remaining = deadline_ms.saturating_sub(now_ms).max(0);
    (remaining + 999) / 1000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::TempDb;
    use crate::db::Database;

    async fn fixture() -> (TempDb, Database) {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        (temp, database)
    }

    #[tokio::test]
    async fn unknown_ip_is_not_blocked() {
        let (_temp, database) = fixture().await;
        let status = check(database.pool(), "203.0.113.1", 1_000)
            .await
            .expect("check");
        assert!(!status.blocked);
        assert_eq!(status.retry_after, None);
    }

    #[tokio::test]
    async fn ten_failures_block_the_ip_for_fifteen_minutes() {
        let (_temp, database) = fixture().await;
        let pool = database.pool();
        let now = 1_700_000_000_000;

        for attempt in 1..MAX_ATTEMPTS {
            let status = record_failure(pool, "203.0.113.2", now)
                .await
                .expect("record");
            assert!(!status.blocked, "attempt {attempt} must not block");
            assert_eq!(status.attempts_left, MAX_ATTEMPTS - attempt);
            assert_eq!(status.retry_after, None);
        }
        let tenth = record_failure(pool, "203.0.113.2", now)
            .await
            .expect("record tenth");
        assert!(tenth.blocked);
        assert_eq!(tenth.attempts_left, 0);
        assert_eq!(tenth.retry_after, Some(900));

        let checked = check(pool, "203.0.113.2", now).await.expect("check");
        assert!(checked.blocked);
        assert_eq!(checked.retry_after, Some(900));
    }

    #[tokio::test]
    async fn expiry_deletes_the_row_and_starts_a_fresh_window() {
        let (_temp, database) = fixture().await;
        let pool = database.pool();
        let start = 1_700_000_000_000;

        for _ in 0..MAX_ATTEMPTS {
            record_failure(pool, "203.0.113.3", start)
                .await
                .expect("record");
        }
        let after_block = start + BLOCK_MS + 1;
        let checked = check(pool, "203.0.113.3", after_block)
            .await
            .expect("check after block");
        assert!(!checked.blocked, "block must expire");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ip_rate_limits")
            .fetch_one(pool)
            .await
            .expect("count");
        assert_eq!(rows, 0, "expired rows are deleted lazily");

        // The next window is anchored now: a new attempt counts as #1.
        let first = record_failure(pool, "203.0.113.3", after_block)
            .await
            .expect("record after expiry");
        assert_eq!(first.attempts_left, MAX_ATTEMPTS - 1);
        let row = sqlx::query("SELECT attempts, first_attempt_at FROM ip_rate_limits")
            .fetch_one(pool)
            .await
            .expect("row");
        assert_eq!(row.get::<Option<i64>, _>("attempts"), Some(1));
        assert_eq!(row.get::<i64, _>("first_attempt_at"), after_block);
    }

    #[tokio::test]
    async fn window_expiry_is_reset_without_a_block_in_between() {
        let (_temp, database) = fixture().await;
        let pool = database.pool();
        let start = 1_700_000_000_000;
        // Nine failed attempts, then wait out the 15-minute window.
        for _ in 0..9 {
            record_failure(pool, "203.0.113.4", start)
                .await
                .expect("record");
        }
        let later = start + WINDOW_MS + 1;
        assert!(
            !check(pool, "203.0.113.4", later)
                .await
                .expect("check")
                .blocked
        );
        let first = record_failure(pool, "203.0.113.4", later)
            .await
            .expect("record next window");
        assert_eq!(first.attempts_left, 9);
    }

    #[tokio::test]
    async fn success_clears_the_ip_row() {
        let (_temp, database) = fixture().await;
        let pool = database.pool();
        record_failure(pool, "203.0.113.5", 1_000)
            .await
            .expect("record");
        clear(pool, "203.0.113.5").await.expect("clear");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ip_rate_limits")
            .fetch_one(pool)
            .await
            .expect("count");
        assert_eq!(rows, 0);
        assert!(
            !check(pool, "203.0.113.5", 1_001)
                .await
                .expect("check")
                .blocked
        );
    }

    #[test]
    fn retry_after_is_rounded_up_to_whole_seconds() {
        assert_eq!(seconds_until(1_000, 1_000), 0);
        assert_eq!(seconds_until(1_001, 1_000), 1);
        assert_eq!(seconds_until(1_999, 1_000), 1);
        assert_eq!(seconds_until(2_000, 1_000), 1);
        assert_eq!(seconds_until(2_001, 1_000), 2);
        assert_eq!(seconds_until(999, 1_000), 0);
    }
}
