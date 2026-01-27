const db = require('../database');
const crypto = require('crypto');
const speakeasy = require('speakeasy');
const { getClientIp } = require('../ip-utils');
const { checkIpRateLimit, recordFailedAttempt, clearIpRateLimit } = require('../rate-limit');

module.exports = {
    verify: async (req, res) => {
        const { pin, device_id } = req.body;
        const ip = getClientIp(req);
        const ua = req.headers['user-agent'];

        // 0. Check IP rate limit FIRST
        const ipStatus = await checkIpRateLimit(ip);
        if (ipStatus.blocked) {
            console.log('[PIN]', { action: 'verify', ip, status: 'ip_blocked', retry_after: ipStatus.retryAfter });
            res.writeHead(429, { 
                'Content-Type': 'application/json',
                'Retry-After': ipStatus.retryAfter 
            });
            return res.end(JSON.stringify({ 
                ok: false,
                error: 'too_many_attempts', 
                retry_after: ipStatus.retryAfter 
            }));
        }

        if (!device_id) {
            res.writeHead(400, { 'Content-Type': 'application/json' });
            return res.end(JSON.stringify({ error: 'device_id required' }));
        }

        const totpSecret = process.env.TOTP_SECRET;

        if (!totpSecret) {
            console.error('[PIN] TOTP_SECRET environment variable is not set!');
            res.writeHead(500, { 'Content-Type': 'application/json' });
            return res.end(JSON.stringify({ error: 'Server configuration error' }));
        }

        // 1. Get device status
        db.get("SELECT * FROM device_access WHERE device_id = ?", [device_id], (err, device) => {
            const now = Date.now();

            // 2. Check if blocked
            if (device && device.status === 'blocked') {
                res.writeHead(200, { 'Content-Type': 'application/json' });
                return res.end(JSON.stringify({ 
                    ok: false, 
                    blocked: true
                }));
            }

            // 3. Verify TOTP
            const isMatch = speakeasy.totp.verify({
                secret: totpSecret,
                encoding: 'base32',
                token: pin,
                window: 1 // Allow ±30 seconds tolerance
            });

            if (isMatch) {
                // Success logic
                const token = crypto.randomBytes(32).toString('hex');
                const tokenHash = crypto.createHash('sha256').update(token).digest('hex');

                const sql = device 
                    ? `UPDATE device_access SET status='allowed', attempts=0, last_attempt_at=?, last_seen_at=?, token_hash=?, ip=?, user_agent=?, blocked_until=NULL WHERE device_id=?`
                    : `INSERT INTO device_access (last_attempt_at, last_seen_at, token_hash, ip, user_agent, device_id, status, attempts) VALUES (?,?,?,?,?,?,'allowed',0)`;
                
                const params = device 
                    ? [now, now, tokenHash, ip, ua, device_id]
                    : [now, now, tokenHash, ip, ua, device_id];

                db.run(sql, params, (err) => {
                    clearIpRateLimit(ip); // Reset IP rate limit on success
                    console.log('[PIN]', { action: 'verify', device_id, ip, status: 'success' });
                    res.writeHead(200, { 'Content-Type': 'application/json' });
                    res.end(JSON.stringify({ ok: true, device_token: token }));
                });
            } else {
                // Failure logic
                const attempts = (device ? device.attempts : 0) + 1;
                const maxAttempts = 5;
                let status = 'pending';

                if (attempts >= maxAttempts) {
                    status = 'blocked';
                }

                const sql = device
                    ? `UPDATE device_access SET attempts=?, last_attempt_at=?, status=?, ip=?, user_agent=? WHERE device_id=?`
                    : `INSERT INTO device_access (attempts, last_attempt_at, status, ip, user_agent, device_id) VALUES (?,?,?,?,?,?)`;

                const params = device
                    ? [attempts, now, status, ip, ua, device_id]
                    : [attempts, now, status, ip, ua, device_id];

                db.run(sql, params, (err) => {
                    // Record failed attempt for IP-based rate limiting
                    recordFailedAttempt(ip).then(ipResult => {
                        const attempts_left = Math.max(0, maxAttempts - attempts);
                        console.log('[PIN]', { 
                            action: 'verify', 
                            device_id, 
                            ip, 
                            status: status === 'blocked' ? 'blocked' : 'fail', 
                            attempts_left,
                            ip_attempts_left: ipResult.attemptsLeft
                        });
                        
                        res.writeHead(200, { 'Content-Type': 'application/json' });
                        res.end(JSON.stringify({ 
                            ok: false, 
                            attempts_left, 
                            blocked: status === 'blocked',
                            ip_blocked: ipResult.blocked,
                            retry_after: ipResult.retryAfter
                        }));
                    });
                });
            }
        });
    }
};
