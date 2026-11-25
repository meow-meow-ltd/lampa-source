//! `settings` table repository (legacy key/value store, schema v7).
//!
//! Legacy semantics (`backend/routes/settings.js`, `backend/settings_keys.js`):
//! * `GET /api/settings` → all rows `{key, value, visible}`;
//! * `PUT /api/settings` → update by key (T04 adds upsert, per the W0 contract);
//! * `DELETE /api/settings/<key>` → delete by key.
//!
//! Security (plan §3.6 п.6, R8): `admin_password`/`admin_token` are never returned
//! in clear text. [`SettingsRepo::list`] and [`SettingsRepo::get`] mask them as
//! `"***"` (the T07 HTTP layer relies on this); only [`SettingsRepo::get_raw`]
//! exposes the stored value for trusted server-side consumers (auth checks).
//! A `PUT`/upsert with the mask value for a secret key is a no-op, so a client
//! round-trip of a masked GET cannot destroy the secret.

// The repository API is the T04 deliverable consumed by the T07/T09 HTTP
// layers (not wired yet); unit tests exercise it directly.
#![allow(dead_code)]

use sqlx::sqlite::SqlitePool;

use crate::db::DbError;

/// Placeholder returned instead of secret values.
pub const MASKED_VALUE: &str = "***";
/// Keys whose values must never be serialised to a client or a log line.
pub const SECRET_SETTING_KEYS: [&str; 2] = ["admin_password", "admin_token"];

/// One `settings` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// Primary key (`settings.key`).
    pub key: String,
    /// Stored value; masked in [`SettingsRepo::list`]/[`SettingsRepo::get`].
    pub value: Option<String>,
    /// Legacy `visible` flag (0/1); visible settings are readable via `/api/value`.
    pub visible: bool,
}

impl Setting {
    /// Whether this row's value is a secret that must stay masked.
    pub fn is_secret(&self) -> bool {
        is_secret_key(&self.key)
    }
}

/// Whether `key` is one of the masked legacy secret keys.
pub fn is_secret_key(key: &str) -> bool {
    SECRET_SETTING_KEYS.contains(&key)
}

/// Repository over the `settings` table.
#[derive(Debug, Clone)]
pub struct SettingsRepo {
    pool: SqlitePool,
}

