const db = require('./database');

const CONFIG = {
    windowMs: 15 * 60 * 1000,      // 15 minute window
    maxAttempts: 10,               // Max failed attempts per IP
    blockDurationMs: 15 * 60 * 1000 // 15 minute cooldown
};

/**
 * Checks if IP is rate limited.
 * @returns {Promise<{blocked: boolean, retryAfter?: number}>}
 */
function checkIpRateLimit(ip) {
    return new Promise((resolve) => {
        const now = Date.now();
        
        db.get(
            "SELECT * FROM ip_rate_limits WHERE ip = ?",
            [ip],
            (err, row) => {
                if (err || !row) {
                    return resolve({ blocked: false });
                }
                
                // Check if currently blocked
                if (row.blocked_until && row.blocked_until > now) {
                    const retryAfter = Math.ceil((row.blocked_until - now) / 1000);
                    return resolve({ blocked: true, retryAfter });
                }
                
                // Check if window expired (reset if so)
                if (now - row.first_attempt_at > CONFIG.windowMs) {
                    db.run("DELETE FROM ip_rate_limits WHERE ip = ?", [ip]);
                    return resolve({ blocked: false });
                }
                
                resolve({ blocked: false, attempts: row.attempts });
            }
        );
    });
}

/**
 * Records a failed attempt for an IP.
 * @returns {Promise<{blocked: boolean, attemptsLeft: number, retryAfter?: number}>}
 */
function recordFailedAttempt(ip) {
    return new Promise((resolve) => {
        const now = Date.now();
        
        db.get(
            "SELECT * FROM ip_rate_limits WHERE ip = ?",
            [ip],
            (err, row) => {
                if (err) {
                    return resolve({ blocked: false, attemptsLeft: CONFIG.maxAttempts });
                }
                
                let attempts, firstAttemptAt, blockedUntil = null;
                
                if (!row || (now - row.first_attempt_at > CONFIG.windowMs)) {
                    // New window
                    attempts = 1;
                    firstAttemptAt = now;
                } else {
                    // Continue existing window
                    attempts = row.attempts + 1;
                    firstAttemptAt = row.first_attempt_at;
                }
                
                // Check if should block
                if (attempts >= CONFIG.maxAttempts) {
                    blockedUntil = now + CONFIG.blockDurationMs;
                }
                
                const sql = row
                    ? `UPDATE ip_rate_limits SET attempts=?, blocked_until=? WHERE ip=?`
                    : `INSERT INTO ip_rate_limits (attempts, first_attempt_at, blocked_until, ip) VALUES (?,?,?,?)`;
                
                const params = row
                    ? [attempts, blockedUntil, ip]
                    : [attempts, firstAttemptAt, blockedUntil, ip];
                
                db.run(sql, params, () => {
                    const attemptsLeft = Math.max(0, CONFIG.maxAttempts - attempts);
                    const retryAfter = blockedUntil 
                        ? Math.ceil(CONFIG.blockDurationMs / 1000) 
                        : undefined;
                    
                    resolve({ 
                        blocked: !!blockedUntil, 
                        attemptsLeft,
                        retryAfter 
                    });
                });
            }
        );
    });
}

/**
 * Clears rate limit for an IP.
 */
function clearIpRateLimit(ip) {
    db.run("DELETE FROM ip_rate_limits WHERE ip = ?", [ip]);
}

/**
 * Cleanup expired entries.
 */
function cleanupExpiredEntries() {
    const now = Date.now();
    db.run(
        "DELETE FROM ip_rate_limits WHERE blocked_until IS NOT NULL AND blocked_until < ?",
        [now]
    );
}

module.exports = {
    checkIpRateLimit,
    recordFailedAttempt,
    clearIpRateLimit,
    cleanupExpiredEntries,
    CONFIG
};
