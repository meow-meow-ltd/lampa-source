const sqlite3 = require('sqlite3').verbose();
const crypto = require('crypto');
const SETTINGS_KEYS = require('./settings_keys');

function safeRun(db, sql, params = []) {
    db.run(sql, params, (err) => {
        if (err) console.error('SQLite run error:', err.message, 'SQL:', String(sql).trim());
    });
}

function safeGet(db, sql, params, cb) {
    db.get(sql, params, (err, row) => {
        if (err) console.error('SQLite get error:', err.message, 'SQL:', String(sql).trim());
        cb(err, row);
    });
}

let db = new sqlite3.Database('./database.sqlite', (err) => {
    if (err) console.error(err.message);
    else {
        console.log('Connected to the SQLite database.');

        db.serialize();

        safeRun(db, "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT, visible BOOLEAN NOT NULL DEFAULT 0)");

        safeRun(db, "CREATE TABLE IF NOT EXISTS schema_version (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL)");
        safeRun(db, "INSERT OR IGNORE INTO schema_version(id, version) VALUES(1, 0)");

        safeGet(db, "SELECT version FROM schema_version WHERE id=1", [], (err, row) => {
            if (err) {
                console.error('schema_version read error:', err.message);
                return;
            }

            let version = row?.version || 0;

            function setVersion(nextVersion) {
                safeRun(db, "UPDATE schema_version SET version=? WHERE id=1", [nextVersion]);
                version = nextVersion;
            }

            if (version < 1) {
                safeRun(db, `
                    CREATE TABLE IF NOT EXISTS profiles (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        name TEXT NOT NULL,
                        icon TEXT,
                        is_main INTEGER NOT NULL DEFAULT 0,
                        is_child INTEGER NOT NULL DEFAULT 0,
                        created_at INTEGER NOT NULL,
                        updated_at INTEGER NOT NULL
                    )
                `);

                safeRun(db, `
                    CREATE TABLE IF NOT EXISTS bookmarks (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        profile_id INTEGER NOT NULL,
                        type TEXT NOT NULL,
                        card_id INTEGER NOT NULL,
                        data TEXT NOT NULL,
                        updated_at INTEGER NOT NULL
                    )
                `);
                safeRun(db, "CREATE UNIQUE INDEX IF NOT EXISTS bookmarks_unique ON bookmarks(profile_id, type, card_id)");
                safeRun(db, "CREATE INDEX IF NOT EXISTS bookmarks_profile_type ON bookmarks(profile_id, type)");

                safeRun(db, `
                    CREATE TABLE IF NOT EXISTS bookmark_events (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        profile_id INTEGER NOT NULL,
                        action TEXT NOT NULL,
                        entity_id INTEGER,
                        type TEXT,
                        card_id INTEGER,
                        data TEXT,
                        updated_at INTEGER NOT NULL
                    )
                `);
                safeRun(db, "CREATE INDEX IF NOT EXISTS bookmark_events_profile_id ON bookmark_events(profile_id, id)");

                safeRun(db, `
                    CREATE TABLE IF NOT EXISTS timeline (
                        hash TEXT PRIMARY KEY,
                        profile_id INTEGER NOT NULL,
                        data TEXT NOT NULL,
                        updated_at INTEGER NOT NULL
                    )
                `);
                safeRun(db, "CREATE INDEX IF NOT EXISTS timeline_profile_id ON timeline(profile_id)");

                safeRun(db, `
                    CREATE TABLE IF NOT EXISTS timeline_events (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        profile_id INTEGER NOT NULL,
                        hash TEXT NOT NULL,
                        data TEXT NOT NULL,
                        updated_at INTEGER NOT NULL
                    )
                `);
                safeRun(db, "CREATE INDEX IF NOT EXISTS timeline_events_profile_id ON timeline_events(profile_id, id)");

                setVersion(1);
            }

            if (version < 2) {
                safeGet(db, "SELECT name FROM sqlite_master WHERE type='table' AND name='timeline'", [], (err, exists) => {
                    if (err) return;

                    if (exists?.name) safeRun(db, "ALTER TABLE timeline RENAME TO timeline_old");

                    safeRun(db, `
                        CREATE TABLE IF NOT EXISTS timeline (
                            profile_id INTEGER NOT NULL,
                            hash TEXT NOT NULL,
                            data TEXT NOT NULL,
                            updated_at INTEGER NOT NULL,
                            PRIMARY KEY(profile_id, hash)
                        )
                    `);
                    safeRun(db, "CREATE INDEX IF NOT EXISTS timeline_profile_id ON timeline(profile_id)");

                    if (exists?.name) safeRun(
                        db,
                        "INSERT OR IGNORE INTO timeline(profile_id, hash, data, updated_at) SELECT profile_id, hash, data, updated_at FROM timeline_old"
                    );
                    if (exists?.name) safeRun(db, "DROP TABLE timeline_old");

                    setVersion(2);
                });
            }

            if (version < 3) {
                safeGet(db, "SELECT name FROM sqlite_master WHERE type='table' AND name='timeline_old'", [], (err, exists) => {
                    if (err) return;

                    if (exists?.name) safeRun(db, "DROP TABLE timeline_old");

                    setVersion(3);
                });
            }

            safeRun(
                db,
                `INSERT INTO settings(key, value, visible) VALUES(?, 'admin', 0) ON CONFLICT(key) DO NOTHING`,
                [SETTINGS_KEYS.ADMIN_PASSWORD]
            );
            const randomToken = crypto.randomBytes(16).toString('hex');
            safeRun(
                db,
                `INSERT INTO settings(key, value, visible) VALUES(?, ?, 0) ON CONFLICT(key) DO NOTHING`,
                [SETTINGS_KEYS.ADMIN_TOKEN, randomToken]
            );
            safeRun(
                db,
                `INSERT INTO settings(key, value, visible) VALUES(?, '', 1) ON CONFLICT(key) DO NOTHING`,
                [SETTINGS_KEYS.TORRSERVER_URL]
            );
        });
    }
});

module.exports = db;
