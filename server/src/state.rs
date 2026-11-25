//! Shared application state wired by the composition root.

use std::sync::Arc;

use crate::config::Config;
use crate::db::{Database, SchemaGuardError};

/// Database availability as resolved once at startup.
///
/// A failed startup guard never aborts the process: `/healthz` stays 200
/// (liveness), while `/readyz` reports 503 (fail-closed readiness) until the
/// operator fixes the database. No writes happen while unavailable.
#[derive(Clone, Debug)]
pub enum DbState {
    /// Guard passed; the single-writer pool is usable.
    Ready(Database),
    /// Guard failed; carries only a sanitised reason code and log detail.
    Unavailable(SchemaGuardError),
}

impl DbState {
    /// The database handle when ready.
    pub fn database(&self) -> Option<&Database> {
        match self {
            DbState::Ready(database) => Some(database),
            DbState::Unavailable(_) => None,
        }
    }

    /// The guard failure when not ready.
    pub fn unavailable_reason(&self) -> Option<&SchemaGuardError> {
        match self {
            DbState::Ready(_) => None,
            DbState::Unavailable(error) => Some(error),
        }
    }
}

/// Shared state for the HTTP layer.
///
/// No global singletons: every handler receives `State<AppState>`. Later waves
/// add the engine/media/session handles here; the database handle is already
/// wired so `/readyz` reflects the startup guard.
#[derive(Clone, Debug)]
pub struct AppState {
    config: Arc<Config>,
    db: DbState,
}

impl AppState {
    /// Builds the state from the process configuration and the database state.
    pub fn new(config: Config, db: DbState) -> Self {
        Self {
            config: Arc::new(config),
            db,
        }
    }

    /// Read-only configuration snapshot.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Database readiness state.
    pub fn db(&self) -> &DbState {
        &self.db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{SchemaErrorKind, SchemaGuardError};

    #[test]
    fn state_exposes_the_config_and_db_state() {
        let config = Config::from_lookup(|_| None).expect("defaults must parse");
        let state = AppState::new(
            config.clone(),
            DbState::Unavailable(SchemaGuardError::plain(SchemaErrorKind::MissingFile)),
        );
        assert_eq!(state.config(), &config);
        assert!(state.db().database().is_none());
        assert_eq!(
            state.db().unavailable_reason().map(SchemaGuardError::code),
            Some("missing_file")
        );
    }
}
