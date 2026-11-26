//! Authentication service: the single place that owns PIN, device, admin and
//! session semantics. HTTP handlers/middleware only translate its outcomes into
//! responses, which keeps the legacy rules unit-testable against temp databases.

use std::fmt;
use std::sync::Arc;

use sqlx::sqlite::SqlitePool;

use crate::auth::constant_time;
use crate::auth::cookie::CookieValue;
use crate::auth::device;
use crate::auth::media::{self, DeviceStatus, MediaDecision, MediaSubject};
use crate::auth::rate_limit;
use crate::auth::totp::{TotpConfigError, TotpVerifier};
use crate::config::Config;
use crate::db::Database;

/// Request facts needed by the auth service (never logged as a whole).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMeta {
    /// Resolved client IP (rate-limit key).
    pub ip: String,
    /// Optional legacy `User-Agent` column value.
    pub user_agent: Option<String>,
}

/// Body fields of `POST /api/pin/verify`.
///
/// Deliberately does **not** implement `Debug`: it carries the TOTP code.
#[derive(Clone, PartialEq, Eq)]
pub struct PinRequest {
    /// Submitted TOTP code (missing = failed attempt, like Node's `undefined`).
    pub pin: Option<String>,
    /// Client-generated legacy device identifier.
    pub device_id: Option<String>,
}

/// Outcome of one `POST /api/pin/verify` call.
#[derive(Clone, PartialEq, Eq)]
pub enum PinOutcome {
    /// IP is blocked before TOTP (HTTP 429 + `Retry-After`).
    IpBlocked {
        /// Whole seconds until the block expires.
        retry_after_secs: i64,
    },
    /// No `device_id` in the body (HTTP 400, Node parity).
    MissingDeviceId,
    /// `TOTP_SECRET` unset/invalid (HTTP 500 "Server configuration error").
    ServerConfig,
    /// Device is blocked: HTTP 200 `{ok:false,blocked:true}`, no TOTP check.
    DeviceBlocked,
    /// PIN accepted; the caller must return `{ok:true,device_token}` and C cookies.
    Success {
        /// Fresh 64-hex device token.
        device_token: String,
    },
    /// PIN rejected; counters and block flags mirror the legacy response.
    Failure {
        /// Attempts left before the device block.
        attempts_left: i64,
        /// Whether this attempt blocked the device.
        blocked: bool,
        /// Whether this attempt blocked the IP.
        ip_blocked: bool,
        /// `Retry-After` seconds when the IP is blocked.
        retry_after_secs: Option<i64>,
    },
    /// Database guard failed at startup / no database handle.
    Unavailable,
    /// A database query failed (sanitised 500 response).
    DatabaseError,
}

impl fmt::Debug for PinOutcome {
    // Never print the freshly minted device token.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PinOutcome::IpBlocked { retry_after_secs } => formatter
                .debug_struct("PinOutcome::IpBlocked")
                .field("retry_after_secs", retry_after_secs)
                .finish(),
            PinOutcome::MissingDeviceId => formatter.write_str("PinOutcome::MissingDeviceId"),
            PinOutcome::ServerConfig => formatter.write_str("PinOutcome::ServerConfig"),
            PinOutcome::DeviceBlocked => formatter.write_str("PinOutcome::DeviceBlocked"),
            PinOutcome::Success { .. } => formatter
                .debug_struct("PinOutcome::Success")
                .field("device_token", &"***")
                .finish(),
            PinOutcome::Failure {
                attempts_left,
                blocked,
                ip_blocked,
                retry_after_secs,
            } => formatter
                .debug_struct("PinOutcome::Failure")
                .field("attempts_left", attempts_left)
                .field("blocked", blocked)
                .field("ip_blocked", ip_blocked)
                .field("retry_after_secs", retry_after_secs)
                .finish(),
            PinOutcome::Unavailable => formatter.write_str("PinOutcome::Unavailable"),
            PinOutcome::DatabaseError => formatter.write_str("PinOutcome::DatabaseError"),
        }
    }
}

