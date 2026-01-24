const db = require('../database');

const BACKUP_KEY = 'backup_latest';

function json(res, status, payload) {
    if (res.headersSent || res.writableEnded) return;
    res.writeHead(status, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(payload));
}

module.exports = {
    handle: (req, res) => {
        if (req.url === '/api/users/backup/export' && req.method === 'POST') {
            const raw = typeof req.body?.data === 'string' ? req.body.data : '';
            if (!raw) return json(res, 400, { secuses: false, text: 'Missing data' });

            try {
                JSON.parse(raw);
            } catch (e) {
                return json(res, 400, { secuses: false, text: 'Invalid JSON' });
            }

            db.run(
                "INSERT INTO settings(key, value, visible) VALUES(?, ?, 0) ON CONFLICT(key) DO UPDATE SET value=excluded.value, visible=0",
                [BACKUP_KEY, raw],
                (err) => {
                    if (err) return json(res, 500, { secuses: false, error: err.message });
                    json(res, 200, { secuses: true });
                }
            );
            return;
        }

        if (req.url === '/api/users/backup/import' && req.method === 'GET') {
            db.get("SELECT value FROM settings WHERE key=?", [BACKUP_KEY], (err, row) => {
                if (err) return json(res, 500, { secuses: false, error: err.message });
                if (!row?.value) return json(res, 200, { secuses: true, data: null });

                try {
                    json(res, 200, { secuses: true, data: JSON.parse(row.value) });
                } catch (e) {
                    json(res, 200, { secuses: true, data: null });
                }
            });
            return;
        }

        res.writeHead(404);
        res.end();
    }
};
