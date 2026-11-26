//! `bookmarks` / `bookmark_events` repository (legacy semantics, schema v7).
//!
//! Mirrors `backend/routes/bookmarks.js` (T03 fixtures 15–20):
//! * `dump` → `version = MAX(bookmark_events.id)` for the profile and all
//!   bookmark rows ordered by `updated_at ASC, id ASC`;
//! * `changelog` → events with `id > since` ordered by `id ASC`; the HTTP layer
//!   maps them to the legacy `{action, …}` shape (`add` renames `entity_id` to
//!   `id` and carries the data, `update` carries `updated_at`, `clear`/`remove`
//!   carry only `entity_id`);
//! * `add` → insert a new bookmark (`add` event) or update the existing row
//!   (`update` event) inside the same profile/type/card scope. `data` is an
//!   opaque JSON string stored exactly as sent;
//! * `remove` → delete by row id when the client sends `id`, otherwise resolve
//!   the row by `(profile, type, card_id)`; an `id`-driven delete appends the
//!   event even when no row matched (legacy `deleteById` always does);
//! * `clear` → delete every bookmark of one type and append a `clear` event
//!   whose `entity_id` is the type name (the column is typeless in SQLite);
//! * `sync` → replace every bookmark of the profile in **one transaction**;
//!   each accepted card becomes a bookmark upsert plus an `add` event. Any
//!   failure rolls the whole transaction back (no partial bookmarks/events).
//!
//! No DDL, no PRAGMAs, no SQL text in errors: failures surface as
//! [`DbError`], whose `Display` is a stable generic sentence.

#![allow(dead_code)] // repository API consumed by the HTTP handler and its tests

pub mod json;

use sqlx::sqlite::{Sqlite, SqlitePool, SqliteRow};
use sqlx::{Row, Transaction};

use self::json::Json;
use crate::db::DbError;

/// One `bookmarks` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    /// Primary key (`bookmarks.id`).
    pub id: i64,
    /// Legacy bookmark type (`film`, `cartoon`, `game`, …).
    pub kind: String,
    /// Legacy card id.
    pub card_id: i64,
    /// Opaque JSON text, stored exactly as the client sent it.
    pub data: String,
    /// Milliseconds since the Unix epoch (legacy `Date.now()`).
    pub updated_at: i64,
}

/// Event `entity_id` as stored: integer row id, text type name, or NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityId {
    /// Numeric `bookmarks.id` (add/update/remove events).
    Int(i64),
    /// Text value (the bookmark type for `clear` events).
    Text(String),
    /// SQL NULL.
    Null,
}

/// One `bookmark_events` row mapped to the fields the changelog exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkEvent {
    /// Stored action (`add`/`update`/`clear`/`remove`; unknown values map to
    /// `remove` in the HTTP layer, like the legacy route).
    pub action: String,
    /// Type-dependent identifier.
    pub entity_id: EntityId,
    /// Bookmark type (only `add`/`update` events carry it).
    pub kind: Option<String>,
    /// Card id (only `add`/`update` events carry it).
    pub card_id: Option<i64>,
    /// JSON text (only `add`/`update` events carry it; `remove` stores NULL).
    pub data: Option<String>,
    /// Event timestamp in milliseconds.
    pub updated_at: i64,
}

/// The event entity value bound into `bookmark_events.entity_id`.
enum EventEntity<'a> {
    Int(i64),
    Text(&'a str),
    Null,
}

/// Fields of one appended `bookmark_events` row.
struct NewEvent<'a> {
    action: &'a str,
    entity: EventEntity<'a>,
    kind: Option<&'a str>,
    card_id: Option<i64>,
    data: Option<&'a str>,
    updated_at: i64,
}

/// One card accepted by `sync`.
struct SyncRow {
    kind: String,
    card_id: i64,
    data: String,
}

/// Repository over `bookmarks` and `bookmark_events`.
#[derive(Debug, Clone)]
pub struct BookmarksRepo {
    pool: SqlitePool,
}

impl BookmarksRepo {
    /// Builds the repository from the shared single-writer pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Legacy `version`: `MAX(bookmark_events.id)` for the profile, `0` if none.
    pub async fn version(&self, profile_id: i64) -> Result<i64, DbError> {
        let version: Option<i64> =
            sqlx::query_scalar("SELECT MAX(id) FROM bookmark_events WHERE profile_id = ?")
                .bind(profile_id)
                .fetch_one(&self.pool)
                .await
                .map_err(DbError::query)?;
        Ok(version.unwrap_or(0))
    }