/// One authenticated device, attached to the request by the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedDevice {
    /// `device_access.id`.
    #[allow(dead_code)] // consumed by T07/T24 handlers/media auth (W2/W3).
    pub row_id: i64,
    /// Legacy device identifier.
    pub device_id: String,
}

/// Device-cookie guard decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceAuth {
    /// Matching `device_access` row with `status = 'allowed'`.
    Allowed(AuthenticatedDevice),
    /// No device cookie at all (`token_required`).
    Missing,
    /// Cookie present but blank (`invalid_token`).
    Empty,
    /// Unknown digest or non-allowed status (`invalid_token`, never 403).
    Invalid,
    /// Startup database guard failed (`invalid_token`, fail closed).
    Unavailable,
    /// Query failure while the database is ready (sanitised 500).
    Error,
}

/// Admin-cookie guard decision (`auth_token` vs `settings.admin_token`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminAuth {
    /// Cookie equals the stored token (constant-time).
    Allowed,
    /// No `auth_token` cookie (`token_required`).
    Missing,
    /// Cookie present but blank (`invalid_token`).
    Empty,
    /// Cookie differs, or no usable stored token (`invalid_token`).
    Invalid,
    /// Startup database guard failed (fail closed).
    Unavailable,
    /// Query failure while the database is ready (sanitised 500).
    Error,
}

/// `GET /api/auth/session` outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    /// Device session (with `admin` reporting an additional valid admin cookie).
    Device {
        /// Paired device identifier.
        device_id: String,
        /// Whether the request also carries a valid `auth_token`.
        admin: bool,
    },
    /// Admin session without a device cookie.
    Admin,
    /// No credential presented (`token_required`).
    Missing,
    /// Credential presented but not valid (`invalid_token`).
    Invalid,
    /// Database guard failed at startup.
    Unavailable,
    /// Query failure while the database is ready.
    DatabaseError,
}

/// Authentication service bound to one configuration and database.
#[derive(Debug, Clone)]
pub struct AuthService {
    config: Arc<Config>,
    database: Option<Database>,
}

impl AuthService {
    /// Builds the service; `database` is `None` when the startup guard failed.
    pub fn new(config: Arc<Config>, database: Option<Database>) -> Self {
        Self { config, database }
    }

    /// Whether the transitional readable `device_token` cookie is still issued
    /// and accepted (`DEVICE_LEGACY_COOKIE=1`, the W1 default). `0` = B-strict.
    pub fn legacy_cookie_enabled(&self) -> bool {
        self.config.device_legacy_cookie()
    }

    fn pool(&self) -> Option<&SqlitePool> {
        self.database.as_ref().map(Database::pool)
    }

    /// Configured verifier (TOTP secret missing/invalid → server config error).
    pub fn totp_verifier(&self) -> Result<TotpVerifier, TotpConfigError> {
        match self.config.totp_secret() {
            Some(secret) => TotpVerifier::from_base32(secret),
            None => Err(TotpConfigError::MissingSecret),
        }
    }

