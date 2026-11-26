//! `timeline` / `timeline_events` repository (legacy semantics, schema v7).
//!
//! Mirrors `backend/routes/timeline.js` (T03 fixtures 21–23):
//! * `dump` → `version = MAX(timeline_events.id)` for the profile and every
//!   `timeline` row ordered by `updated_at ASC`;
//! * `changelog` → events after `since` ordered by `id ASC` (no LIMIT — the live
//!   database holds tens of thousands of rows and the legacy route never
//!   truncates);
//! * `update` → upsert `(profile_id, hash)` and append one `timeline_events`
//!   row with the same JSON text. The `hash` column is TEXT and is stored
//!   verbatim (the schema/domain value is a short opaque string, never a
//!   truncated integer).
//!
//! The JSON text is built in the legacy field order
//! (`hash, percent, time, duration, profile`) and rendered compactly, so a dump
//! after an update serialises exactly like the Node reference. Parsing for the
//! dump/changelog response re-uses the ordered JSON model from
//! `src/bookmarks/json.rs` (included here with `#[path]`; the crate root is
//! outside the T08 write scope).

#![allow(dead_code)] // repository API consumed by the HTTP handler and its tests

// The single-source ordered-JSON model is included from `src/bookmarks/json.rs`
// by both repositories; clippy flags the duplicate module load, which is the
// deliberate cost of keeping the model in one file while the crate root is
// outside the T08 write scope.
#[allow(clippy::duplicate_mod)]
#[path = "../bookmarks/json.rs"]
pub mod json;

use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use self::json::Json;
use crate::db::DbError;

/// One `timeline`/`timeline_events` row exposed to the HTTP layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEntry {
    /// Opaque content hash (TEXT; 4–10 characters in normal data).
    pub hash: String,
    /// Stored JSON text (the progress object serialised by the legacy route).
    pub data: String,
}

/// Progress values accepted by [`TimelineRepo::update`].
#[derive(Debug, Clone)]
pub struct TimelineProgress<'a> {
    /// Opaque content hash (`hash` column, TEXT).
    pub hash: &'a str,
    /// `percent` value; stored as sent (legacy `payload.percent || 0`).
    pub percent: &'a Json,
    /// `time` value, seconds.
    pub time: &'a Json,
    /// `duration` value, seconds.
    pub duration: &'a Json,
    /// `profile` value stored inside the JSON text (legacy `payload.profile ||
    /// profileId`; it may differ from the row's `profile_id`).
    pub profile: &'a Json,
}

/// Repository over `timeline` and `timeline_events`.
#[derive(Debug, Clone)]
pub struct TimelineRepo {
    pool: SqlitePool,
}

impl TimelineRepo {
    /// Builds the repository from the shared single-writer pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Legacy `version`: `MAX(timeline_events.id)` for the profile, `0` if none.
    pub async fn version(&self, profile_id: i64) -> Result<i64, DbError> {
        let version: Option<i64> =
            sqlx::query_scalar("SELECT MAX(id) FROM timeline_events WHERE profile_id = ?")
                .bind(profile_id)
                .fetch_one(&self.pool)
                .await
                .map_err(DbError::query)?;
        Ok(version.unwrap_or(0))
    }

