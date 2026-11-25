//! Shared application state wired by the composition root.

use std::sync::Arc;

use crate::config::Config;

/// Minimal shared state for W0.
///
/// Later waves add the database pool, engine, media and session handles here;
/// there are no global singletons, every handler receives `State<AppState>`.
#[derive(Clone, Debug)]
pub struct AppState {
    config: Arc<Config>,
}

impl AppState {
    /// Builds the state from the process configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Read-only configuration snapshot.
    pub fn config(&self) -> &Config {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_exposes_the_config() {
        let config = Config::from_lookup(|_| None).expect("defaults must parse");
        let state = AppState::new(config.clone());
        assert_eq!(state.config(), &config);
    }
}
