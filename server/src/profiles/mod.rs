//! `profiles` table repository (legacy semantics, schema v7).
//!
//! Mirrors `backend/routes/profiles.js`:
//! * `GET all` creates the `Main` profile when the table is empty and then lists
//!   rows ordered by `is_main DESC, sort_order ASC, id ASC`;
//! * `create` validates a non-empty name, trims it, defaults `icon` to `l_1`,
//!   appends after the current maximum `sort_order` and never creates a main profile;
//! * `update` changes only the provided fields (legacy ignores empty strings) and
//!   always refreshes `updated_at`;
//! * `delete` is a plain `DELETE FROM profiles WHERE id = ?` — **no cascade**.
//!   The legacy schema has no FK constraints (`foreign_keys=OFF`), so
//!   `bookmarks`/`profile_settings`/`timeline*` rows intentionally become orphans.

// The repository API is the T04 deliverable consumed by the T09 HTTP layer
// (`http::api::profiles`) and by the per-profile settings / user-backup
// repositories below; a few diagnostics helpers remain unwired.
#![allow(dead_code)]

pub mod profile_settings;
pub mod users_backup;

use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::sqlite::SqlitePool;

use crate::db::DbError;

/// Default icon used by the legacy profile model.
pub const DEFAULT_ICON: &str = "l_1";
/// Name of the profile created for an empty database.
pub const MAIN_PROFILE_NAME: &str = "Main";

/// One `profiles` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Primary key.
    pub id: i64,
    /// Display name.
    pub name: String,
    /// Icon identifier; `None` only for legacy rows written without an icon.
    pub icon: Option<String>,
    /// Legacy `is_main` flag (exactly one main profile in normal data).
    pub is_main: bool,
    /// Legacy `is_child` flag.
    pub is_child: bool,
    /// Legacy sort order (ascending).
    pub sort_order: i64,
    /// Creation timestamp, milliseconds since the Unix epoch.
    pub created_at: i64,
    /// Last update timestamp, milliseconds since the Unix epoch.
    pub updated_at: i64,
}

impl Profile {
    /// Icon with the legacy `l_1` fallback applied (for API mapping in T09).
    pub fn icon_or_default(&self) -> &str {
        self.icon.as_deref().unwrap_or(DEFAULT_ICON)
    }
}

/// Fields accepted by [`ProfilesRepo::update`]; `None` = leave unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileUpdate {
    /// New display name (empty strings are ignored, like the legacy route).
    pub name: Option<String>,
    /// New icon (empty strings are ignored).
    pub icon: Option<String>,
    /// Legacy `is_child` flag.
    pub is_child: Option<bool>,
    /// Legacy sort order.
    pub sort_order: Option<i64>,
}

impl ProfileUpdate {
    fn is_empty(&self) -> bool {
        let name = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let icon = self
            .icon
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        name.is_none() && icon.is_none() && self.is_child.is_none() && self.sort_order.is_none()
    }
}

/// Repository over the `profiles` table.
#[derive(Debug, Clone)]
pub struct ProfilesRepo {
    pool: SqlitePool,
}

impl ProfilesRepo {
    /// Builds the repository from the shared single-writer pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Legacy `GET /api/profiles/all`: ensures `Main` exists, then lists.
    pub async fn list(&self) -> Result<Vec<Profile>, DbError> {
        self.ensure_main_profile().await?;
        let rows = sqlx::query_as::<_, (i64, String, Option<String>, i64, i64, i64, i64, i64)>(
            "SELECT id, name, icon, is_main, is_child, created_at, updated_at, sort_order \
             FROM profiles ORDER BY is_main DESC, sort_order ASC, id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(rows
            .into_iter()
            .map(
                |(id, name, icon, is_main, is_child, created_at, updated_at, sort_order)| Profile {
                    id,
                    name,
                    icon,
                    is_main: is_main != 0,
                    is_child: is_child != 0,
                    sort_order,
                    created_at,
                    updated_at,
                },
            )
            .collect())
    }

    /// Counts profiles without creating `Main` (diagnostics/tests).
    pub async fn count(&self) -> Result<i64, DbError> {
        sqlx::query_scalar("SELECT count(*) FROM profiles")
            .fetch_one(&self.pool)
            .await
            .map_err(DbError::query)
    }

