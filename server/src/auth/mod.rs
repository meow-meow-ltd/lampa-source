//! Authentication C (T10): TOTP PIN verification, device/admin cookies, the
//! session bootstrap endpoint, PIN rate limits and the media-auth policy hook.
//!
//! Design (plan §5, `docs/decisions.md` D6, `review/inv/S8-auth.md`):
//!
//! * **TOTP** — SHA-1, 6 digits, 30 s step, `window = 1` (speakeasy-compatible).
//!   The shared secret is read from `TOTP_SECRET` (base32) and is redacted from
//!   `Config`'s `Debug`; verified codes are compared in constant time.
//! * **Device tokens** — every successful PIN verify mints a fresh 32-byte
//!   token (64 lowercase hex chars) and stores its lowercase SHA-256 hex digest
//!   in `device_access.token_hash` (byte-compatible with the Node backend).
//!   The previous token is invalidated by the update.
//! * **Cookies (transitional C)** — the PIN response sets `lampa_session`
//!   (HttpOnly) and, while `DEVICE_LEGACY_COOKIE=1`, the readable legacy
//!   `device_token` with the same value. `device_token` is **never** HttpOnly
//!   (the client JS still owns it during C → B); `DEVICE_LEGACY_COOKIE=0`
//!   switches the server to B-strict (session cookie only).
//! * **Guard** — device routes need an `allowed` `device_access` row matching
//!   the SHA-256 of the presented cookie; admin routes compare `auth_token`
//!   against `settings.admin_token` in constant time. Missing credential →
//!   `401 {"error":"token_required"}`, blank/unknown/revoked →
//!   `401 {"error":"invalid_token"}`. Blocked devices never get a 403 (the PIN
//!   gate must stay reachable).
//! * **Rate limits** — `ip_rate_limits`: 10 failed attempts / 15 min → blocked
//!   for 15 min, checked before TOTP; a device is blocked after 5 failed PIN
//!   attempts.
//! * **`last_seen_at`** — updated at most once per 30 s per device.
//! * **Media auth** — [`media`] is the policy hook W3 wires into the media
//!   routes; it is unit-tested now and denies anything that is not an allowed
//!   device session.
//!
//! No token, TOTP code or cookie value is ever logged; only non-secret status
//! fields (IP, device id, counters) are.

pub mod client_ip;
pub mod constant_time;
pub mod cookie;
pub mod device;
pub mod media;
pub mod rate_limit;
pub mod service;
pub mod totp;

pub use cookie::CookieValue;
pub use service::{
    AdminAuth, AuthService, DeviceAuth, PinOutcome, PinRequest, RequestMeta, SessionOutcome,
};

/// `last_seen_at` write throttle (plan §5.3: at most once per 30 s per device).
pub const LAST_SEEN_THROTTLE_MS: i64 = 30_000;

/// Current wall clock in milliseconds since the Unix epoch (legacy `Date.now()`).
pub fn now_millis() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_positive_and_the_throttle_is_thirty_seconds() {
        assert!(now_millis() > 0);
        assert_eq!(LAST_SEEN_THROTTLE_MS, 30_000);
    }
}