    /// `POST /api/pin/verify` semantics, in the legacy order:
    /// IP limit → `device_id` → TOTP config → blocked device → TOTP → update.
    pub async fn verify_pin(
        &self,
        request: &PinRequest,
        meta: &RequestMeta,
        now_ms: i64,
    ) -> PinOutcome {
        let Some(pool) = self.pool() else {
            return PinOutcome::Unavailable;
        };

        // 0. IP rate limit first (legacy pin.js:13-26).
        let ip_status = match rate_limit::check(pool, &meta.ip, now_ms).await {
            Ok(status) => status,
            Err(_) => return PinOutcome::DatabaseError,
        };
        if ip_status.blocked {
            let retry_after_secs = ip_status.retry_after.unwrap_or(rate_limit::BLOCK_MS / 1000);
            tracing::info!(
                action = "verify",
                ip = %meta.ip,
                status = "ip_blocked",
                retry_after = retry_after_secs,
                "[PIN]"
            );
            return PinOutcome::IpBlocked { retry_after_secs };
        }

        // 1. device_id is required before any TOTP work (legacy 400).
        let Some(device_id) = request
            .device_id
            .as_deref()
            .map(str::trim)
            .filter(|device_id| !device_id.is_empty())
            .map(str::to_owned)
        else {
            return PinOutcome::MissingDeviceId;
        };

        // 2. TOTP configuration (legacy 500 "Server configuration error").
        let Ok(verifier) = self.totp_verifier() else {
            return PinOutcome::ServerConfig;
        };

        // 3. Device status; a blocked device is answered before TOTP.
        let existing = match device::find_by_device_id(pool, &device_id).await {
            Ok(row) => row,
            Err(_) => return PinOutcome::DatabaseError,
        };
        if existing
            .as_ref()
            .is_some_and(|row| row.status == device::STATUS_BLOCKED)
        {
            tracing::info!(
                action = "verify",
                device = %device_id,
                status = "blocked",
                "[PIN]"
            );
            return PinOutcome::DeviceBlocked;
        }

        // 4. TOTP comparison.
        let pin = request.pin.as_deref().unwrap_or("");
        let now_seconds = u64::try_from(now_ms.max(0) / 1000).unwrap_or(0);
        if verifier.verify(pin, now_seconds) {
            let token = device::generate_token();
            let token_hash = device::hash_token(&token);
            if device::record_success(
                pool,
                &device_id,
                existing.as_ref(),
                &token_hash,
                &meta.ip,
                meta.user_agent.as_deref(),
                now_ms,
            )
            .await
            .is_err()
            {
                return PinOutcome::DatabaseError;
            }
            // Legacy clears the IP window on success; a failed cleanup must not
            // turn a valid PIN into an error.
            let _ = rate_limit::clear(pool, &meta.ip).await;
            tracing::info!(
                action = "verify",
                device = %device_id,
                status = "success",
                "[PIN]"
            );
            PinOutcome::Success {
                device_token: token,
            }
        } else {
            let record = match device::record_failure(
                pool,
                &device_id,
                existing.as_ref(),
                &meta.ip,
                meta.user_agent.as_deref(),
                now_ms,
            )
            .await
            {
                Ok(record) => record,
                Err(_) => return PinOutcome::DatabaseError,
            };
            let ip_record = match rate_limit::record_failure(pool, &meta.ip, now_ms).await {
                Ok(record) => record,
                Err(_) => return PinOutcome::DatabaseError,
            };
            tracing::info!(
                action = "verify",
                device = %device_id,
                status = if record.blocked { "blocked" } else { "fail" },
                attempts_left = (device::MAX_FAILED_ATTEMPTS - record.attempts).max(0),
                ip_attempts_left = ip_record.attempts_left,
                "[PIN]"
            );
            PinOutcome::Failure {
                attempts_left: (device::MAX_FAILED_ATTEMPTS - record.attempts).max(0),
                blocked: record.blocked,
                ip_blocked: ip_record.blocked,
                retry_after_secs: ip_record.retry_after,
            }
        }
    }

    /// Validates a device cookie against `device_access` and throttles
    /// `last_seen_at`. The cookie value itself is never logged or retained.
    pub async fn authenticate_device(&self, presented: CookieValue<'_>, now_ms: i64) -> DeviceAuth {
        match presented {
            CookieValue::Missing => DeviceAuth::Missing,
            CookieValue::Empty => DeviceAuth::Empty,
            CookieValue::Value(credential) => {
                let Some(pool) = self.pool() else {
                    return DeviceAuth::Unavailable;
                };
                let digest = device::hash_token(credential);
                let row = match device::find_by_token_hash(pool, &digest).await {
                    Ok(row) => row,
                    Err(_) => return DeviceAuth::Error,
                };
                match row {
                    Some(row) if row.status == device::STATUS_ALLOWED => {
                        if device::should_touch_last_seen(row.last_seen_at, now_ms)
                            && device::touch_last_seen(pool, row.id, now_ms).await.is_err()
                        {
                            tracing::debug!("last_seen update failed");
                        }
                        DeviceAuth::Allowed(AuthenticatedDevice {
                            row_id: row.id,
                            device_id: row.device_id,
                        })
                    }
                    _ => DeviceAuth::Invalid,
                }
            }
        }
    }