    /// All timeline rows of the profile in legacy dump order.
    pub async fn dump(&self, profile_id: i64) -> Result<Vec<TimelineEntry>, DbError> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT hash, data FROM timeline WHERE profile_id = ? ORDER BY updated_at ASC",
        )
        .bind(profile_id)
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(rows
            .into_iter()
            .map(|(hash, data)| TimelineEntry { hash, data })
            .collect())
    }

    /// Events after `since` in ascending id order (never limited).
    pub async fn changelog(
        &self,
        profile_id: i64,
        since: i64,
    ) -> Result<Vec<TimelineEntry>, DbError> {
        let rows = sqlx::query(
            "SELECT id, hash, data FROM timeline_events \
             WHERE profile_id = ? AND id > ? ORDER BY id ASC",
        )
        .bind(profile_id)
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::query)?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(TimelineEntry {
                hash: row.try_get("hash").map_err(DbError::query)?,
                data: row.try_get("data").map_err(DbError::query)?,
            });
        }
        Ok(entries)
    }

    /// Legacy `POST /api/timeline/update`: upsert + append one event.
    ///
    /// The two statements are deliberately not wrapped in a transaction: the
    /// legacy route runs them sequentially and a failed event insert leaves the
    /// upsert in place (`sync` is the only transactional legacy mutation).
    pub async fn update(
        &self,
        profile_id: i64,
        progress: &TimelineProgress<'_>,
    ) -> Result<(), DbError> {
        let now = now_millis();
        let data = Json::Object(vec![
            ("hash".to_owned(), Json::String(progress.hash.to_owned())),
            ("percent".to_owned(), progress.percent.clone()),
            ("time".to_owned(), progress.time.clone()),
            ("duration".to_owned(), progress.duration.clone()),
            ("profile".to_owned(), progress.profile.clone()),
        ])
        .to_compact_string();

        sqlx::query(
            "INSERT INTO timeline(profile_id, hash, data, updated_at) VALUES(?, ?, ?, ?) \
             ON CONFLICT(profile_id, hash) DO UPDATE SET data = excluded.data, \
             updated_at = excluded.updated_at",
        )
        .bind(profile_id)
        .bind(progress.hash)
        .bind(&data)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(DbError::query)?;

        sqlx::query(
            "INSERT INTO timeline_events(profile_id, hash, data, updated_at) VALUES(?, ?, ?, ?)",
        )
        .bind(profile_id)
        .bind(progress.hash)
        .bind(&data)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(())
    }
}

/// Milliseconds since the Unix epoch (legacy `Date.now()` equivalent).
pub(crate) fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(_) => 0,
    }
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
    async fn update_upserts_and_appends_events_with_legacy_json_order() {
        let (_temp, database) = fixture().await;
        let repo = TimelineRepo::new(database.pool().clone());
        let percent = Json::Number("80".to_owned());
        let time = Json::Number("4800".to_owned());
        let duration = Json::Number("6000".to_owned());
        let profile = Json::Number("1".to_owned());

        repo.update(
            1,
            &TimelineProgress {
                hash: "golden-hash-000a",
                percent: &percent,
                time: &time,
                duration: &duration,
                profile: &profile,
            },
        )
        .await
        .expect("update");

        let stored = sqlx::query_scalar::<_, String>(
            "SELECT data FROM timeline WHERE profile_id = 1 AND hash = 'golden-hash-000a'",
        )
        .fetch_one(database.pool())
        .await
        .expect("stored data");
        assert_eq!(
            stored,
            r#"{"hash":"golden-hash-000a","percent":80,"time":4800,"duration":6000,"profile":1}"#
        );

        let dump = repo.dump(1).await.expect("dump");
        assert_eq!(dump.len(), 1);
        assert_eq!(dump[0].data, stored);
        assert_eq!(repo.version(1).await.expect("version"), 1);

        // The second update for the same hash replaces the row and appends a
        // second event (no duplicate key in the dump).
        let percent_second = Json::Number("90".to_owned());
        repo.update(
            1,
            &TimelineProgress {
                hash: "golden-hash-000a",
                percent: &percent_second,
                time: &time,
                duration: &duration,
                profile: &profile,
            },
        )
        .await
        .expect("second update");
        assert_eq!(repo.dump(1).await.expect("dump").len(), 1);
        assert_eq!(repo.version(1).await.expect("version"), 2);
        assert_eq!(repo.changelog(1, 1).await.expect("since").len(), 1);
        assert_eq!(repo.changelog(2, 0).await.expect("other profile").len(), 0);
        database.close().await;
    }

    #[tokio::test]
    async fn changelog_is_not_limited() {
        let (_temp, database) = fixture().await;
        let repo = TimelineRepo::new(database.pool().clone());
        for index in 0..250_i64 {
            sqlx::query(
                "INSERT INTO timeline_events(profile_id, hash, data, updated_at) \
                 VALUES(1, ?, '{}', ?)",
            )
            .bind(format!("bulk-{index:03}"))
            .bind(index)
            .execute(database.pool())
            .await
            .expect("seed event");
            sqlx::query(
                "INSERT INTO timeline(profile_id, hash, data, updated_at) VALUES(1, ?, '{}', ?)",
            )
            .bind(format!("bulk-{index:03}"))
            .bind(index)
            .execute(database.pool())
            .await
            .expect("seed row");
        }
        assert_eq!(repo.changelog(1, 0).await.expect("changelog").len(), 250);
        assert_eq!(repo.dump(1).await.expect("dump").len(), 250);
        assert_eq!(repo.version(1).await.expect("version"), 250);
        database.close().await;
    }
}
