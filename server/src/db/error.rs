//! Repository error type.
//!
//! The HTTP layer must never forward raw SQLite/SQLx diagnostics to clients
//! (plan §3.6 п.6: "сирі SQL-помилки клієнту не віддаємо"). Therefore:
//!
//! * [`DbError`] `Display` is a stable, generic sentence — no SQL text, no
//!   table/column names, no bound values;
//! * the underlying driver error stays available through [`std::error::Error::source`]
//!   for server-side logs only;
//! * [`DbError::code`] gives a short machine-readable reason safe for API
//!   responses (T07/T09 decide the exact legacy body).

// The repository API is the T04 deliverable consumed by the T07/T09 HTTP
// layers (not wired yet); unit tests exercise it directly.
#![allow(dead_code)]

use std::error::Error as StdError;
use std::fmt;

/// Machine-readable category of a repository failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorKind {
    /// The driver rejected a statement (syntax, constraint, closed pool, ...).
    Query,
    /// The caller passed invalid repository input (empty key, empty name, ...).
    Validation,
    /// A profile update carried no fields to change (legacy 400 case).
    NothingToUpdate,
}

impl DbErrorKind {
    /// Stable snake_case code suitable for logs and API error bodies.
    pub fn code(self) -> &'static str {
        match self {
            DbErrorKind::Query => "db_query_failed",
            DbErrorKind::Validation => "invalid_input",
            DbErrorKind::NothingToUpdate => "no_fields_to_update",
        }
    }
}

/// Sanitised repository error.
///
/// Construct exclusively through [`DbError::query`], [`DbError::validation`] and
/// [`DbError::nothing_to_update`]; the fields stay private so no future caller can
/// smuggle raw SQL text into the public message.
pub struct DbError {
    kind: DbErrorKind,
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl DbError {
    /// Wraps a driver error; the driver text is kept for logs only.
    pub fn query(source: sqlx::Error) -> Self {
        Self {
            kind: DbErrorKind::Query,
            source: Some(Box::new(source)),
        }
    }

    /// Invalid repository input; `reason` is a short, non-SQL explanation.
    pub fn validation(reason: &'static str) -> Self {
        Self {
            kind: DbErrorKind::Validation,
            source: Some(Box::new(ValidationReason(reason))),
        }
    }

    /// Legacy `POST /api/profiles/update` with an empty patch.
    pub fn nothing_to_update() -> Self {
        Self {
            kind: DbErrorKind::NothingToUpdate,
            source: None,
        }
    }

    /// Category of the failure.
    pub fn kind(&self) -> DbErrorKind {
        self.kind
    }

    /// Machine-readable code (safe to map to an API response).
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately generic: never contains SQL or driver diagnostics.
        write!(formatter, "database operation failed ({})", self.code())
    }
}

impl fmt::Debug for DbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DbError")
            .field("kind", &self.kind)
            .field("source", &self.source)
            .finish()
    }
}

impl StdError for DbError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match &self.source {
            Some(source) => Some(source.as_ref()),
            None => None,
        }
    }
}

#[derive(Debug)]
struct ValidationReason(&'static str);

impl fmt::Display for ValidationReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl StdError for ValidationReason {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_error_display_is_sanitized() {
        let error = DbError::query(sqlx::Error::RowNotFound);
        let message = error.to_string();
        assert_eq!(message, "database operation failed (db_query_failed)");
        // The driver-specific text ("no rows returned...") never reaches Display.
        assert!(!message.contains("rows"));
        assert!(error.source().is_some());
    }

    #[test]
    fn validation_error_carries_code_but_not_reason_in_message() {
        let error = DbError::validation("empty settings key");
        assert_eq!(error.kind(), DbErrorKind::Validation);
        assert_eq!(error.code(), "invalid_input");
        assert_eq!(
            error.to_string(),
            "database operation failed (invalid_input)"
        );
        assert!(!error.to_string().contains("empty settings key"));
    }

    #[test]
    fn nothing_to_update_has_no_source() {
        let error = DbError::nothing_to_update();
        assert_eq!(error.code(), "no_fields_to_update");
        assert!(error.source().is_none());
    }
}
