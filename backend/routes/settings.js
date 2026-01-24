const db = require('../database');

module.exports = {
    handle: (req, res) => {
        if (req.cookies['auth_token'] !== 'valid_token') {
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
        } else if (req.method === 'POST') {
            const { key, value } = req.body;
            db.run(`INSERT INTO settings(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value`, [key, value], function(err) {
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
    }
};
