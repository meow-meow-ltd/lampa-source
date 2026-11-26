//! `profile_settings` repository (composite `(profile_id, key)` primary key).
//!
//! Mirrors `backend/routes/profile_settings.js`:
//!
//! * the primary key is the composite `(profile_id, key)` — the same key may
//!   exist for many profiles and each profile has its own value;
//! * values are opaque strings in the database; the legacy HTTP layer
//!   stringifies non-string JSON values before writing (T09 handler);
//! * every write sets `updated_at` (`Date.now()` milliseconds); a `null` value
//!   in the legacy POST **deletes** the row instead of storing `"null"`;
//! * there are no foreign keys in schema v7, so settings rows survive a
//!   profile delete as orphans — that is the legacy contract (plan §3.5).

use sqlx::sqlite::SqlitePool;

use crate::db::DbError;

use super::now_millis;

/// One `profile_settings` row (the profile id is selected by the query).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSetting {
    /// Setting key (unique per profile).
    pub key: String,
    /// Stored value; `None` for legacy rows written with SQL `NULL`.
    pub value: Option<String>,
}

/// Repository over the `profile_settings` table.
#[derive(Debug, Clone)]
pub struct ProfileSettingsRepo {
    pool: SqlitePool,
}

impl ProfileSettingsRepo {
    /// Builds the repository from the shared single-writer pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// All settings of one profile, in key order (deterministic output).
    pub async fn list(&self, profile_id: i64) -> Result<Vec<ProfileSetting>, DbError> {
        let rows = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT key, value FROM profile_settings WHERE profile_id = ? ORDER BY key ASC",
        )
        .bind(profile_id)
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(rows
            .into_iter()
            .map(|(key, value)| ProfileSetting { key, value })
            .collect())
    }

    /// One setting by composite key; `None` when the row does not exist.
    pub async fn get(&self, profile_id: i64, key: &str) -> Result<Option<ProfileSetting>, DbError> {
        let row = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT key, value FROM profile_settings WHERE profile_id = ? AND key = ?",
        )
        .bind(profile_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(row.map(|(key, value)| ProfileSetting { key, value }))
    }

    /// Inserts or updates one setting, bumping `updated_at`.
    ///
    /// The SQL mirrors the legacy upsert verbatim:
    /// `ON CONFLICT(profile_id, key) DO UPDATE SET value=excluded.value,
    /// updated_at=excluded.updated_at`.
    pub async fn upsert(&self, profile_id: i64, key: &str, value: &str) -> Result<(), DbError> {
        if key.is_empty() {
            return Err(DbError::validation("profile setting key must not be empty"));
        }
        sqlx::query(
            "INSERT INTO profile_settings(profile_id, key, value, updated_at) VALUES(?, ?, ?, ?) \
             ON CONFLICT(profile_id, key) DO UPDATE SET value = excluded.value, \
             updated_at = excluded.updated_at",
        )
        .bind(profile_id)
        .bind(key)
        .bind(value)
        .bind(now_millis())
        .execute(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(())
    }

    /// Deletes one setting; returns whether a row was removed.
    pub async fn delete(&self, profile_id: i64, key: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM profile_settings WHERE profile_id = ? AND key = ?")
            .bind(profile_id)
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(DbError::query)?;
        Ok(result.rows_affected() > 0)
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
    async fn composite_key_upsert_reads_back_per_profile() {
        let (_temp, database) = fixture().await;
        let repo = database.pool();
        let repo = ProfileSettingsRepo::new(repo.clone());
        database.profiles().list().await.expect("Main");

        repo.upsert(1, "cinema_mode", "golden")
            .await
            .expect("insert");
        repo.upsert(1, "watch_progress", "1234")
            .await
            .expect("insert");
        repo.upsert(2, "cinema_mode", "off").await.expect("insert");

        let first = repo.list(1).await.expect("list profile 1");
        assert_eq!(
            first
                .iter()
                .map(|setting| (setting.key.as_str(), setting.value.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("cinema_mode", Some("golden")),
                ("watch_progress", Some("1234")),
            ]
        );
        let second = repo.list(2).await.expect("list profile 2");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].value.as_deref(), Some("off"));

        // Same key, different profile: the composite key isolates both rows.
        assert_eq!(
            repo.get(1, "cinema_mode")
                .await
                .expect("get")
                .expect("row")
                .value
                .as_deref(),
            Some("golden")
        );
        assert_eq!(
            repo.get(2, "cinema_mode")
                .await
                .expect("get")
                .expect("row")
                .value
                .as_deref(),
            Some("off")
        );
        assert!(repo.get(3, "cinema_mode").await.expect("get").is_none());
        database.close().await;
    }

    #[tokio::test]
    async fn upsert_updates_the_value_and_timestamp() {
        let (_temp, database) = fixture().await;
        let repo = ProfileSettingsRepo::new(database.pool().clone());
        database.profiles().list().await.expect("Main");

        repo.upsert(1, "key", "one").await.expect("insert");
        let created = repo.get(1, "key").await.expect("get").expect("row");
        sqlx::query(
            "UPDATE profile_settings SET updated_at = 1 WHERE profile_id = 1 AND key = 'key'",
        )
        .execute(database.pool())
        .await
        .expect("pin timestamp");

        repo.upsert(1, "key", "two").await.expect("update");
        let updated = repo.get(1, "key").await.expect("get").expect("row");
        assert_eq!(updated.value.as_deref(), Some("two"));
        let updated_at: i64 =
            sqlx::query_scalar("SELECT updated_at FROM profile_settings WHERE profile_id = 1")
                .fetch_one(database.pool())
                .await
                .expect("updated_at");
        assert!(updated_at > 1, "upsert must bump updated_at");
        assert_eq!(created.key, updated.key);
        database.close().await;
    }

    #[tokio::test]
    async fn delete_is_scoped_to_the_composite_key() {
        let (_temp, database) = fixture().await;
        let repo = ProfileSettingsRepo::new(database.pool().clone());
        database.profiles().list().await.expect("Main");

        repo.upsert(1, "a", "1").await.expect("insert");
        repo.upsert(1, "b", "2").await.expect("insert");
        repo.upsert(2, "a", "3").await.expect("insert");

        assert!(repo.delete(1, "a").await.expect("delete"));
        assert!(!repo.delete(1, "a").await.expect("delete again"));
        assert!(repo.get(1, "a").await.expect("get").is_none());
        assert!(repo.get(1, "b").await.expect("get").is_some());
        assert!(repo.get(2, "a").await.expect("get").is_some());
        database.close().await;
    }

    #[tokio::test]
    async fn settings_survive_profile_deletion_as_orphans() {
        let (_temp, database) = fixture().await;
        let repo = ProfileSettingsRepo::new(database.pool().clone());
        let profiles = database.profiles();
        profiles.list().await.expect("Main");
        let kids = profiles.create("Kids", None, None).await.expect("create");

        repo.upsert(kids.id, "menu", "1").await.expect("setting");
        assert!(profiles.delete(kids.id).await.expect("delete profile"));

        // No FK constraints: the row remains, still readable by profile id.
        let orphan = repo.get(kids.id, "menu").await.expect("get orphan");
        assert_eq!(
            orphan.expect("orphaned setting survives").value.as_deref(),
            Some("1")
        );
        let fk_errors: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_foreign_key_check")
            .fetch_one(database.pool())
            .await
            .expect("foreign_key_check");
        assert_eq!(fk_errors, 0);
        database.close().await;
    }
}