    /// Compares the `auth_token` cookie with `settings.admin_token` in constant
    /// time (legacy plaintext value, never masked or rewritten here).
    pub async fn authenticate_admin(&self, presented: CookieValue<'_>) -> AdminAuth {
        match presented {
            CookieValue::Missing => AdminAuth::Missing,
            CookieValue::Empty => AdminAuth::Empty,
            CookieValue::Value(credential) => {
                let Some(pool) = self.pool() else {
                    return AdminAuth::Unavailable;
                };
                match admin_token(pool).await {
                    Ok(Some(stored))
                        if constant_time::eq(credential.as_bytes(), stored.as_bytes()) =>
                    {
                        AdminAuth::Allowed
                    }
                    Ok(_) => AdminAuth::Invalid,
                    Err(_) => AdminAuth::Error,
                }
            }
        }
    }

    /// `GET /api/auth/session`: paired state for a valid device/admin cookie;
    /// deliberately does **not** write `last_seen_at` (S8 §8.3).
    pub async fn session(
        &self,
        device_cookie: CookieValue<'_>,
        admin_cookie: CookieValue<'_>,
    ) -> SessionOutcome {
        let Some(pool) = self.pool() else {
            return SessionOutcome::Unavailable;
        };

        let mut device_invalid = matches!(device_cookie, CookieValue::Empty);
        let mut device_id = None;
        if let CookieValue::Value(credential) = device_cookie {
            match device::find_by_token_hash(pool, &device::hash_token(credential)).await {
                Ok(Some(row)) if row.status == device::STATUS_ALLOWED => {
                    device_id = Some(row.device_id);
                }
                Ok(_) => device_invalid = true,
                Err(_) => return SessionOutcome::DatabaseError,
            }
        }

        let mut admin_invalid = matches!(admin_cookie, CookieValue::Empty);
        let mut admin_allowed = false;
        if let CookieValue::Value(credential) = admin_cookie {
            match admin_token(pool).await {
                Ok(Some(stored)) if constant_time::eq(credential.as_bytes(), stored.as_bytes()) => {
                    admin_allowed = true;
                }
                Ok(_) => admin_invalid = true,
                Err(_) => return SessionOutcome::DatabaseError,
            }
        }

        if let Some(device_id) = device_id {
            return SessionOutcome::Device {
                device_id,
                admin: admin_allowed,
            };
        }
        if admin_allowed {
            return SessionOutcome::Admin;
        }
        if device_invalid || admin_invalid {
            SessionOutcome::Invalid
        } else {
            SessionOutcome::Missing
        }
    }

    /// Media-auth bridge for W3 (T24): resolves the device cookie and applies
    /// [`media::authorize`]. Fail closed on every non-allowed outcome.
    #[allow(dead_code)] // W3 wires this into the media routes.
    pub async fn authorize_media(&self, presented: CookieValue<'_>, now_ms: i64) -> MediaDecision {
        match self.authenticate_device(presented, now_ms).await {
            DeviceAuth::Allowed(device) => media::authorize(Some(MediaSubject {
                device_id: device.device_id,
                status: DeviceStatus::Allowed,
            })),
            _ => media::authorize(None),
        }
    }
}

