const sqlite3 = require('sqlite3').verbose();

let db = new sqlite3.Database('./database.sqlite', (err) => {
    if (err) console.error(err.message);
    else {
        console.log('Connected to the SQLite database.');
        db.run("CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT)", () => {
             db.run(`INSERT INTO settings(key, value) VALUES('admin_password', 'admin') ON CONFLICT(key) DO NOTHING`);
        });
    }
});

module.exports = db;
