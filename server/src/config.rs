//! Environment-based configuration.
//!
//! Only non-secret values are read here: `HOST`, `PORT`, `RUST_LOG`, the SQLite
//! connection URL and the lab-only `ALLOW_UNKNOWN_SCHEMA` switch. Defaults are
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

/// Error returned when an environment variable is present but invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `PORT` is not a valid TCP port (0..=65535).
    InvalidPort { value: String },
    /// `ALLOW_UNKNOWN_SCHEMA` is not a recognised boolean flag.
    InvalidAllowUnknownSchema { value: String },
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
            Some(value) => parse_bool_flag(&value)?,
        };

        Ok(Self {
            host,
            port,
            log_filter,
            database_url,
            allow_unknown_schema,
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

fn parse_bool_flag(value: &str) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(ConfigError::InvalidAllowUnknownSchema {
            value: value.to_owned(),
        }),
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
        assert_eq!(config.bind_address(), "0.0.0.0:3000");
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
