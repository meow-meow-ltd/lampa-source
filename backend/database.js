const sqlite3 = require('sqlite3').verbose();
const crypto = require('crypto');
const SETTINGS_KEYS = require('../src/utils/constants');

let db = new sqlite3.Database('./database.sqlite', (err) => {
    if (err) console.error(err.message);
    else {
        console.log('Connected to the SQLite database.');
        db.run("CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT, visible BOOLEAN NOT NULL DEFAULT 0)", () => {
             db.run(`INSERT INTO settings(key, value, visible) VALUES(?, 'admin', 0) ON CONFLICT(key) DO NOTHING`, [SETTINGS_KEYS.ADMIN_PASSWORD]);
             const randomToken = crypto.randomBytes(16).toString('hex');
             db.run(`INSERT INTO settings(key, value, visible) VALUES(?, ?, 0) ON CONFLICT(key) DO NOTHING`, [SETTINGS_KEYS.ADMIN_TOKEN, randomToken]);
             db.run(`INSERT INTO settings(key, value, visible) VALUES(?, '', 1) ON CONFLICT(key) DO NOTHING`, [SETTINGS_KEYS.TORRSERVER_URL]);
        });
    }
});

module.exports = db;