    /// All bookmarks of the profile in legacy dump order.
    pub async fn dump(&self, profile_id: i64) -> Result<Vec<Bookmark>, DbError> {
        let rows = sqlx::query_as::<_, (i64, String, i64, String, i64)>(
            "SELECT id, type, card_id, data, updated_at FROM bookmarks \
             WHERE profile_id = ? ORDER BY updated_at ASC, id ASC",
        )
        .bind(profile_id)
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::query)?;
        Ok(rows
            .into_iter()
            .map(|(id, kind, card_id, data, updated_at)| Bookmark {
                id,
                kind,
                card_id,
                data,
                updated_at,
            })
            .collect())
    }

    /// Events after `since` in ascending id order.
    pub async fn changelog(
        &self,
        profile_id: i64,
        since: i64,
    ) -> Result<Vec<BookmarkEvent>, DbError> {
        let rows = sqlx::query(
            "SELECT id, action, entity_id, type, card_id, data, updated_at \
             FROM bookmark_events WHERE profile_id = ? AND id > ? ORDER BY id ASC",
        )
        .bind(profile_id)
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(DbError::query)?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            events.push(BookmarkEvent {
                action: row.try_get("action").map_err(DbError::query)?,
                entity_id: decode_entity_id(&row)?,
                kind: row.try_get("type").map_err(DbError::query)?,
                card_id: row.try_get("card_id").map_err(DbError::query)?,
                data: row.try_get("data").map_err(DbError::query)?,
                updated_at: row.try_get("updated_at").map_err(DbError::query)?,
            });
        }
        Ok(events)
    }

    /// Legacy `POST /api/bookmarks/add` (insert → `add`, existing → `update`).
    pub async fn add(
        &self,
        profile_id: i64,
        kind: &str,
        card_id: i64,
        data: &str,
    ) -> Result<(), DbError> {
        let now = now_millis();
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM bookmarks WHERE profile_id = ? AND type = ? AND card_id = ?",
        )
        .bind(profile_id)
        .bind(kind)
        .bind(card_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(DbError::query)?;

        match existing {
            Some(id) => {
                sqlx::query(
                    "UPDATE bookmarks SET data = ?, updated_at = ? WHERE id = ? AND profile_id = ?",
                )
                .bind(data)
                .bind(now)
                .bind(id)
                .bind(profile_id)
                .execute(&self.pool)
                .await
                .map_err(DbError::query)?;
                self.insert_event(
                    profile_id,
                    NewEvent {
                        action: "update",
                        entity: EventEntity::Int(id),
                        kind: Some(kind),
                        card_id: Some(card_id),
                        data: Some(data),
                        updated_at: now,
                    },
                )
                .await?;
            }
            None => {
                let result = sqlx::query(
                    "INSERT INTO bookmarks(profile_id, type, card_id, data, updated_at) \
                     VALUES(?, ?, ?, ?, ?)",
                )
                .bind(profile_id)
                .bind(kind)
                .bind(card_id)
                .bind(data)
                .bind(now)
                .execute(&self.pool)
                .await
                .map_err(DbError::query)?;
                self.insert_event(
                    profile_id,
                    NewEvent {
                        action: "add",
                        entity: EventEntity::Int(result.last_insert_rowid()),
                        kind: Some(kind),
                        card_id: Some(card_id),
                        data: Some(data),
                        updated_at: now,
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Legacy `POST /api/bookmarks/remove`.
    ///
    /// `existing_id` is the legacy `id` field when non-zero. An id-driven
    /// delete appends the `remove` event even when the row did not belong to
    /// the profile (the Node route always calls `deleteById`); a
    /// `(type, card_id)` lookup that finds nothing is a silent success.
    pub async fn remove(
        &self,
        profile_id: i64,
        kind: &str,
        card_id: i64,
        existing_id: Option<i64>,
    ) -> Result<(), DbError> {
        let now = now_millis();
        let id = match existing_id {
            Some(id) => Some(id),
            None => sqlx::query_scalar(
                "SELECT id FROM bookmarks WHERE profile_id = ? AND type = ? AND card_id = ?",
            )
            .bind(profile_id)
            .bind(kind)
            .bind(card_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(DbError::query)?,
        };
        let Some(id) = id else {
            return Ok(());
        };

        sqlx::query("DELETE FROM bookmarks WHERE id = ? AND profile_id = ?")
            .bind(id)
            .bind(profile_id)
            .execute(&self.pool)
            .await
            .map_err(DbError::query)?;
        self.insert_event(
            profile_id,
            NewEvent {
                action: "remove",
                entity: EventEntity::Int(id),
                kind: Some(kind),
                card_id: Some(card_id),
                data: None,
                updated_at: now,
            },
        )
        .await?;
        Ok(())
    }

    /// Legacy `POST /api/bookmarks/clear` for one bookmark type.
    pub async fn clear(&self, profile_id: i64, group: &str) -> Result<(), DbError> {
        let now = now_millis();
        sqlx::query("DELETE FROM bookmarks WHERE profile_id = ? AND type = ?")
            .bind(profile_id)
            .bind(group)
            .execute(&self.pool)
            .await
            .map_err(DbError::query)?;
        self.insert_event(
            profile_id,
            NewEvent {
                action: "clear",
                entity: EventEntity::Text(group),
                kind: None,
                card_id: None,
                data: None,
                updated_at: now,
            },
        )
        .await?;
        Ok(())
    }

    /// Legacy `POST /api/bookmarks/sync`: transactional full replacement.
    ///
    /// `data` is the parsed legacy payload (`{type: [card, …]}`); only object
    /// members whose value is an array are considered, and a card without a
    /// `parseInt`-able `id` is skipped, exactly like the Node route.
    pub async fn sync(&self, profile_id: i64, data: &Json) -> Result<(), DbError> {
        let rows = sync_rows(data);
        let now = now_millis();
        let mut transaction = self.pool.begin().await.map_err(DbError::query)?;
        let result = apply_sync(&mut transaction, profile_id, &rows, now).await;
        match result {
            Ok(()) => transaction.commit().await.map_err(DbError::query),
            Err(error) => {
                // Explicit rollback so no partial bookmarks/events survive even
                // if the connection is reused immediately afterwards.
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn insert_event(&self, profile_id: i64, event: NewEvent<'_>) -> Result<(), DbError> {
        let query = sqlx::query(
            "INSERT INTO bookmark_events(profile_id, action, entity_id, type, card_id, data, \
             updated_at) VALUES(?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(profile_id)
        .bind(event.action);
        let query = match event.entity {
            EventEntity::Int(id) => query.bind(id),
            EventEntity::Text(value) => query.bind(value),
            EventEntity::Null => query.bind(Option::<i64>::None),
        };
        query
            .bind(event.kind)
            .bind(event.card_id)
            .bind(event.data)
            .bind(event.updated_at)
            .execute(&self.pool)
            .await
            .map_err(DbError::query)?;
        Ok(())
    }
}

/// Transactional body of [`BookmarksRepo::sync`].
async fn apply_sync(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: i64,
    rows: &[SyncRow],
    now: i64,
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM bookmarks WHERE profile_id = ?")
        .bind(profile_id)
        .execute(&mut **transaction)
        .await
        .map_err(DbError::query)?;

    for row in rows {
        sqlx::query(
            "INSERT INTO bookmarks(profile_id, type, card_id, data, updated_at) VALUES(?, ?, ?, ?, ?) \
             ON CONFLICT(profile_id, type, card_id) DO UPDATE SET data = excluded.data, \
             updated_at = excluded.updated_at",
        )
        .bind(profile_id)
        .bind(&row.kind)
        .bind(row.card_id)
        .bind(&row.data)
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(DbError::query)?;

        let entity_id: i64 = sqlx::query_scalar(
            "SELECT id FROM bookmarks WHERE profile_id = ? AND type = ? AND card_id = ?",
        )
        .bind(profile_id)
        .bind(&row.kind)
        .bind(row.card_id)
        .fetch_one(&mut **transaction)
        .await
        .map_err(DbError::query)?;

        sqlx::query(
            "INSERT INTO bookmark_events(profile_id, action, entity_id, type, card_id, data, \
             updated_at) VALUES(?, 'add', ?, ?, ?, ?, ?)",
        )
        .bind(profile_id)
        .bind(entity_id)
        .bind(&row.kind)
        .bind(row.card_id)
        .bind(&row.data)
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(DbError::query)?;
    }
    Ok(())
}

/// Legacy `Object.keys(data)` card collection for `sync`.
fn sync_rows(data: &Json) -> Vec<SyncRow> {
    let Json::Object(entries) = data else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for (kind, value) in entries {
        let Json::Array(cards) = value else {
            continue;
        };
        for card in cards {
            let Some(card_id) = card.get("id").and_then(Json::js_parse_int) else {
                continue;
            };
            rows.push(SyncRow {
                kind: kind.clone(),
                card_id,
                data: card.to_compact_string(),
            });
        }
    }
    rows
}

/// Reads `bookmark_events.entity_id` without assuming the legacy typeless column
/// holds only integers (a `clear` event stores the bookmark type as text).
fn decode_entity_id(row: &SqliteRow) -> Result<EntityId, DbError> {
    match row.try_get::<Option<i64>, _>("entity_id") {
        Ok(Some(value)) => Ok(EntityId::Int(value)),
        Ok(None) => Ok(EntityId::Null),
        Err(_) => match row.try_get::<Option<String>, _>("entity_id") {
            Ok(Some(value)) => Ok(EntityId::Text(value)),
            Ok(None) => Ok(EntityId::Null),
            Err(error) => Err(DbError::query(error)),
        },
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

    async fn seed_bookmark(database: &Database, profile_id: i64, kind: &str, card_id: i64) {
        sqlx::query(
            "INSERT INTO bookmarks(profile_id, type, card_id, data, updated_at) \
             VALUES(?, ?, ?, ?, 1)",
        )
        .bind(profile_id)
        .bind(kind)
        .bind(card_id)
        .bind(format!("{{\"id\":{card_id}}}"))
        .execute(database.pool())
        .await
        .expect("seed bookmark");
    }

    #[tokio::test]
    async fn version_dump_and_changelog_use_max_event_id_and_legacy_order() {
        let (_temp, database) = fixture().await;
        let repo = BookmarksRepo::new(database.pool().clone());
        seed_bookmark(&database, 1, "film", 10).await;
        seed_bookmark(&database, 1, "game", 20).await;
        for (index, updated_at) in [(1_i64, 5_i64), (2, 1)] {
            sqlx::query(
                "INSERT INTO bookmarks(profile_id, type, card_id, data, updated_at) \
                 VALUES(1, 'film', ?, '{}', ?)",
            )
            .bind(100 + index)
            .bind(updated_at)
            .execute(database.pool())
            .await
            .expect("seed ordered bookmark");
        }
        sqlx::query(
            "INSERT INTO bookmark_events(profile_id, action, entity_id, type, card_id, data, \
             updated_at) VALUES(1, 'add', 1, 'film', 10, '{}', 1)",
        )
        .execute(database.pool())
        .await
        .expect("seed add event");
        sqlx::query(
            "INSERT INTO bookmark_events(profile_id, action, entity_id, type, card_id, data, \
             updated_at) VALUES(1, 'clear', 'film', NULL, NULL, NULL, 2)",
        )
        .execute(database.pool())
        .await
        .expect("seed clear event");

        assert_eq!(repo.version(1).await.expect("version"), 2);
        assert_eq!(repo.version(9).await.expect("empty version"), 0);

        let dump = repo.dump(1).await.expect("dump");
        assert_eq!(dump.len(), 4);
        assert_eq!(
            dump.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![1, 2, 4, 3],
            "updated_at ASC, id ASC"
        );

        let changelog = repo.changelog(1, 0).await.expect("changelog");
        assert_eq!(changelog.len(), 2);
        assert_eq!(changelog[0].action, "add");
        assert_eq!(changelog[0].entity_id, EntityId::Int(1));
        assert_eq!(changelog[1].action, "clear");
        assert_eq!(changelog[1].entity_id, EntityId::Text("film".to_owned()));
        assert_eq!(changelog[1].kind, None);
        assert_eq!(changelog[1].data, None);
        assert_eq!(repo.changelog(1, 1).await.expect("since").len(), 1);
        database.close().await;
    }

    #[tokio::test]
    async fn add_upsert_remove_and_clear_append_legacy_events() {
        let (_temp, database) = fixture().await;
        let repo = BookmarksRepo::new(database.pool().clone());

        repo.add(1, "film", 10, r#"{"id":10,"custom":true}"#)
            .await
            .expect("insert");
        repo.add(1, "film", 10, r#"{"id":10,"custom":false}"#)
            .await
            .expect("update");
        repo.add(1, "cartoon", 20, "{}")
            .await
            .expect("insert second");

        let dump = repo.dump(1).await.expect("dump");
        assert_eq!(dump.len(), 2);
        assert_eq!(dump[0].data, r#"{"id":10,"custom":false}"#);

        let changelog = repo.changelog(1, 0).await.expect("changelog");
        let actions: Vec<_> = changelog
            .iter()
            .map(|event| (event.action.as_str(), event.entity_id.clone()))
            .collect();
        assert_eq!(
            actions,
            vec![
                ("add", EntityId::Int(dump[0].id)),
                ("update", EntityId::Int(dump[0].id)),
                ("add", EntityId::Int(dump[1].id)),
            ]
        );
        assert_eq!(
            changelog[1].data.as_deref(),
            Some(r#"{"id":10,"custom":false}"#)
        );

        // Removing an unknown type/card is a silent success with no event.
        let before = repo.changelog(1, 0).await.expect("before").len();
        repo.remove(1, "film", 999, None)
            .await
            .expect("unknown remove");
        assert_eq!(repo.changelog(1, 0).await.expect("after").len(), before);

        // Removing by id always appends the legacy `remove` event.
        repo.remove(1, "film", 10, Some(dump[0].id))
            .await
            .expect("remove");
        assert_eq!(repo.dump(1).await.expect("dump").len(), 1);
        let changelog = repo.changelog(1, 0).await.expect("changelog");
        assert_eq!(changelog.last().expect("event").action, "remove");
        assert_eq!(changelog.last().expect("event").data, None);

        // Clear stores the type name in entity_id (text in a typeless column).
        repo.clear(1, "cartoon").await.expect("clear");
        assert!(repo.dump(1).await.expect("dump").is_empty());
        let changelog = repo.changelog(1, 0).await.expect("changelog");
        assert_eq!(changelog.last().expect("event").action, "clear");
        assert_eq!(
            changelog.last().expect("event").entity_id,
            EntityId::Text("cartoon".to_owned())
        );
        assert_eq!(changelog.last().expect("event").kind, None);

        // Profile isolation: nothing leaked into profile 2.
        assert_eq!(repo.version(2).await.expect("version"), 0);
        assert!(repo.dump(2).await.expect("dump").is_empty());
        database.close().await;
    }

    #[tokio::test]
    async fn sync_replaces_all_bookmarks_and_canonicalises_cards() {
        let (_temp, database) = fixture().await;
        let repo = BookmarksRepo::new(database.pool().clone());
        seed_bookmark(&database, 1, "film", 1).await;
        seed_bookmark(&database, 2, "film", 2).await;

        let payload = Json::parse(
            r#"{"cartoon":[{"title":"C","id":30}],"film":[{"id":10},{"id":"11"},{"nope":1}],"other":"skip"}"#,
        )
        .expect("payload");
        repo.sync(1, &payload).await.expect("sync");

        let dump = repo.dump(1).await.expect("dump");
        assert_eq!(
            dump.iter().map(|row| row.card_id).collect::<Vec<_>>(),
            vec![30, 10, 11]
        );
        assert_eq!(dump[0].data, r#"{"title":"C","id":30}"#);
        assert_eq!(
            dump[1].data, r#"{"id":10}"#,
            "object members keep their order"
        );

        let changelog = repo.changelog(1, 0).await.expect("changelog");
        assert_eq!(changelog.len(), 3);
        assert!(changelog.iter().all(|event| event.action == "add"));
        assert_eq!(repo.version(1).await.expect("version"), 3);

        // Profile 2 is untouched by a profile-1 sync.
        assert_eq!(repo.dump(2).await.expect("dump").len(), 1);

        // A non-object payload deletes every bookmark (legacy `{}` semantics).
        repo.sync(1, &Json::parse("5").expect("number"))
            .await
            .expect("sync number");
        assert!(repo.dump(1).await.expect("dump").is_empty());
        database.close().await;
    }

    #[tokio::test]
    async fn sync_rolls_back_bookmarks_and_events_on_failure() {
        let (_temp, database) = fixture().await;
        let repo = BookmarksRepo::new(database.pool().clone());
        seed_bookmark(&database, 1, "film", 1).await;
        sqlx::query(
            "INSERT INTO bookmark_events(profile_id, action, entity_id, type, card_id, data, \
             updated_at) VALUES(1, 'add', 1, 'film', 1, '{}', 1)",
        )
        .execute(database.pool())
        .await
        .expect("seed event");

        // Fail only the second card's event insert, after the first bookmark and
        // event were already written inside the transaction.
        sqlx::query(
            "CREATE TRIGGER fail_bookmark_event BEFORE INSERT ON bookmark_events \
             WHEN NEW.card_id = 9008 BEGIN SELECT RAISE(ABORT, 'forced rollback'); END",
        )
        .execute(database.pool())
        .await
        .expect("trigger");

        let payload =
            Json::parse(r#"{"film":[{"id":9001}],"game":[{"id":9008}]}"#).expect("payload");
        let error = repo.sync(1, &payload).await.expect_err("sync must fail");
        assert_eq!(error.code(), "db_query_failed");
        assert!(!error.to_string().contains("forced rollback"));

        let dump = repo.dump(1).await.expect("dump");
        assert_eq!(dump.len(), 1, "the delete must be rolled back");
        assert_eq!(dump[0].card_id, 1);
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM bookmark_events")
            .fetch_one(database.pool())
            .await
            .expect("count events");
        assert_eq!(events, 1, "no partial events may survive the rollback");
        database.close().await;
    }
}
