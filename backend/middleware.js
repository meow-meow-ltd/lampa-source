const bodyParser = require('body-parser');
const cookieParser = require('cookie-parser');
const authParams = require('./routes/auth');
const settingsParams = require('./routes/settings');
const adminParams = require('./routes/admin');
const dataParams = require('./routes/data');
const profilesParams = require('./routes/profiles');
const bookmarksParams = require('./routes/bookmarks');
const timelineParams = require('./routes/timeline');
const backupParams = require('./routes/backup');
const profileSettingsParams = require('./routes/profile_settings');
const pinParams = require('./routes/pin');
const db = require('./database');
const crypto = require('crypto');
const { getClientIp } = require('./ip-utils');

module.exports = function(req, res, next) {
    cookieParser()(req, res, () => {
        bodyParser.urlencoded({ extended: true })(req, res, () => {
            bodyParser.json()(req, res, () => {
                const url = req.url;
                const method = req.method;

                // 1. PIN Verification Route
                if (url === '/api/pin/verify' && method === 'POST') {
                    return pinParams.verify(req, res);
                }

                // 2. Global Request Guard for /api/
                if (url.startsWith('/api/') && !url.startsWith('/api/pin/verify')) {
                    // Check for Admin bypass first
                    const isAdminRoute = url.startsWith('/api/admin/');
                    
                    db.get("SELECT value FROM settings WHERE key = ?", ['admin_token'], (err, tokenRow) => {
                        const validAdminToken = tokenRow?.value;
                        const hasAdminAuth = req.cookies['auth_token'] === validAdminToken && validAdminToken;

                        // Allow if it's an admin route (they have their own check) or if we have a valid admin cookie
                        if (isAdminRoute || hasAdminAuth) {
                            return handleRoutes(req, res, next);
                        }

                        const token = req.cookies['device_token'];
                        
                        if (!token) {
                            console.log('[GUARD] Rejected: missing token', { url, ip: getClientIp(req) });
                            res.writeHead(401, { 'Content-Type': 'application/json' });
                            return res.end(JSON.stringify({ error: 'token_required' }));
                        }

                        const tokenHash = crypto.createHash('sha256').update(token).digest('hex');

                        db.get("SELECT * FROM device_access WHERE token_hash = ?", [tokenHash], (err, device) => {
                            if (err || !device || device.status !== 'allowed') {
                                console.log('[GUARD] Rejected: invalid/blocked token', { 
                                    url, 
                                    ip: getClientIp(req),
                                    status: device ? device.status : 'no_device' 
                                });
                                res.writeHead(401, { 'Content-Type': 'application/json' });
                                return res.end(JSON.stringify({ error: 'invalid_token' }));
                            }

                            // Update last seen
                            db.run("UPDATE device_access SET last_seen_at = ? WHERE id = ?", [Date.now(), device.id]);

                            handleRoutes(req, res, next);
                        });
                    });
                } else {
                    handleRoutes(req, res, next);
                }
            });
        });
    });
};

function handleRoutes(req, res, next) {
    if (req.url === '/admin/login' && req.method === 'POST') {
        authParams.login(req, res);
    } else if (req.url === '/admin/logout' && req.method === 'POST') {
        authParams.logout(req, res);
    } else if (req.url === '/admin') {
        adminParams.serve(req, res, next);
    } else if (req.url.startsWith('/api/admin/devices') || req.url.startsWith('/api/admin/device/action')) {
        adminParams.handleDevices(req, res);
    } else if (req.url.startsWith('/api/settings')) {
        settingsParams.handle(req, res);
    } else if (req.url.startsWith('/api/values') || req.url.startsWith('/api/value')) {
        dataParams.handle(req, res);
    } else if (req.url.startsWith('/api/profiles')) {
        profilesParams.handle(req, res);
    } else if (req.url.startsWith('/api/profile/settings')) {
        profileSettingsParams.handle(req, res);
    } else if (req.url.startsWith('/api/bookmarks')) {
        bookmarksParams.handle(req, res);
    } else if (req.url.startsWith('/api/timeline')) {
        timelineParams.handle(req, res);
    } else if (req.url.startsWith('/api/users/backup')) {
        backupParams.handle(req, res);
    } else {
        next();
    }
}
