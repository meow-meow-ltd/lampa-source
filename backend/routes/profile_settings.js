const db = require('../database');
const url = require('url');

function json(res, status, payload) {
    if (res.headersSent || res.writableEnded) return;
    res.writeHead(status, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(payload));
}

function profileIdFromReq(req) {
    const raw = req.headers?.profile || req.headers?.['x-profile'] || req.body?.profile || req.body?.params?.profile;
    const id = parseInt(raw);
    return Number.isFinite(id) ? id : 0;
}

function now() {
    return Date.now();
}

module.exports = {
    handle: (req, res) => {
        if (!req.url.startsWith('/api/profile/settings')) {
            res.writeHead(404);
            return res.end();
        }

        const profileId = profileIdFromReq(req);
        if (!profileId) return json(res, 400, { secuses: false, text: 'Missing profile' });

        if (req.method === 'GET') {
            const query = url.parse(req.url, true).query;
            const key = query.key;

            if (key) {
                db.get(
                    "SELECT value FROM profile_settings WHERE profile_id=? AND key=?",
                    [profileId, key],
                    (err, row) => {
                        if (err) return json(res, 500, { secuses: false, error: err.message });
                        json(res, 200, { secuses: true, value: row ? row.value : null });
                    }
                );
            } else {
                db.all(
                    "SELECT key, value FROM profile_settings WHERE profile_id=?",
                    [profileId],
                    (err, rows) => {
                        if (err) return json(res, 500, { secuses: false, error: err.message });
                        
                        const settings = {};
                        rows.forEach(row => {
                            settings[row.key] = row.value;
                        });
                        
                        json(res, 200, { secuses: true, value: settings });
                    }
                );
            }
            return;
        }

        if (req.method === 'POST') {
            const key = typeof req.body?.key === 'string' ? req.body.key.trim() : '';
            let value = typeof req.body?.value === 'undefined' ? undefined : req.body.value;

            if (!key) return json(res, 400, { secuses: false, text: 'Missing key' });
            if (typeof value === 'undefined') return json(res, 400, { secuses: false, text: 'Missing value' });

            if (value === null) {
                db.run(
                    "DELETE FROM profile_settings WHERE profile_id=? AND key=?",
                    [profileId, key],
                    (err) => {
                        if (err) return json(res, 500, { secuses: false, error: err.message });
                        json(res, 200, { secuses: true });
                    }
                );
                return;
            }

            if (typeof value !== 'string') {
                try {
                    value = JSON.stringify(value);
                } catch (e) {
                    return json(res, 400, { secuses: false, text: 'Invalid value' });
                }
            }

            db.run(
                "INSERT INTO profile_settings(profile_id, key, value, updated_at) VALUES(?, ?, ?, ?) " +
                    "ON CONFLICT(profile_id, key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
                [profileId, key, value, now()],
                (err) => {
                    if (err) return json(res, 500, { secuses: false, error: err.message });
                    json(res, 200, { secuses: true });
                }
            );
            return;
        }

        res.writeHead(405);
        res.end();
    }
};