/// Raw `settings.admin_token` value (legacy plaintext; `None` when absent).
async fn admin_token(pool: &SqlitePool) -> Result<Option<String>, sqlx::Error> {
    use sqlx::Row;
    let row = sqlx::query("SELECT value FROM settings WHERE key = 'admin_token'")
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|row| row.get::<Option<String>, _>("value")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::cookie::CookieValue;
    use crate::auth::device::{hash_token, MAX_FAILED_ATTEMPTS};
    use crate::db::test_support::TempDb;

    const S8_SECRET: &str = "JBSWY3DPEHPK3PXP";
    const NOW_MS: i64 = 1_700_000_000_000;
    const ADMIN_TOKEN: &str = "admin-token-for-tests";

    fn config(secret: Option<&str>, legacy: bool) -> Arc<Config> {
        let secret = secret.map(str::to_owned);
        let legacy = if legacy { "1" } else { "0" }.to_owned();
        Arc::new(
            Config::from_lookup(move |key| match key {
                "TOTP_SECRET" => secret.clone(),
                "DEVICE_LEGACY_COOKIE" => Some(legacy.clone()),
                _ => None,
            })
            .expect("test configuration"),
        )
    }

    async fn fixture(secret: Option<&str>, legacy: bool) -> (TempDb, AuthService) {
        let temp = TempDb::new().await;
        let database = temp.open().await.expect("v7 fixture opens");
        let service = AuthService::new(config(secret, legacy), Some(database));
        (temp, service)
    }

    fn service_pool(service: &AuthService) -> &SqlitePool {
        service.database.as_ref().expect("ready db").pool()
    }

    fn meta(ip: &str) -> RequestMeta {
        RequestMeta {
            ip: ip.to_owned(),
            user_agent: Some("lampa-test/1.0".to_owned()),
        }
    }

    fn valid_pin(service: &AuthService, now_ms: i64) -> String {
        service
            .totp_verifier()
            .expect("configured secret")
            .generate_at(u64::try_from(now_ms / 1000).unwrap_or(0))
    }

    fn wrong_pin(valid: &str) -> String {
        if valid == "000000" {
            "111111".to_owned()
        } else {
            "000000".to_owned()
        }
    }

    fn pin_request(device_id: &str, pin: &str) -> PinRequest {
        PinRequest {
            pin: Some(pin.to_owned()),
            device_id: Some(device_id.to_owned()),
        }
    }

    async fn seed_allowed_token(service: &AuthService, device_id: &str, token: &str) {
        sqlx::query(
            "INSERT INTO device_access(device_id, status, attempts, token_hash, last_seen_at) \
             VALUES(?, 'allowed', 0, ?, ?)",
        )
        .bind(device_id)
        .bind(hash_token(token))
        .bind(NOW_MS)
        .execute(service_pool(service))
        .await
        .expect("seed device");
    }

    async fn seed_admin_token(service: &AuthService, token: &str) {
        sqlx::query("INSERT INTO settings(key, value, visible) VALUES('admin_token', ?, 0)")
            .bind(token)
            .execute(service_pool(service))
            .await
            .expect("seed admin token");
    }

    #[tokio::test]
    async fn successful_pin_issues_a_fresh_hashed_token() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        let pin = valid_pin(&service, NOW_MS);
        let outcome = service
            .verify_pin(&pin_request("device-a", &pin), &meta("203.0.113.9"), NOW_MS)
            .await;
        let PinOutcome::Success { device_token } = outcome else {
            panic!("expected success, got {outcome:?}");
        };
        assert_eq!(device_token.len(), 64);
        assert!(device_token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(device_token, device_token.to_ascii_lowercase());

        let stored: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM device_access WHERE device_id = 'device-a'")
                .fetch_one(service_pool(&service))
                .await
                .expect("read hash");
        assert_eq!(stored.as_deref(), Some(hash_token(&device_token).as_str()));
        assert_eq!(stored.as_deref().map(str::len), Some(64));
        assert_eq!(
            stored.as_deref(),
            Some(hash_token(&device_token).to_ascii_lowercase().as_str())
        );

        let row = device::find_by_device_id(service_pool(&service), "device-a")
            .await
            .expect("read row")
            .expect("row");
        assert_eq!(row.status, device::STATUS_ALLOWED);
        assert_eq!(row.attempts, 0);
        assert_eq!(row.last_seen_at, Some(NOW_MS));
    }

    #[tokio::test]
    async fn every_success_rotates_the_token_and_invalidates_the_previous() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        let pin = valid_pin(&service, NOW_MS);
        let first = match service
            .verify_pin(&pin_request("device-b", &pin), &meta("203.0.113.9"), NOW_MS)
            .await
        {
            PinOutcome::Success { device_token } => device_token,
            other => panic!("first verify failed: {other:?}"),
        };
        let second = match service
            .verify_pin(
                &pin_request("device-b", &pin),
                &meta("203.0.113.9"),
                NOW_MS + 1,
            )
            .await
        {
            PinOutcome::Success { device_token } => device_token,
            other => panic!("second verify failed: {other:?}"),
        };
        assert_ne!(first, second, "a new token is minted on every verify");
        assert_eq!(
            service
                .authenticate_device(CookieValue::Value(&first), NOW_MS + 2)
                .await,
            DeviceAuth::Invalid,
            "the previous token must stop working"
        );
        assert!(matches!(
            service
                .authenticate_device(CookieValue::Value(&second), NOW_MS + 2)
                .await,
            DeviceAuth::Allowed(_)
        ));
    }

    #[tokio::test]
    async fn five_wrong_pins_block_the_device_and_skip_totp_afterwards() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        let correct = valid_pin(&service, NOW_MS);
        let wrong = wrong_pin(&correct);
        for attempt in 1..=MAX_FAILED_ATTEMPTS {
            let outcome = service
                .verify_pin(
                    &pin_request("device-c", &wrong),
                    &meta("198.51.100.1"),
                    NOW_MS,
                )
                .await;
            match outcome {
                PinOutcome::Failure {
                    attempts_left,
                    blocked,
                    ..
                } => {
                    assert_eq!(attempts_left, MAX_FAILED_ATTEMPTS - attempt);
                    assert_eq!(blocked, attempt == MAX_FAILED_ATTEMPTS);
                }
                other => panic!("attempt {attempt}: unexpected {other:?}"),
            }
        }
        // Blocked devices short-circuit before TOTP: even the correct PIN gets
        // the legacy `{ok:false,blocked:true}` shape (no 403 anywhere).
        assert_eq!(
            service
                .verify_pin(
                    &pin_request("device-c", &correct),
                    &meta("198.51.100.1"),
                    NOW_MS,
                )
                .await,
            PinOutcome::DeviceBlocked
        );
    }

    #[tokio::test]
    async fn ip_limit_is_checked_before_totp_and_blocks_ten_attempts() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        let ip = "198.51.100.2";
        let correct = valid_pin(&service, NOW_MS);
        let wrong = wrong_pin(&correct);
        for attempt in 1..=10 {
            let outcome = service
                .verify_pin(
                    &pin_request(&format!("device-ip-{attempt}"), &wrong),
                    &meta(ip),
                    NOW_MS,
                )
                .await;
            match outcome {
                PinOutcome::Failure {
                    ip_blocked,
                    retry_after_secs,
                    ..
                } => {
                    assert_eq!(ip_blocked, attempt == 10);
                    assert_eq!(
                        retry_after_secs,
                        if attempt == 10 { Some(900) } else { None }
                    );
                }
                other => panic!("attempt {attempt}: unexpected {other:?}"),
            }
        }
        // A correct PIN still hits the IP block (checked first).
        assert_eq!(
            service
                .verify_pin(&pin_request("device-ip-new", &correct), &meta(ip), NOW_MS)
                .await,
            PinOutcome::IpBlocked {
                retry_after_secs: 900
            }
        );
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM device_access WHERE device_id = 'device-ip-new'",
        )
        .fetch_one(service_pool(&service))
        .await
        .expect("count");
        assert_eq!(rows, 0, "blocked IP must not touch device rows");
    }

    #[tokio::test]
    async fn success_clears_the_ip_rate_limit_window() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        let correct = valid_pin(&service, NOW_MS);
        let wrong = wrong_pin(&correct);
        for attempt in 1..5 {
            service
                .verify_pin(
                    &pin_request(&format!("device-clear-{attempt}"), &wrong),
                    &meta("198.51.100.3"),
                    NOW_MS,
                )
                .await;
        }
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ip_rate_limits")
            .fetch_one(service_pool(&service))
            .await
            .expect("count");
        assert_eq!(rows, 1);
        service
            .verify_pin(
                &pin_request("device-clear-ok", &correct),
                &meta("198.51.100.3"),
                NOW_MS,
            )
            .await;
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ip_rate_limits")
            .fetch_one(service_pool(&service))
            .await
            .expect("count");
        assert_eq!(rows, 0, "success clears the IP window");
    }

    #[tokio::test]
    async fn malformed_pin_requests_follow_the_legacy_order() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        // Missing device_id → 400 before TOTP (even with a wrong PIN).
        assert_eq!(
            service
                .verify_pin(
                    &PinRequest {
                        pin: Some("000000".to_owned()),
                        device_id: None,
                    },
                    &meta("198.51.100.4"),
                    NOW_MS,
                )
                .await,
            PinOutcome::MissingDeviceId
        );
        // Missing PIN still counts as a failed attempt (Node `undefined`).
        let outcome = service
            .verify_pin(
                &PinRequest {
                    pin: None,
                    device_id: Some("device-missing-pin".to_owned()),
                },
                &meta("198.51.100.4"),
                NOW_MS,
            )
            .await;
        assert!(matches!(
            outcome,
            PinOutcome::Failure {
                attempts_left: 4,
                blocked: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn missing_secret_is_a_server_configuration_error() {
        let (_temp, service) = fixture(None, true).await;
        assert_eq!(
            service
                .verify_pin(
                    &pin_request("device-x", "123456"),
                    &meta("198.51.100.5"),
                    NOW_MS,
                )
                .await,
            PinOutcome::ServerConfig
        );
        assert!(service.totp_verifier().is_err());
    }

    #[tokio::test]
    async fn unavailable_database_fails_closed_without_touching_anything() {
        let service = AuthService::new(config(Some(S8_SECRET), true), None);
        assert_eq!(
            service
                .verify_pin(
                    &pin_request("device-x", "123456"),
                    &meta("198.51.100.6"),
                    NOW_MS,
                )
                .await,
            PinOutcome::Unavailable
        );
        assert_eq!(
            service
                .authenticate_device(CookieValue::Value("x"), NOW_MS)
                .await,
            DeviceAuth::Unavailable
        );
        assert_eq!(
            service.authenticate_admin(CookieValue::Value("x")).await,
            AdminAuth::Unavailable
        );
        assert_eq!(
            service
                .session(CookieValue::Value("x"), CookieValue::Missing)
                .await,
            SessionOutcome::Unavailable
        );
    }

    #[tokio::test]
    async fn device_guard_distinguishes_missing_empty_invalid_and_allowed() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        seed_allowed_token(&service, "device-d", "known-token").await;

        assert_eq!(
            service
                .authenticate_device(CookieValue::Missing, NOW_MS)
                .await,
            DeviceAuth::Missing
        );
        assert_eq!(
            service
                .authenticate_device(CookieValue::Empty, NOW_MS)
                .await,
            DeviceAuth::Empty
        );
        assert_eq!(
            service
                .authenticate_device(CookieValue::Value("wrong"), NOW_MS)
                .await,
            DeviceAuth::Invalid
        );
        match service
            .authenticate_device(CookieValue::Value("known-token"), NOW_MS)
            .await
        {
            DeviceAuth::Allowed(device) => assert_eq!(device.device_id, "device-d"),
            other => panic!("expected allowed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pending_and_blocked_devices_are_invalid_not_forbidden() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        seed_allowed_token(&service, "device-pending", "pending-token").await;
        seed_allowed_token(&service, "device-blocked", "blocked-token").await;
        sqlx::query(
            "UPDATE device_access SET status = 'pending' WHERE device_id = 'device-pending'",
        )
        .execute(service_pool(&service))
        .await
        .expect("pending");
        sqlx::query(
            "UPDATE device_access SET status = 'blocked' WHERE device_id = 'device-blocked'",
        )
        .execute(service_pool(&service))
        .await
        .expect("blocked");
        for token in ["pending-token", "blocked-token"] {
            assert_eq!(
                service
                    .authenticate_device(CookieValue::Value(token), NOW_MS)
                    .await,
                DeviceAuth::Invalid
            );
        }
    }

    #[tokio::test]
    async fn admin_guard_compares_the_plaintext_settings_token() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        seed_admin_token(&service, ADMIN_TOKEN).await;
        assert_eq!(
            service.authenticate_admin(CookieValue::Missing).await,
            AdminAuth::Missing
        );
        assert_eq!(
            service.authenticate_admin(CookieValue::Empty).await,
            AdminAuth::Empty
        );
        assert_eq!(
            service
                .authenticate_admin(CookieValue::Value("wrong-token"))
                .await,
            AdminAuth::Invalid
        );
        assert_eq!(
            service
                .authenticate_admin(CookieValue::Value(ADMIN_TOKEN))
                .await,
            AdminAuth::Allowed
        );
    }

    #[tokio::test]
    async fn admin_guard_rejects_when_no_admin_token_row_exists() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        assert_eq!(
            service
                .authenticate_admin(CookieValue::Value("anything"))
                .await,
            AdminAuth::Invalid
        );
    }

    #[tokio::test]
    async fn last_seen_is_throttled_to_once_per_thirty_seconds() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        seed_allowed_token(&service, "device-e", "known-token").await;
        // Now 29 s later: no write.
        service
            .authenticate_device(CookieValue::Value("known-token"), NOW_MS + 29_999)
            .await;
        let row = device::find_by_device_id(service_pool(&service), "device-e")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.last_seen_at, Some(NOW_MS));
        // Exactly 30 s later: one write.
        service
            .authenticate_device(CookieValue::Value("known-token"), NOW_MS + 30_000)
            .await;
        let row = device::find_by_device_id(service_pool(&service), "device-e")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.last_seen_at, Some(NOW_MS + 30_000));
    }

    #[tokio::test]
    async fn session_reports_device_admin_invalid_and_missing_states() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        seed_allowed_token(&service, "device-f", "known-token").await;
        seed_admin_token(&service, ADMIN_TOKEN).await;

        assert_eq!(
            service
                .session(CookieValue::Value("known-token"), CookieValue::Missing)
                .await,
            SessionOutcome::Device {
                device_id: "device-f".to_owned(),
                admin: false
            }
        );
        assert_eq!(
            service
                .session(
                    CookieValue::Value("known-token"),
                    CookieValue::Value(ADMIN_TOKEN)
                )
                .await,
            SessionOutcome::Device {
                device_id: "device-f".to_owned(),
                admin: true
            }
        );
        assert_eq!(
            service
                .session(CookieValue::Missing, CookieValue::Value(ADMIN_TOKEN))
                .await,
            SessionOutcome::Admin
        );
        assert_eq!(
            service
                .session(CookieValue::Missing, CookieValue::Missing)
                .await,
            SessionOutcome::Missing
        );
        assert_eq!(
            service
                .session(CookieValue::Value("wrong"), CookieValue::Missing)
                .await,
            SessionOutcome::Invalid
        );
        assert_eq!(
            service
                .session(CookieValue::Empty, CookieValue::Missing)
                .await,
            SessionOutcome::Invalid
        );
    }

    #[tokio::test]
    async fn session_does_not_write_last_seen_at() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        seed_allowed_token(&service, "device-g", "known-token").await;
        service
            .session(CookieValue::Value("known-token"), CookieValue::Missing)
            .await;
        let row = device::find_by_device_id(service_pool(&service), "device-g")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.last_seen_at, Some(NOW_MS), "session must stay a read");
    }

    #[tokio::test]
    async fn media_auth_bridge_allows_only_allowed_devices() {
        let (_temp, service) = fixture(Some(S8_SECRET), true).await;
        seed_allowed_token(&service, "device-h", "known-token").await;
        assert_eq!(
            service
                .authorize_media(CookieValue::Value("known-token"), NOW_MS)
                .await,
            MediaDecision::Allow {
                device_id: "device-h".to_owned()
            }
        );
        assert_eq!(
            service
                .authorize_media(CookieValue::Value("wrong"), NOW_MS)
                .await,
            MediaDecision::Deny(media::MediaDenyReason::Unauthenticated)
        );
        assert_eq!(
            service.authorize_media(CookieValue::Missing, NOW_MS).await,
            MediaDecision::Deny(media::MediaDenyReason::Unauthenticated)
        );
    }
}
