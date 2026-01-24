const db = require('../database');

module.exports = {
    login: (req, res) => {
        db.get("SELECT value FROM settings WHERE key = 'admin_password'", (err, row) => {
            if (err) {
                res.writeHead(500, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ error: err.message }));
                return;
            }
            const password = row ? row.value : 'admin';
            if (req.body.password === password) {
                res.setHeader('Set-Cookie', 'auth_token=valid_token; Path=/; HttpOnly');
                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ success: true }));
            } else {
                res.writeHead(401, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ success: false }));
            }
        });
    },
    logout: (req, res) => {
        res.setHeader('Set-Cookie', 'auth_token=; Path=/; HttpOnly; Max-Age=0');
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ success: true }));
    }
};
