const db = require('../database');
const url = require('url');

function now() {
    return Date.now();
}

function profileIdFromReq(req) {
    const raw = req.headers?.profile || req.headers?.['x-profile'] || req.body?.profile;
    const id = parseInt(raw);
    return isNaN(id) ? 0 : id;
}

function json(res, status, payload) {
    if (res.headersSent || res.writableEnded) return;
    res.writeHead(status, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(payload));
}

function getVersion(profileId, callback) {
    db.get("SELECT MAX(id) as version FROM bookmark_events WHERE profile_id=?", [profileId], (err, row) => {
        if (err) return callback(err);
        callback(null, row?.version || 0);
    });
}

module.exports = {
    handle: (req, res) => {
        const parsed = url.parse(req.url, true);
        const pathname = parsed.pathname;

        if (pathname === '/api/bookmarks/dump' && req.method === 'GET') {
            const profileId = profileIdFromReq(req);
            if (!profileId) return json(res, 400, { secuses: false, text: 'Missing profile' });

            return getVersion(profileId, (err, version) => {
                if (err) return json(res, 500, { secuses: false, error: err.message });

                db.all(
                    "SELECT id, type, card_id, data, updated_at FROM bookmarks WHERE profile_id=? ORDER BY updated_at ASC, id ASC",
                    [profileId],
                    (err, rows) => {
                        if (err) return json(res, 500, { secuses: false, error: err.message });

                        res.writeHead(200, { 'Content-Type': 'text/plain; charset=utf-8' });
                        res.end(JSON.stringify({ version, bookmarks: rows || [] }));
                    }
                );
            });
        }

        if (pathname === '/api/bookmarks/changelog' && req.method === 'GET') {
            const profileId = profileIdFromReq(req);
            if (!profileId) return json(res, 400, { secuses: false, text: 'Missing profile' });

            const since = parseInt(parsed.query?.since || 0) || 0;

            return getVersion(profileId, (err, version) => {
                if (err) return json(res, 500, { secuses: false, error: err.message });

                db.all(
                    "SELECT id, action, entity_id, type, card_id, data, updated_at FROM bookmark_events WHERE profile_id=? AND id>? ORDER BY id ASC",
                    [profileId, since],
                    (err, rows) => {
                        if (err) return json(res, 500, { secuses: false, error: err.message });

                        const changelog = (rows || []).map((e) => {
                            if (e.action === 'add') {
                                return {
                                    action: 'add',
                                    id: e.entity_id,
                                    type: e.type,
                                    card_id: e.card_id,
                                    data: e.data,
                                    updated_at: e.updated_at
                                };
                            }
                            if (e.action === 'clear') {
                                return { action: 'clear', entity_id: e.entity_id };
                            }
                            if (e.action === 'update') {
                                return { action: 'update', entity_id: e.entity_id, updated_at: e.updated_at };
                            }
                            return { action: 'remove', entity_id: e.entity_id };
                        });

                        json(res, 200, { version, changelog });
                    }
                );
            });
        }

        if ((pathname === '/api/bookmarks/add' || pathname === '/api/bookmarks/remove') && req.method === 'POST') {
            const profileId = profileIdFromReq(req);
            if (!profileId) return json(res, 400, { secuses: false, text: 'Missing profile' });

            const action = pathname.endsWith('/add') ? 'add' : 'remove';
            const type = (req.body?.type || '').trim();
            const cardId = parseInt(req.body?.card_id);
            const existingId = parseInt(req.body?.id) || 0;
            const data = typeof req.body?.data === 'string' ? req.body.data : null;
            const t = now();

            if (!type || isNaN(cardId)) return json(res, 400, { secuses: false, text: 'Invalid payload' });

            if (action === 'remove') {
                const deleteById = (id, done) => {
                    db.run("DELETE FROM bookmarks WHERE id=? AND profile_id=?", [id, profileId], (err) => {
                        if (err) return done(err);
                        db.run(
                            "INSERT INTO bookmark_events(profile_id, action, entity_id, type, card_id, data, updated_at) VALUES(?, 'remove', ?, ?, ?, NULL, ?)",
                            [profileId, id, type, cardId, t],
                            done
                        );
                    });
                };

                if (existingId) {
                    return deleteById(existingId, (err) => {
                        if (err) return json(res, 500, { secuses: false, error: err.message });
                        json(res, 200, { secuses: true });
                    });
                }

                return db.get(
                    "SELECT id FROM bookmarks WHERE profile_id=? AND type=? AND card_id=?",
                    [profileId, type, cardId],
                    (err, row) => {
                        if (err) return json(res, 500, { secuses: false, error: err.message });
                        if (!row?.id) return json(res, 200, { secuses: true });

                        deleteById(row.id, (err) => {
                            if (err) return json(res, 500, { secuses: false, error: err.message });
                            json(res, 200, { secuses: true });
                        });
                    }
                );
            }

            if (!data) return json(res, 400, { secuses: false, text: 'Missing data' });

            db.get(
                "SELECT id FROM bookmarks WHERE profile_id=? AND type=? AND card_id=?",
                [profileId, type, cardId],
                (err, row) => {
                    if (err) return json(res, 500, { secuses: false, error: err.message });

                    const existing = row?.id || 0;

                    if (existing) {
                        db.run(
                            "UPDATE bookmarks SET data=?, updated_at=? WHERE id=? AND profile_id=?",
                            [data, t, existing, profileId],
                            (err) => {
                                if (err) return json(res, 500, { secuses: false, error: err.message });
                                db.run(
                                    "INSERT INTO bookmark_events(profile_id, action, entity_id, type, card_id, data, updated_at) VALUES(?, 'update', ?, ?, ?, ?, ?)",
                                    [profileId, existing, type, cardId, data, t],
                                    (err) => {
                                        if (err) return json(res, 500, { secuses: false, error: err.message });
                                        json(res, 200, { secuses: true });
                                    }
                                );
                            }
                        );
                    } else {
                        db.run(
                            "INSERT INTO bookmarks(profile_id, type, card_id, data, updated_at) VALUES(?, ?, ?, ?, ?)",
                            [profileId, type, cardId, data, t],
                            function (err) {
                                if (err) return json(res, 500, { secuses: false, error: err.message });

                                db.run(
                                    "INSERT INTO bookmark_events(profile_id, action, entity_id, type, card_id, data, updated_at) VALUES(?, 'add', ?, ?, ?, ?, ?)",
                                    [profileId, this.lastID, type, cardId, data, t],
                                    (err) => {
                                        if (err) return json(res, 500, { secuses: false, error: err.message });
                                        json(res, 200, { secuses: true });
                                    }
                                );
                            }
                        );
                    }
                }
            );
            return;
        }

        if (pathname === '/api/bookmarks/clear' && req.method === 'POST') {
            const profileId = profileIdFromReq(req);
            if (!profileId) return json(res, 400, { secuses: false, text: 'Missing profile' });

            const type = (req.body?.type || '').trim();
            const group = (req.body?.group || '').trim();
            const t = now();

            if (type !== 'group' || !group) return json(res, 400, { secuses: false, text: 'Invalid payload' });

            db.run("DELETE FROM bookmarks WHERE profile_id=? AND type=?", [profileId, group], (err) => {
                if (err) return json(res, 500, { secuses: false, error: err.message });

                db.run(
                    "INSERT INTO bookmark_events(profile_id, action, entity_id, type, card_id, data, updated_at) VALUES(?, 'clear', ?, NULL, NULL, NULL, ?)",
                    [profileId, group, t],
                    (err) => {
                        if (err) return json(res, 500, { secuses: false, error: err.message });
                        json(res, 200, { secuses: true });
                    }
                );
            });
            return;
        }

        if (pathname === '/api/bookmarks/sync' && req.method === 'POST') {
            const profileId = profileIdFromReq(req);
            if (!profileId) return json(res, 400, { secuses: false, text: 'Missing profile' });

            const raw = typeof req.body?.data === 'string' ? req.body.data : '';
            let data;

            try {
                data = raw ? JSON.parse(raw) : {};
            } catch (e) {
                return json(res, 400, { secuses: false, text: 'Invalid JSON' });
            }

            const t = now();

            db.serialize(() => {
                db.run("BEGIN TRANSACTION");
                db.run("DELETE FROM bookmarks WHERE profile_id=?", [profileId]);

                const inserts = [];
                if (data && typeof data === 'object') {
                    Object.keys(data).forEach((type) => {
                        const list = data[type];
                        if (!Array.isArray(list)) return;
                        list.forEach((card) => {
                            const cardId = parseInt(card?.id);
                            if (isNaN(cardId)) return;

                            inserts.push({ type, cardId, data: JSON.stringify(card) });
                        });
                    });
                }

                let pending = inserts.length;
                if (!pending) {
                    db.run("COMMIT");
                    return json(res, 200, { secuses: true });
                }

                inserts.forEach((row) => {
                    db.run(
                        "INSERT INTO bookmarks(profile_id, type, card_id, data, updated_at) VALUES(?, ?, ?, ?, ?) " +
                            "ON CONFLICT(profile_id, type, card_id) DO UPDATE SET data=excluded.data, updated_at=excluded.updated_at",
                        [profileId, row.type, row.cardId, row.data, t],
                        function (err) {
                            if (err) {
                                db.run("ROLLBACK");
                                pending = 0;
                                return json(res, 500, { secuses: false, error: err.message });
                            }

                            const entityId = this.lastID || null;
                            db.run(
                                "INSERT INTO bookmark_events(profile_id, action, entity_id, type, card_id, data, updated_at) VALUES(?, 'add', ?, ?, ?, ?, ?)",
                                [profileId, entityId, row.type, row.cardId, row.data, t],
                                (err) => {
                                    if (err) {
                                        db.run("ROLLBACK");
                                        pending = 0;
                                        return json(res, 500, { secuses: false, error: err.message });
                                    }

                                    pending -= 1;
                                    if (pending === 0) {
                                        db.run("COMMIT");
                                        json(res, 200, { secuses: true });
                                    }
                                }
                            );
                        }
                    );
                });
            });
            return;
        }

        res.writeHead(404);
        res.end();
    }
};