impl SettingsRepo {
    /// Builds the repository from the shared single-writer pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// All settings in legacy table order, with secret values masked.
    pub async fn list(&self) -> Result<Vec<Setting>, DbError> {
        let rows = sqlx::query_as::<_, (String, Option<String>, bool)>(
            "SELECT key, value, visible FROM settings",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(rows
            .into_iter()
            .map(|(key, value, visible)| Setting {
                value: mask_value(&key, value),
                key,
                visible,
            })
            .collect())
    }

    /// One setting by key, with the secret value masked.
    pub async fn get(&self, key: &str) -> Result<Option<Setting>, DbError> {
        Ok(self.get_raw(key).await?.map(|mut setting| {
            setting.value = mask_value(&setting.key, setting.value);
            setting
        }))
    }

    /// One setting by key, with the real stored value.
    ///
    /// Trusted server-side callers only (auth, admin checks); never serialise the
    /// result of this method directly to a client.
    pub async fn get_raw(&self, key: &str) -> Result<Option<Setting>, DbError> {
        let row = sqlx::query_as::<_, (String, Option<String>, bool)>(
            "SELECT key, value, visible FROM settings WHERE key = ?",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(row.map(|(key, value, visible)| Setting {
            key,
            value,
            visible,
        }))
    }

    /// Inserts a new setting or updates the value of an existing one.
    ///
    /// * `visible` is not modified for existing rows (new rows get the legacy
    ///   default `0`, matching `backend/routes/backup.js`);
    /// * for `admin_password`/`admin_token`, the masked value `"***"` is ignored
    ///   (echoing a masked GET back must not overwrite the secret);
    /// * empty keys are rejected as invalid input.
    pub async fn upsert(&self, key: &str, value: &str) -> Result<(), DbError> {
        if key.trim().is_empty() {
            return Err(DbError::validation("settings key must not be empty"));
        }
        if is_secret_key(key) && value == MASKED_VALUE {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO settings(key, value, visible) VALUES(?, ?, 0) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(())
    }

    /// Deletes a setting by key; returns whether a row was removed.
    pub async fn delete(&self, key: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(DbError::query)?;
        Ok(result.rows_affected() > 0)
    }
}

fn mask_value(key: &str, value: Option<String>) -> Option<String> {
    if is_secret_key(key) {
        Some(MASKED_VALUE.to_owned())
    } else {
        value
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
    async fn upsert_get_list_and_delete_round_trip() {
        let (_temp, database) = fixture().await;
        let repo = database.settings();

        assert!(repo.list().await.expect("empty list").is_empty());
        assert!(repo
            .get("head_search")
            .await
            .expect("get missing")
            .is_none());

        repo.upsert("head_search", "1")
            .await
            .expect("insert setting");
        let setting = repo
            .get("head_search")
            .await
            .expect("get setting")
            .expect("setting exists");
        assert_eq!(setting.key, "head_search");
        assert_eq!(setting.value.as_deref(), Some("1"));
        assert!(!setting.visible, "new settings default to visible=0");

        repo.upsert("head_search", "0")
            .await
            .expect("update setting");
        assert_eq!(
            repo.get_raw("head_search")
                .await
                .expect("raw get")
                .expect("exists")
                .value
                .as_deref(),
            Some("0")
        );

        assert_eq!(repo.list().await.expect("list").len(), 1);
        assert!(repo.delete("head_search").await.expect("delete"));
        assert!(!repo.delete("head_search").await.expect("delete again"));
        assert!(repo.list().await.expect("list after delete").is_empty());
        database.close().await;
    }

    #[tokio::test]
    async fn secrets_are_masked_and_mask_upsert_is_a_no_op() {
        let (_temp, database) = fixture().await;
        let repo = database.settings();

        repo.upsert("admin_password", "hunter2-plaintext")
            .await
            .expect("store secret");
        repo.upsert("admin_token", "token-plaintext")
            .await
            .expect("store token");

        let listed = repo.list().await.expect("list");
        for setting in &listed {
            assert!(setting.is_secret());
            assert_eq!(setting.value.as_deref(), Some(MASKED_VALUE));
        }
        assert_eq!(
            repo.get("admin_password")
                .await
                .expect("masked get")
                .expect("exists")
                .value
                .as_deref(),
            Some(MASKED_VALUE)
        );
        assert_eq!(
            repo.get_raw("admin_password")
                .await
                .expect("raw get")
                .expect("exists")
                .value
                .as_deref(),
            Some("hunter2-plaintext")
        );

        // Echoing the mask back (as a client would after GET) must not overwrite.
        repo.upsert("admin_password", MASKED_VALUE)
            .await
            .expect("masked upsert");
        repo.upsert("admin_token", MASKED_VALUE)
            .await
            .expect("masked upsert");
        assert_eq!(
            repo.get_raw("admin_password")
                .await
                .expect("raw get")
                .expect("exists")
                .value
                .as_deref(),
            Some("hunter2-plaintext")
        );
        assert_eq!(
            repo.get_raw("admin_token")
                .await
                .expect("raw get")
                .expect("exists")
                .value
                .as_deref(),
            Some("token-plaintext")
        );

        // Non-secret values are never masked, including a literal "***".
        repo.upsert("menu_data", MASKED_VALUE)
            .await
            .expect("literal");
        assert_eq!(
            repo.get("menu_data")
                .await
                .expect("get")
                .expect("exists")
                .value
                .as_deref(),
            Some(MASKED_VALUE)
        );
        database.close().await;
    }

    #[tokio::test]
    async fn upsert_preserves_the_legacy_visible_flag() {
        let (_temp, database) = fixture().await;
        let repo = database.settings();

        sqlx::query("INSERT INTO settings(key, value, visible) VALUES('menu_server', 'old', 1)")
            .execute(database.pool())
            .await
            .expect("seed visible setting");

        repo.upsert("menu_server", "new").await.expect("update");
        let setting = repo
            .get_raw("menu_server")
            .await
            .expect("raw get")
            .expect("exists");
        assert_eq!(setting.value.as_deref(), Some("new"));
        assert!(setting.visible, "upsert must not reset visible");
        database.close().await;
    }

    #[tokio::test]
    async fn empty_key_is_rejected_without_sql_leak() {
        let (_temp, database) = fixture().await;
        let repo = database.settings();
        let error = repo.upsert("   ", "x").await.expect_err("empty key");
        assert_eq!(error.code(), "invalid_input");
        assert!(!error.to_string().contains("INSERT"));
        assert_eq!(repo.list().await.expect("list").len(), 0);
        database.close().await;
    }

    #[test]
    fn secret_keys_are_exactly_the_legacy_ones() {
        assert!(is_secret_key("admin_password"));
        assert!(is_secret_key("admin_token"));
        assert!(!is_secret_key("menu_server"));
        assert!(!is_secret_key(""));
    }
}