    /// Creates a profile appended after the current maximum `sort_order`.
    ///
    /// `sort_order` may override the position (legacy `sort` field). The name is
    /// trimmed and must be non-empty; the icon defaults to `l_1`.
    pub async fn create(
        &self,
        name: &str,
        icon: Option<&str>,
        sort_order: Option<i64>,
    ) -> Result<Profile, DbError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DbError::validation("profile name must not be empty"));
        }
        let icon = icon
            .map(str::trim)
            .filter(|icon| !icon.is_empty())
            .unwrap_or(DEFAULT_ICON)
            .to_owned();
        let sort_order = match sort_order {
            Some(sort_order) => sort_order,
            None => {
                let max: Option<i64> = sqlx::query_scalar("SELECT max(sort_order) FROM profiles")
                    .fetch_one(&self.pool)
                    .await
                    .map_err(DbError::query)?;
                max.unwrap_or(0).saturating_add(1)
            }
        };
        let now = now_millis();

        let result = sqlx::query(
            "INSERT INTO profiles(name, icon, is_main, is_child, sort_order, created_at, updated_at) \
             VALUES(?, ?, 0, 0, ?, ?, ?)",
        )
        .bind(name)
        .bind(&icon)
        .bind(sort_order)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(DbError::query)?;

        Ok(Profile {
            id: result.last_insert_rowid(),
            name: name.to_owned(),
            icon: Some(icon),
            is_main: false,
            is_child: false,
            sort_order,
            created_at: now,
            updated_at: now,
        })
    }

    /// Legacy `POST /api/profiles/update`: patches only non-empty fields.
    ///
    /// Returns `false` when no row has `id`; an empty patch is
    /// [`DbErrorKind::NothingToUpdate`].
    pub async fn update(&self, id: i64, patch: ProfileUpdate) -> Result<bool, DbError> {
        if patch.is_empty() {
            return Err(DbError::nothing_to_update());
        }

        // Static statement with per-field "was provided?" flags: no dynamic SQL
        // string building and no injection surface, while still patching only the
        // fields the caller supplied (legacy `POST /api/profiles/update`).
        let name = patch
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let icon = patch
            .icon
            .as_deref()
            .map(str::trim)
            .filter(|icon| !icon.is_empty())
            .map(str::to_owned);
        let is_child = patch.is_child.map(i64::from);
        let sort_order = patch.sort_order;

        let result = sqlx::query(
            "UPDATE profiles SET \
             name = CASE WHEN ? THEN ? ELSE name END, \
             icon = CASE WHEN ? THEN ? ELSE icon END, \
             is_child = CASE WHEN ? THEN ? ELSE is_child END, \
             sort_order = CASE WHEN ? THEN ? ELSE sort_order END, \
             updated_at = ? \
             WHERE id = ?",
        )
        .bind(name.is_some())
        .bind(name)
        .bind(icon.is_some())
        .bind(icon)
        .bind(is_child.is_some())
        .bind(is_child)
        .bind(sort_order.is_some())
        .bind(sort_order)
        .bind(now_millis())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(result.rows_affected() > 0)
    }

    /// Legacy `POST /api/profiles/delete`: plain `DELETE`, no cascade.
    pub async fn delete(&self, id: i64) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM profiles WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(DbError::query)?;
        Ok(result.rows_affected() > 0)
    }

    /// Creates `Main` (`sort_order = 1`) if the table is empty.
    ///
    /// Runs in a transaction so two concurrent first reads cannot insert two
    /// main profiles; the single-writer pool serialises the transaction.
    async fn ensure_main_profile(&self) -> Result<(), DbError> {
        let mut transaction = self.pool.begin().await.map_err(DbError::query)?;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles")
            .fetch_one(&mut *transaction)
            .await
            .map_err(DbError::query)?;
        if count == 0 {
            let now = now_millis();
            sqlx::query(
                "INSERT INTO profiles(name, icon, is_main, is_child, sort_order, created_at, updated_at) \
                 VALUES(?, ?, 1, 0, 1, ?, ?)",
            )
            .bind(MAIN_PROFILE_NAME)
            .bind(DEFAULT_ICON)
            .bind(now)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(DbError::query)?;
        }
        transaction.commit().await.map_err(DbError::query)?;
        Ok(())
    }
}

