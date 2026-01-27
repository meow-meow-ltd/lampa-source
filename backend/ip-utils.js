/**
 * Safely extracts client IP from request.
 * Handles X-Forwarded-For (comma-separated list from proxies).
 * 
 * @param {Request} req - Express request object
 * @returns {string} Client IP address
 */
function getClientIp(req) {
  // Cloudflare: best source of the real client IP
  const cfIp = req.headers['cf-connecting-ip'];
  if (cfIp) return String(cfIp).trim();

  // Fallback: X-Forwarded-For (can be spoofed if untrusted clients can reach your app)
  const xff = req.headers['x-forwarded-for'];
  if (xff) return String(xff).split(',')[0].trim();

  // Last resort: socket address (will usually be cloudflared/local)
  return (
    req.socket?.remoteAddress ||
    req.connection?.remoteAddress ||
    req.ip ||
    'unknown'
  );
}

module.exports = { getClientIp };