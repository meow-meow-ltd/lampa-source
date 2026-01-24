const db = require('../database');
const url = require('url');

module.exports = {
    handle: (req, res) => {
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
};
