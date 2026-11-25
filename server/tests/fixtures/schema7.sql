-- schema7.sql — TEST-ONLY fixture reproducing the legacy Lampa schema v7.
--
-- SOURCE OF TRUTH: review/inv/S2-schema.sql (DDL read from the production snapshot
-- `/opt/lampa/poc/fixtures/lampa-copy.sqlite`, sha256 cbe70d02…b78106, 8 110 080 B).
-- The CREATE statements below are copied verbatim (same order, same whitespace) from
-- that audit artefact, so `sqlite_master.sql` is identical to legacy v7.
--
-- Expected shape after applying this file to an empty database:
--   * 10 application tables + sqlite_sequence + 8 explicit indexes + 4 autoindexes
--     = 23 objects in sqlite_schema;
--   * `schema_version` row `{id: 1, version: 7}`;
--   * `journal_mode = delete` (nothing here sets WAL);
--   * no foreign keys, no views, no triggers, no WITHOUT ROWID/STRICT tables.
--
-- USAGE: tests only (synthetic temp databases under /tmp or a CI tmpdir).
-- This file is NOT a migration: the server never applies it, never executes DDL at
-- runtime, and never creates `_sqlx_migrations`. Do not point it at
-- /opt/lampa/data/database.sqlite or /opt/lampa/staging-data/database.sqlite.
--
-- `CREATE TABLE sqlite_sequence(name, seq);` is intentionally absent: the name is
-- reserved by SQLite and the table appears automatically because of AUTOINCREMENT.

-- ===== TABLES =====
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT, visible BOOLEAN NOT NULL DEFAULT 0);

CREATE TABLE schema_version (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);

CREATE TABLE profiles (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        name TEXT NOT NULL,
                        icon TEXT,
                        is_main INTEGER NOT NULL DEFAULT 0,
                        is_child INTEGER NOT NULL DEFAULT 0,
                        created_at INTEGER NOT NULL,
                        updated_at INTEGER NOT NULL
                    , sort_order INTEGER NOT NULL DEFAULT 0);

CREATE TABLE bookmarks (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        profile_id INTEGER NOT NULL,
                        type TEXT NOT NULL,
                        card_id INTEGER NOT NULL,
                        data TEXT NOT NULL,
                        updated_at INTEGER NOT NULL
                    );

CREATE TABLE bookmark_events (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        profile_id INTEGER NOT NULL,
                        action TEXT NOT NULL,
                        entity_id INTEGER,
                        type TEXT,
                        card_id INTEGER,
                        data TEXT,
                        updated_at INTEGER NOT NULL
                    );

CREATE TABLE timeline_events (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        profile_id INTEGER NOT NULL,
                        hash TEXT NOT NULL,
                        data TEXT NOT NULL,
                        updated_at INTEGER NOT NULL
                    );

CREATE TABLE profile_settings (
                        profile_id INTEGER NOT NULL,
                        key TEXT NOT NULL,
                        value TEXT,
                        updated_at INTEGER NOT NULL,
                        PRIMARY KEY(profile_id, key)
                    );

CREATE TABLE timeline (
                            profile_id INTEGER NOT NULL,
                            hash TEXT NOT NULL,
                            data TEXT NOT NULL,
                            updated_at INTEGER NOT NULL,
                            PRIMARY KEY(profile_id, hash)
                        );

CREATE TABLE device_access (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        device_id TEXT NOT NULL,
                        ip TEXT,
                        user_agent TEXT,
                        status TEXT NOT NULL DEFAULT 'pending',
                        attempts INTEGER DEFAULT 0,
                        last_attempt_at INTEGER,
                        blocked_until INTEGER,
                        last_seen_at INTEGER,
                        token_hash TEXT
                    );

CREATE TABLE ip_rate_limits (
                        ip TEXT PRIMARY KEY,
                        attempts INTEGER DEFAULT 0,
                        first_attempt_at INTEGER NOT NULL,
                        blocked_until INTEGER
                    );

-- ===== EXPLICIT INDEXES =====
CREATE UNIQUE INDEX bookmarks_unique ON bookmarks(profile_id, type, card_id);

CREATE INDEX bookmarks_profile_type ON bookmarks(profile_id, type);

CREATE INDEX bookmark_events_profile_id ON bookmark_events(profile_id, id);

CREATE INDEX timeline_events_profile_id ON timeline_events(profile_id, id);

CREATE INDEX profile_settings_profile_id ON profile_settings(profile_id);

CREATE INDEX profiles_sort_order ON profiles(sort_order);

CREATE UNIQUE INDEX device_access_id ON device_access(device_id);

CREATE INDEX ip_rate_limits_blocked ON ip_rate_limits(blocked_until);

-- ===== SCHEMA VERSION ROW =====
INSERT INTO schema_version(id, version) VALUES (1, 7);
