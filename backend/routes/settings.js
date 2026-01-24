const db = require('../database');
const SETTINGS_KEYS = require('../../src/utils/constants');

module.exports = {
    handle: (req, res) => {
        db.get("SELECT value FROM settings WHERE key = ?", [SETTINGS_KEYS.ADMIN_TOKEN], (err, tokenRow) => {
            const validToken = tokenRow?.value;
            if (req.cookies['auth_token'] !== validToken) {
                 res.writeHead(401);
                 return res.end();
            }

            if (req.method === 'GET') {
            db.all("SELECT * FROM settings", [], (err, rows) => {
                if (err) {
                    res.writeHead(500, { 'Content-Type': 'application/json' });
                    res.end(JSON.stringify({ error: err.message }));
                    return;
                }
                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify(rows));
            });
        } else if (req.method === 'PUT') {
            const { key, value } = req.body;
            db.run(`UPDATE settings SET value=? WHERE key=?`, [value, key], function(err) {
                if (err) {
                    console.log(err.message);
                    res.writeHead(500, { 'Content-Type': 'application/json' });
                    return res.end(JSON.stringify({ error: err.message }));
                }
                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ success: true }));
            });
        } else if (req.method === 'DELETE') {
            const key = req.url.split('/').pop();
             db.run(`DELETE FROM settings WHERE key=?`, [key], function(err) {
                if (err) {
                    console.log(err.message);
                    res.writeHead(500, { 'Content-Type': 'application/json' });
                    return res.end(JSON.stringify({ error: err.message }));
                }
                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ success: true }));
            });
        } else {
            res.writeHead(405);
            res.end();
        }
    });
}
};