/// Milliseconds since the Unix epoch (legacy `Date.now()` equivalent).
fn now_millis() -> i64 {
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
    async fn listing_an_empty_table_creates_main_once() {
        let (_temp, database) = fixture().await;
        let repo = database.profiles();
        assert_eq!(repo.count().await.expect("count"), 0);

        let profiles = repo.list().await.expect("list creates Main");
        assert_eq!(profiles.len(), 1);
        let main = &profiles[0];
        assert_eq!(main.name, MAIN_PROFILE_NAME);
        assert_eq!(main.icon.as_deref(), Some(DEFAULT_ICON));
        assert!(main.is_main);
        assert!(!main.is_child);
        assert_eq!(main.sort_order, 1);

        let again = repo.list().await.expect("second list");
        assert_eq!(again.len(), 1, "Main must not be duplicated");
        assert_eq!(again[0].id, main.id);
        database.close().await;
    }

    #[tokio::test]
    async fn create_appends_after_the_maximum_sort_order() {
        let (_temp, database) = fixture().await;
        let repo = database.profiles();
        repo.list().await.expect("create Main");

        let kids = repo
            .create("  Kids  ", None, None)
            .await
            .expect("create Kids");
        assert_eq!(kids.name, "Kids", "name is trimmed");
        assert_eq!(kids.icon.as_deref(), Some(DEFAULT_ICON));
        assert!(!kids.is_main);
        assert!(!kids.is_child);
        assert_eq!(kids.sort_order, 2);
        assert!(kids.created_at > 0);
        assert_eq!(kids.created_at, kids.updated_at);

        let custom = repo
            .create("Custom", Some("icon_x"), Some(10))
            .await
            .expect("create with explicit sort");
        assert_eq!(custom.sort_order, 10);
        assert_eq!(custom.icon.as_deref(), Some("icon_x"));

        let listed = repo.list().await.expect("list");
        assert_eq!(
            listed
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Main", "Kids", "Custom"],
            "is_main DESC, sort_order ASC, id ASC"
        );
        database.close().await;
    }

    #[tokio::test]
    async fn create_rejects_an_empty_name() {
        let (_temp, database) = fixture().await;
        let repo = database.profiles();
        let error = repo
            .create("   ", None, None)
            .await
            .expect_err("empty name");
        assert_eq!(error.code(), "invalid_input");
        assert!(!error.to_string().contains("INSERT"));
        assert_eq!(repo.count().await.expect("count"), 0);
        database.close().await;
    }

    #[tokio::test]
    async fn update_changes_only_provided_fields() {
        let (_temp, database) = fixture().await;
        let repo = database.profiles();
        let main = repo.list().await.expect("Main")[0].clone();

        assert!(repo
            .update(
                main.id,
                ProfileUpdate {
                    name: Some("Home".to_owned()),
                    is_child: Some(true),
                    sort_order: Some(7),
                    icon: Some("icon_home".to_owned()),
                },
            )
            .await
            .expect("update"));

        let updated = &repo.list().await.expect("list")[0];
        assert_eq!(updated.name, "Home");
        assert_eq!(updated.icon.as_deref(), Some("icon_home"));
        assert!(updated.is_child);
        assert_eq!(updated.sort_order, 7);
        assert_eq!(updated.created_at, main.created_at);
        assert!(updated.updated_at >= main.updated_at);

        // Empty strings are ignored by the legacy route; a fully empty patch is a 400.
        let error = repo
            .update(
                main.id,
                ProfileUpdate {
                    name: Some(String::new()),
                    icon: Some("  ".to_owned()),
                    ..ProfileUpdate::default()
                },
            )
            .await
            .expect_err("empty patch");
        assert_eq!(error.code(), "no_fields_to_update");

        let missing = repo
            .update(
                9_999,
                ProfileUpdate {
                    name: Some("Ghost".to_owned()),
                    ..ProfileUpdate::default()
                },
            )
            .await
            .expect("update missing id");
        assert!(!missing, "no row updated");
        database.close().await;
    }

    #[tokio::test]
    async fn delete_is_plain_delete_without_cascade() {
        let (_temp, database) = fixture().await;
        let repo = database.profiles();
        repo.list().await.expect("create Main");
        let kids = repo.create("Kids", None, None).await.expect("create Kids");

        // Legacy data that references the profile; no FK constraints exist.
        sqlx::query(
            "INSERT INTO bookmarks(profile_id, type, card_id, data, updated_at) \
             VALUES(?, 'book', 42, '{}', 1)",
        )
        .bind(kids.id)
        .execute(database.pool())
        .await
        .expect("bookmark orphan");
        sqlx::query(
            "INSERT INTO profile_settings(profile_id, key, value, updated_at) \
             VALUES(?, 'menu', '1', 1)",
        )
        .bind(kids.id)
        .execute(database.pool())
        .await
        .expect("profile setting orphan");
        sqlx::query(
            "INSERT INTO timeline_events(profile_id, hash, data, updated_at) \
             VALUES(?, 'hash', '{}', 1)",
        )
        .bind(kids.id)
        .execute(database.pool())
        .await
        .expect("timeline event orphan");

        assert!(repo.delete(kids.id).await.expect("delete"));
        assert!(!repo.delete(kids.id).await.expect("delete again"));
        let remaining = repo.list().await.expect("list after delete");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, MAIN_PROFILE_NAME);

        for (table, sql) in [
            (
                "bookmarks",
                "SELECT count(*) FROM bookmarks WHERE profile_id = ?",
            ),
            (
                "profile_settings",
                "SELECT count(*) FROM profile_settings WHERE profile_id = ?",
            ),
            (
                "timeline_events",
                "SELECT count(*) FROM timeline_events WHERE profile_id = ?",
            ),
        ] {
            let count: i64 = sqlx::query_scalar(sql)
                .bind(kids.id)
                .fetch_one(database.pool())
                .await
                .expect("count orphans");
            assert_eq!(count, 1, "{table} rows must survive profile deletion");
        }

        let foreign_key_errors: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pragma_foreign_key_check")
                .fetch_one(database.pool())
                .await
                .expect("foreign_key_check");
        assert_eq!(foreign_key_errors, 0, "schema has no FK constraints at all");
        database.close().await;
    }

    #[test]
    fn update_empty_detection_matches_legacy_truthiness() {
        assert!(ProfileUpdate::default().is_empty());
        assert!(ProfileUpdate {
            name: Some("  ".to_owned()),
            ..ProfileUpdate::default()
        }
        .is_empty());
        assert!(!ProfileUpdate {
            is_child: Some(false),
            ..ProfileUpdate::default()
        }
        .is_empty());
        assert!(!ProfileUpdate {
            sort_order: Some(0),
            ..ProfileUpdate::default()
        }
        .is_empty());
    }
}
