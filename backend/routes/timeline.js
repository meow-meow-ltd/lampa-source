const db = require('../database');
const url = require('url');

function now() {
    return Date.now();
}

function profileIdFromReq(req) {
    const raw = req.headers?.profile || req.headers?.['x-profile'] || req.body?.profile || req.body?.params?.profile;
    const id = parseInt(raw);
    return isNaN(id) ? 0 : id;
}

function json(res, status, payload) {
    res.writeHead(status, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(payload));
}

function getVersion(profileId, callback) {
    db.get("SELECT MAX(id) as version FROM timeline_events WHERE profile_id=?", [profileId], (err, row) => {
        if (err) return callback(err);
        callback(null, row?.version || 0);
    });
}

function parseData(hash, data) {
    try {
        const obj = typeof data === 'string' ? JSON.parse(data) : data;
        if (obj && typeof obj === 'object') obj.hash = hash;
        return obj;
    } catch (e) {
        return { hash };
    }
}

module.exports = {
    handle: (req, res) => {
        const parsed = url.parse(req.url, true);
        const pathname = parsed.pathname;

        if (pathname === '/api/timeline/dump' && req.method === 'GET') {
            const profileId = profileIdFromReq(req);
            if (!profileId) return json(res, 400, { secuses: false, text: 'Missing profile' });

            return getVersion(profileId, (err, version) => {
                if (err) return json(res, 500, { secuses: false, error: err.message });

                db.all(
                    "SELECT hash, data FROM timeline WHERE profile_id=? ORDER BY updated_at ASC",
                    [profileId],
                    (err, rows) => {
                        if (err) return json(res, 500, { secuses: false, error: err.message });

                        const timelines = {};
                        (rows || []).forEach((row) => {
                            timelines[row.hash] = parseData(row.hash, row.data);
                        });

                        res.writeHead(200, { 'Content-Type': 'text/plain; charset=utf-8' });
                        res.end(JSON.stringify({ version, timelines }));
                    }
                );
            });
        }

        if (pathname === '/api/timeline/changelog' && req.method === 'GET') {
            const profileId = profileIdFromReq(req);
            if (!profileId) return json(res, 400, { secuses: false, text: 'Missing profile' });

            const since = parseInt(parsed.query?.since || 0) || 0;

            return getVersion(profileId, (err, version) => {
                if (err) return json(res, 500, { secuses: false, error: err.message });

                db.all(
                    "SELECT id, hash, data FROM timeline_events WHERE profile_id=? AND id>? ORDER BY id ASC",
                    [profileId, since],
                    (err, rows) => {
                        if (err) return json(res, 500, { secuses: false, error: err.message });

                        const timelines = {};
                        (rows || []).forEach((row) => {
                            timelines[row.hash] = parseData(row.hash, row.data);
                        });

                        json(res, 200, { version, timelines });
                    }
                );
            });
        }

        if (pathname === '/api/timeline/update' && req.method === 'POST') {
            const profileId = profileIdFromReq(req);
            if (!profileId) return json(res, 400, { secuses: false, text: 'Missing profile' });

            const payload = req.body?.params || req.body || {};
            const hash = typeof payload.hash === 'string' ? payload.hash : '';
            if (!hash) return json(res, 400, { secuses: false, text: 'Missing hash' });

            const t = now();
            const data = JSON.stringify({
                hash,
                percent: payload.percent || 0,
                time: payload.time || 0,
                duration: payload.duration || 0,
                profile: payload.profile || profileId
            });

            db.run(
                "INSERT INTO timeline(profile_id, hash, data, updated_at) VALUES(?, ?, ?, ?) " +
                    "ON CONFLICT(profile_id, hash) DO UPDATE SET data=excluded.data, updated_at=excluded.updated_at",
                [profileId, hash, data, t],
                (err) => {
                    if (err) return json(res, 500, { secuses: false, error: err.message });

                    db.run(
                        "INSERT INTO timeline_events(profile_id, hash, data, updated_at) VALUES(?, ?, ?, ?)",
                        [profileId, hash, data, t],
                        (err) => {
                            if (err) return json(res, 500, { secuses: false, error: err.message });
                            json(res, 200, { secuses: true });
                        }
                    );
                }
            );
        }

        res.writeHead(404);
        res.end();
    }
};

