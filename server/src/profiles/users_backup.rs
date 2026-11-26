//! User backup dataset stored in `settings.backup_latest` (legacy quirk).
//!
//! `backend/routes/backup.js` keeps the client's last backup as a single JSON
//! text blob in the `settings` table under the key `backup_latest`. The route
//! names are inverted in the legacy API and that inversion is part of the
//! contract (T03 fixtures 24/25):
//!
//! * `POST /api/users/backup/export` — **writes** the blob (validated JSON
//!   text, stored verbatim);
//! * `GET /api/users/backup/import` — **reads** it back, parsed.
//!
//! The repository only owns the storage half: [`UsersBackupRepo::save`] keeps
//! the client's raw JSON text byte-for-byte (so a later read returns exactly
//! what the client sent) and forces the legacy `visible = 0` flag on both
//! insert and conflict, like the Node upsert.

use sqlx::sqlite::SqlitePool;

use crate::db::DbError;

/// `settings` key holding the latest user backup blob.
pub const BACKUP_SETTING_KEY: &str = "backup_latest";

/// Repository for the single `settings.backup_latest` backup row.
#[derive(Debug, Clone)]
pub struct UsersBackupRepo {
    pool: SqlitePool,
}

impl UsersBackupRepo {
    /// Builds the repository from the shared single-writer pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Legacy `POST /api/users/backup/export` write side.
    ///
    /// Stores `raw` verbatim and resets `visible` to `0` (legacy upsert).
    pub async fn save(&self, raw: &str) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO settings(key, value, visible) VALUES(?, ?, 0) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, visible = 0",
        )
        .bind(BACKUP_SETTING_KEY)
        .bind(raw)
        .execute(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(())
    }

    /// Legacy `GET /api/users/backup/import` read side.
    ///
    /// `None` means no backup row (or a legacy row with SQL `NULL`); the
    /// handler maps that to `{"secuses":true,"data":null}`. Empty strings are
    /// treated as absent by the handler (legacy `if (!row?.value)`).
    pub async fn load(&self) -> Result<Option<String>, DbError> {
        let value =
            sqlx::query_scalar::<_, Option<String>>("SELECT value FROM settings WHERE key = ?")
                .bind(BACKUP_SETTING_KEY)
                .fetch_optional(&self.pool)
                .await
                .map_err(DbError::query)?;
        Ok(value.flatten())
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
    async fn save_and_load_round_trip_verbatim() {
        let (_temp, database) = fixture().await;
        let repo = UsersBackupRepo::new(database.pool().clone());
        assert!(repo.load().await.expect("empty").is_none());

        let raw = r#"{"golden":true,"items":[1,2,3],"note":"synthetic backup"}"#;
        repo.save(raw).await.expect("save");

        assert_eq!(repo.load().await.expect("load").as_deref(), Some(raw));
        let visible: i64 =
            sqlx::query_scalar("SELECT visible FROM settings WHERE key = 'backup_latest'")
                .fetch_one(database.pool())
                .await
                .expect("visible");
        assert_eq!(visible, 0, "legacy upsert forces visible = 0");

        // Overwrite: the newer blob wins and the flag stays 0.
        let newer = r#"{"golden":false}"#;
        repo.save(newer).await.expect("overwrite");
        assert_eq!(repo.load().await.expect("load").as_deref(), Some(newer));
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM settings WHERE key = 'backup_latest'")
                .fetch_one(database.pool())
                .await
                .expect("count");
        assert_eq!(count, 1, "exactly one backup row (upsert, not append)");
        database.close().await;
    }

    #[tokio::test]
    async fn a_visible_backup_row_is_reset_to_invisible() {
        let (_temp, database) = fixture().await;
        sqlx::query(
            "INSERT INTO settings(key, value, visible) VALUES('backup_latest', '{\"old\":true}', 1)",
        )
        .execute(database.pool())
        .await
        .expect("seed visible row");

        let repo = UsersBackupRepo::new(database.pool().clone());
        repo.save(r#"{"new":true}"#).await.expect("save");
        let (value, visible): (String, i64) =
            sqlx::query_as("SELECT value, visible FROM settings WHERE key = 'backup_latest'")
                .fetch_one(database.pool())
                .await
                .expect("row");
        assert_eq!(value, r#"{"new":true}"#);
        assert_eq!(visible, 0, "legacy upsert sets visible = 0 explicitly");
        database.close().await;
    }

    #[tokio::test]
    async fn a_null_row_reads_as_absent() {
        let (_temp, database) = fixture().await;
        sqlx::query("INSERT INTO settings(key, value, visible) VALUES('backup_latest', NULL, 0)")
            .execute(database.pool())
            .await
            .expect("seed null row");
        let repo = UsersBackupRepo::new(database.pool().clone());
        assert!(repo.load().await.expect("load").is_none());
        database.close().await;
    }
}
