//! Environment-based configuration.
//!
//! Non-secret values: `HOST`, `PORT`, `RUST_LOG`, the SQLite connection URL, the
//! lab-only `ALLOW_UNKNOWN_SCHEMA` switch and the transitional auth switch
//! `DEVICE_LEGACY_COOKIE` (default `1` = C, `0` = B-strict).
//!
//! The TOTP shared secret (`TOTP_SECRET`, base32) is the only secret read here:
//! it is wrapped in [`SecretString`], so `Config`'s `Debug` output (and any
//! `tracing` field using it) prints `***` instead of the value. Defaults are
//! documented in `server/README.md` and `docs/w0-notes/T04.md`.

use std::fmt;

/// Default bind host when `HOST` is unset or empty.
pub const DEFAULT_HOST: &str = "0.0.0.0";
/// Default bind port when `PORT` is unset or empty.
pub const DEFAULT_PORT: u16 = 3000;
/// Default log filter when `RUST_LOG` is unset, empty or invalid.
pub const DEFAULT_LOG_FILTER: &str = "info";
/// Default database URL: the production container path, opened read-write
/// without ever creating the file (`mode=rw`, plan §4.2).
pub const DEFAULT_DATABASE_URL: &str = "sqlite:/data/database.sqlite?mode=rw";
/// Default transitional auth mode: `1` issues/accepts the legacy readable
/// `device_token` cookie (C); `0` is B-strict (`lampa_session` only).
pub const DEFAULT_DEVICE_LEGACY_COOKIE: bool = true;

/// A secret read from the environment but redacted from `Debug` output.
///
/// The inner value is never formatted; `Config` and every structure embedding
/// it can therefore be logged safely.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(Option<String>);

impl SecretString {
    fn from_non_empty(value: Option<String>) -> Self {
        Self(non_empty(value))
    }

    /// The secret value, if configured and non-empty.
    pub fn as_str(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(_) => formatter.write_str("\"***\""),
            None => formatter.write_str("None"),
        }
    }
}

