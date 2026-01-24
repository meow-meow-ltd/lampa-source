const sqlite3 = require('sqlite3').verbose();
const crypto = require('crypto');

let db = new sqlite3.Database('./database.sqlite', (err) => {
    if (err) console.error(err.message);
    else {
        console.log('Connected to the SQLite database.');
        db.run("CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT, visible BOOLEAN NOT NULL DEFAULT 0)", () => {
             db.run(`INSERT INTO settings(key, value, visible) VALUES('admin_password', 'admin', 0) ON CONFLICT(key) DO NOTHING`);
             const randomToken = crypto.randomBytes(16).toString('hex');
             db.run(`INSERT INTO settings(key, value, visible) VALUES('admin_token', ?, 0) ON CONFLICT(key) DO NOTHING`, [randomToken]);
             db.run(`INSERT INTO settings(key, value, visible) VALUES('torrserver_url', '', 1) ON CONFLICT(key) DO NOTHING`);
        });
    }
});

module.exports = db;
