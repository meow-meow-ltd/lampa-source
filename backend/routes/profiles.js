const db = require('../database');

function now() {
    return Date.now();
}

function mapProfile(row) {
    return {
        id: row.id,
        name: row.name,
        icon: row.icon || 'l_1',
        main: !!row.is_main,
        child: !!row.is_child,
        sort: row.sort_order || 0
    };
}

function ensureDefaultProfile(callback) {
    db.get("SELECT COUNT(1) as cnt FROM profiles", (err, row) => {
        if (err) return callback(err);

        if ((row?.cnt || 0) > 0) return callback(null);

        const t = now();
        db.get("SELECT MAX(sort_order) as max_sort FROM profiles", [], (err, rowMax) => {
            if (err) return callback(err);

            const nextSort = (rowMax?.max_sort || 0) + 1;
            db.run(
                "INSERT INTO profiles(name, icon, is_main, is_child, sort_order, created_at, updated_at) VALUES(?, ?, 1, 0, ?, ?, ?)",
                ['Main', 'l_1', nextSort, t, t],
                callback
            );
        });
    });
}

module.exports = {
    handle: (req, res) => {
        if (req.url === '/api/profiles/all' && req.method === 'GET') {
            return ensureDefaultProfile((err) => {
                if (err) {
                    res.writeHead(500, { 'Content-Type': 'application/json' });
                    return res.end(JSON.stringify({ secuses: false, error: err.message }));
                }

                db.all("SELECT * FROM profiles ORDER BY is_main DESC, sort_order ASC, id ASC", [], (err, rows) => {
                    if (err) {
                        res.writeHead(500, { 'Content-Type': 'application/json' });
                        return res.end(JSON.stringify({ secuses: false, error: err.message }));
                    }

                    res.writeHead(200, { 'Content-Type': 'application/json' });
                    res.end(JSON.stringify({ secuses: true, profiles: rows.map(mapProfile) }));
                });
            });
        }

        if (req.url === '/api/profiles/create' && req.method === 'POST') {
            const name = (req.body?.name || '').trim();
            const icon = typeof req.body?.icon === 'string' ? req.body.icon.trim() : 'l_1';
            const sort = Number.isFinite(parseInt(req.body?.sort)) ? parseInt(req.body.sort) : null;

            if (!name) {
                res.writeHead(400, { 'Content-Type': 'application/json' });
                return res.end(JSON.stringify({ secuses: false, text: 'Name is required' }));
            }

            const t = now();
            const insertProfile = (sortOrder) => {
                db.run(
                    "INSERT INTO profiles(name, icon, is_main, is_child, sort_order, created_at, updated_at) VALUES(?, ?, 0, 0, ?, ?, ?)",
                    [name, icon || 'l_1', sortOrder, t, t],
                    function (err) {
                        if (err) {
                            res.writeHead(500, { 'Content-Type': 'application/json' });
                            return res.end(JSON.stringify({ secuses: false, error: err.message }));
                        }

                        db.get("SELECT * FROM profiles WHERE id = ?", [this.lastID], (err, row) => {
                            if (err) {
                                res.writeHead(500, { 'Content-Type': 'application/json' });
                                return res.end(JSON.stringify({ secuses: false, error: err.message }));
                            }

                            res.writeHead(200, { 'Content-Type': 'application/json' });
                            res.end(JSON.stringify({ secuses: true, profile: mapProfile(row) }));
                        });
                    }
                );
            };

            if (sort !== null) {
                insertProfile(sort);
            } else {
                db.get("SELECT MAX(sort_order) as max_sort FROM profiles", [], (err, rowMax) => {
                    if (err) {
                        res.writeHead(500, { 'Content-Type': 'application/json' });
                        return res.end(JSON.stringify({ secuses: false, error: err.message }));
                    }

                    const nextSort = (rowMax?.max_sort || 0) + 1;
                    insertProfile(nextSort);
                });
            }
            return;
        }

        if (req.url === '/api/profiles/update' && req.method === 'POST') {
            const id = parseInt(req.body?.id);
            const name = typeof req.body?.name === 'string' ? req.body.name.trim() : undefined;
            const icon = typeof req.body?.icon === 'string' ? req.body.icon.trim() : undefined;
            const is_child = typeof req.body?.child === 'boolean' ? (req.body.child ? 1 : 0) : undefined;
            const sort = Number.isFinite(parseInt(req.body?.sort)) ? parseInt(req.body.sort) : undefined;

            if (!id) {
                res.writeHead(400, { 'Content-Type': 'application/json' });
                return res.end(JSON.stringify({ secuses: false, text: 'Invalid id' }));
            }

            const fields = [];
            const values = [];

            if (name) {
                fields.push('name=?');
                values.push(name);
            }
            if (icon) {
                fields.push('icon=?');
                values.push(icon);
            }
            if (typeof is_child !== 'undefined') {
                fields.push('is_child=?');
                values.push(is_child);
            }
            if (typeof sort !== 'undefined') {
                fields.push('sort_order=?');
                values.push(sort);
            }

            if (!fields.length) {
                res.writeHead(400, { 'Content-Type': 'application/json' });
                return res.end(JSON.stringify({ secuses: false, text: 'No fields to update' }));
            }

            fields.push('updated_at=?');
            values.push(now());
            values.push(id);

            db.run(`UPDATE profiles SET ${fields.join(', ')} WHERE id=?`, values, function (err) {
                if (err) {
                    res.writeHead(500, { 'Content-Type': 'application/json' });
                    return res.end(JSON.stringify({ secuses: false, error: err.message }));
                }

                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ secuses: true }));
            });
            return;
        }

        if (req.url === '/api/profiles/delete' && req.method === 'POST') {
            const id = parseInt(req.body?.id);

            if (!id) {
                res.writeHead(400, { 'Content-Type': 'application/json' });
                return res.end(JSON.stringify({ secuses: false, text: 'Invalid id' }));
            }

            db.run("DELETE FROM profiles WHERE id=?", [id], function (err) {
                if (err) {
                    res.writeHead(500, { 'Content-Type': 'application/json' });
                    return res.end(JSON.stringify({ secuses: false, error: err.message }));
                }

                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ secuses: true }));
            });
            return;
        }

        res.writeHead(404);
        res.end();
    }
};