/// Error returned when an environment variable is present but invalid.
///
/// The `Invalid*` prefix is intentional and shared by all variants; the lint is
/// allowed rather than renaming stable public error identities.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `PORT` is not a valid TCP port (0..=65535).
    InvalidPort { value: String },
    /// `ALLOW_UNKNOWN_SCHEMA` is not a recognised boolean flag.
    InvalidAllowUnknownSchema { value: String },
    /// `DEVICE_LEGACY_COOKIE` is not a recognised boolean flag.
    InvalidDeviceLegacyCookie { value: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidPort { value } => {
                write!(formatter, "PORT must be a valid TCP port, got {value:?}")
            }
            ConfigError::InvalidAllowUnknownSchema { value } => write!(
                formatter,
                "ALLOW_UNKNOWN_SCHEMA must be 1/0/true/false, got {value:?}"
            ),
            ConfigError::InvalidDeviceLegacyCookie { value } => write!(
                formatter,
                "DEVICE_LEGACY_COOKIE must be 1/0/true/false, got {value:?}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Runtime configuration resolved once at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    host: String,
    port: u16,
    log_filter: String,
    database_url: String,
    allow_unknown_schema: bool,
    device_legacy_cookie: bool,
    totp_secret: SecretString,
}

impl Config {
    /// Reads configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Builds configuration from an arbitrary lookup function.
    ///
    /// Tests use this instead of mutating the process environment; production
    /// code uses `from_env`. Empty and whitespace-only values fall back to
    /// the documented defaults.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let host = non_empty(lookup("HOST")).unwrap_or_else(|| DEFAULT_HOST.to_owned());

        let port = match non_empty(lookup("PORT")) {
            Some(value) => value.parse::<u16>().map_err(|_| ConfigError::InvalidPort {
                value: value.clone(),
            })?,
            None => DEFAULT_PORT,
        };

        let log_filter =
            non_empty(lookup("RUST_LOG")).unwrap_or_else(|| DEFAULT_LOG_FILTER.to_owned());

        let database_url =
            non_empty(lookup("DATABASE_URL")).unwrap_or_else(|| DEFAULT_DATABASE_URL.to_owned());

        // Lab-only escape hatch; production config must not set it (W0-7).
        let allow_unknown_schema = match non_empty(lookup("ALLOW_UNKNOWN_SCHEMA")) {
            None => false,
            Some(value) => parse_bool_flag(&value, |value| {
                ConfigError::InvalidAllowUnknownSchema { value }
            })?,
        };

        // Transitional auth mode (D6/S8): default C, `0` = B-strict.
        let device_legacy_cookie = match non_empty(lookup("DEVICE_LEGACY_COOKIE")) {
            None => DEFAULT_DEVICE_LEGACY_COOKIE,
            Some(value) => parse_bool_flag(&value, |value| {
                ConfigError::InvalidDeviceLegacyCookie { value }
            })?,
        };

        // Secret: never logged (SecretString's Debug prints `***`).
        let totp_secret = SecretString::from_non_empty(lookup("TOTP_SECRET"));

        Ok(Self {
            host,
            port,
            log_filter,
            database_url,
            allow_unknown_schema,
            device_legacy_cookie,
            totp_secret,
        })
    }

    /// Interface the HTTP server binds to.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// TCP port the HTTP server binds to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Filter passed to `tracing-subscriber`; never contains secrets.
    pub fn log_filter(&self) -> &str {
        &self.log_filter
    }

    /// SQLite connection URL (`sqlite:<absolute path>?mode=rw`).
    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    /// Lab-only schema-version bypass; must stay `false` in production.
    pub fn allow_unknown_schema(&self) -> bool {
        self.allow_unknown_schema
    }

    /// Transitional auth mode: `true` = C (also issue/accept the readable legacy
    /// `device_token`), `false` = B-strict (`lampa_session` only).
    pub fn device_legacy_cookie(&self) -> bool {
        self.device_legacy_cookie
    }

    /// Base32 TOTP secret (`TOTP_SECRET`); `None` when unset/blank.
    pub fn totp_secret(&self) -> Option<&str> {
        self.totp_secret.as_str()
    }

    /// `host:port` string accepted by `TcpListener::bind`.
    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_bool_flag<F>(value: &str, error: F) -> Result<bool, ConfigError>
where
    F: Fn(String) -> ConfigError,
{
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(error(value.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config_from(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let values: HashMap<&str, &str> = pairs.iter().copied().collect();
        Config::from_lookup(|key| values.get(key).map(|value| (*value).to_owned()))
    }

    #[test]
    fn defaults_are_documented_values() {
        let config = config_from(&[]).expect("defaults must parse");
        assert_eq!(config.host(), DEFAULT_HOST);
        assert_eq!(config.port(), DEFAULT_PORT);
        assert_eq!(config.log_filter(), DEFAULT_LOG_FILTER);
        assert_eq!(config.database_url(), DEFAULT_DATABASE_URL);
        assert!(!config.allow_unknown_schema());
        assert!(config.device_legacy_cookie());
        assert_eq!(config.totp_secret(), None);
        assert_eq!(config.bind_address(), "0.0.0.0:3000");
    }

    #[test]
    fn transitional_auth_mode_defaults_to_c_and_can_switch_to_b_strict() {
        let strict = config_from(&[("DEVICE_LEGACY_COOKIE", "0")]).expect("strict flag");
        assert!(!strict.device_legacy_cookie());
        let c = config_from(&[("DEVICE_LEGACY_COOKIE", "true")]).expect("true flag");
        assert!(c.device_legacy_cookie());
        assert!(config_from(&[("DEVICE_LEGACY_COOKIE", "   ")])
            .expect("blank falls back")
            .device_legacy_cookie());
        let error =
            config_from(&[("DEVICE_LEGACY_COOKIE", "yes")]).expect_err("strict boolean parsing");
        assert_eq!(
            error,
            ConfigError::InvalidDeviceLegacyCookie {
                value: "yes".to_owned()
            }
        );
        assert!(error.to_string().contains("DEVICE_LEGACY_COOKIE"));
    }

    #[test]
    fn totp_secret_is_read_but_redacted_from_debug() {
        let config = config_from(&[("TOTP_SECRET", "JBSWY3DPEHPK3PXP")]).expect("secret");
        assert_eq!(config.totp_secret(), Some("JBSWY3DPEHPK3PXP"));
        let debug = format!("{config:?}");
        assert!(!debug.contains("JBSWY3DPEHPK3PXP"));
        assert!(debug.contains("\"***\""));
        assert_eq!(
            config_from(&[("TOTP_SECRET", "   ")])
                .expect("blank secret")
                .totp_secret(),
            None
        );
    }

    #[test]
    fn environment_overrides_are_applied() {
        let config = config_from(&[
            ("HOST", "127.0.0.1"),
            ("PORT", "9120"),
            ("RUST_LOG", "debug"),
        ])
        .expect("overrides must parse");
        assert_eq!(config.host(), "127.0.0.1");
        assert_eq!(config.port(), 9120);
        assert_eq!(config.log_filter(), "debug");
        assert_eq!(config.bind_address(), "127.0.0.1:9120");
    }

    #[test]
    fn empty_and_whitespace_values_fall_back_to_defaults() {
        let config = config_from(&[("HOST", "   "), ("PORT", ""), ("RUST_LOG", "")])
            .expect("blank values must not fail");
        assert_eq!(config.host(), DEFAULT_HOST);
        assert_eq!(config.port(), DEFAULT_PORT);
        assert_eq!(config.log_filter(), DEFAULT_LOG_FILTER);
    }

    #[test]
    fn values_are_trimmed() {
        let config = config_from(&[("HOST", " 127.0.0.1 "), ("PORT", " 9120 ")])
            .expect("trimmed values must parse");
        assert_eq!(config.host(), "127.0.0.1");
        assert_eq!(config.port(), 9120);
    }

    #[test]
    fn invalid_port_is_a_config_error_not_a_panic() {
        let error = config_from(&[("PORT", "not-a-port")]).expect_err("invalid port must fail");
        assert_eq!(
            error,
            ConfigError::InvalidPort {
                value: "not-a-port".to_owned()
            }
        );
        assert!(error.to_string().contains("PORT"));
    }

    #[test]
    fn out_of_range_port_is_a_config_error() {
        assert!(config_from(&[("PORT", "70000")]).is_err());
        assert!(config_from(&[("PORT", "-1")]).is_err());
    }

    #[test]
    fn database_url_and_lab_flag_can_be_overridden() {
        let config = config_from(&[
            ("DATABASE_URL", "sqlite:/tmp/lab.sqlite?mode=rw"),
            ("ALLOW_UNKNOWN_SCHEMA", "1"),
        ])
        .expect("valid overrides");
        assert_eq!(config.database_url(), "sqlite:/tmp/lab.sqlite?mode=rw");
        assert!(config.allow_unknown_schema());

        let config = config_from(&[("ALLOW_UNKNOWN_SCHEMA", "false")]).expect("false");
        assert!(!config.allow_unknown_schema());
    }

    #[test]
    fn allow_unknown_schema_rejects_typos_instead_of_guessing() {
        let error =
            config_from(&[("ALLOW_UNKNOWN_SCHEMA", "yes")]).expect_err("strict flag parsing");
        assert_eq!(
            error,
            ConfigError::InvalidAllowUnknownSchema {
                value: "yes".to_owned()
            }
        );
        assert!(error.to_string().contains("ALLOW_UNKNOWN_SCHEMA"));
        assert!(!config_from(&[("ALLOW_UNKNOWN_SCHEMA", "   ")])
            .expect("blank")
            .allow_unknown_schema());
    }
}
