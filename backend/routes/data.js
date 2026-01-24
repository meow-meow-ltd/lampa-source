const db = require('../database');
const url = require('url');

module.exports = {
    handle: (req, res) => {
        if (req.method === 'POST') {
            // POST /api/values
            const keys = req.body?.keys;
            if (!Array.isArray(keys) || keys.length === 0) {
                res.writeHead(400, { 'Content-Type': 'application/json' });
                return res.end(JSON.stringify({ error: 'Missing or invalid keys parameter' }));
            }

            const placeholders = keys.map(() => '?').join(',');
            const query = `SELECT key, value, visible FROM settings WHERE key IN (${placeholders})`;

            db.all(query, keys, (err, rows) => {
                if (err) {
                    res.writeHead(500, { 'Content-Type': 'application/json' });
                    return res.end(JSON.stringify({ error: err.message }));
                }

                const result = {};
                rows.forEach(row => {
                    if (row.visible) {
                        result[row.key] = row.value;
                    } else {
                        result[row.key] = null;
                    }
                });

                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify(result));
            });
            return;
        } else if (req.method === 'GET') {
            // GET /api/value?key=KEY
            const query = url.parse(req.url, true).query;
            const key = query.key;

            if (!key) {
                res.writeHead(400, { 'Content-Type': 'application/json' });
                return res.end(JSON.stringify({ error: 'Missing key parameter' }));
            }

            db.get("SELECT value, visible FROM settings WHERE key = ?", [key], (err, row) => {
                if (err) {
                    res.writeHead(500, { 'Content-Type': 'application/json' });
                    return res.end(JSON.stringify({ error: err.message }));
                }

                if (row && row.visible) {
                    res.writeHead(200, { 'Content-Type': 'application/json' });
                    res.end(JSON.stringify(row.value));
                } else {
                    res.writeHead(200, { 'Content-Type': 'application/json' });
                    res.end(JSON.stringify(null));
                }
            });
        }
    }
};
