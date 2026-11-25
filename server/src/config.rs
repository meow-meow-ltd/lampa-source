//! Environment-based configuration.
//!
//! Only non-secret values are read here: `HOST`, `PORT` and `RUST_LOG`.
//! Defaults are documented in `server/README.md`.

use std::fmt;

/// Default bind host when `HOST` is unset or empty.
pub const DEFAULT_HOST: &str = "0.0.0.0";
/// Default bind port when `PORT` is unset or empty.
pub const DEFAULT_PORT: u16 = 3000;
/// Default log filter when `RUST_LOG` is unset, empty or invalid.
pub const DEFAULT_LOG_FILTER: &str = "info";

/// Error returned when an environment variable is present but invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `PORT` is not a valid TCP port (0..=65535).
    InvalidPort { value: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidPort { value } => {
                write!(formatter, "PORT must be a valid TCP port, got {value:?}")
            }
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

        Ok(Self {
            host,
            port,
            log_filter,
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
}
