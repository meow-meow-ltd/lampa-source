//! Media-auth policy hook (wired by W3; unit-tested now).
//!
//! Plan §5.3: every media request (`/gst/*`, `/stream/*`, `/ffp/*`, `/subs/*`,
//! `/cache`, `/torrents`, `/settings`, `/viewed`, `/api/v2.0/*`) must resolve
//! its device session and be rejected unless `device_access.status = 'allowed'`.
//! There is no authorization cache and no 403 for blocked devices: a revoked
//! segment request is a `401` that expires the cookies and sends the client
//! back to the PIN gate.
//!
//! W3 (T24) will call [`authorize`] with the session resolved by
//! [`crate::auth::AuthService::authenticate_device`]; this module deliberately
//! contains no HTTP/media-engine coupling, so the policy is exhaustively unit
//! tested before the media routes exist.

#![allow(dead_code)] // W3 wires the hook into the media routes.

/// Device status as stored in `device_access.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    /// Paired and usable.
    Allowed,
    /// New, re-voked or never paired device.
    Pending,
    /// Blocked after repeated failed PIN attempts.
    Blocked,
}

impl DeviceStatus {
    /// Maps the legacy TEXT status; unknown values are treated as `Pending`.
    pub fn from_db(status: &str) -> Self {
        match status {
            "allowed" => DeviceStatus::Allowed,
            "blocked" => DeviceStatus::Blocked,
            _ => DeviceStatus::Pending,
        }
    }
}

/// Minimal session facts the media policy needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSubject {
    /// Legacy device identifier (for logs/context downstream; not a secret).
    pub device_id: String,
    /// Current device status.
    pub status: DeviceStatus,
}

/// Why a media request is denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaDenyReason {
    /// No valid device cookie / no matching `device_access` row.
    Unauthenticated,
    /// The row exists but is `pending` or `blocked` (revoked pair).
    Revoked,
}

/// Allow/deny decision for one media request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaDecision {
    /// Serve the media request as `device_id`.
    Allow {
        /// Authenticated device identifier.
        device_id: String,
    },
    /// Reject with the auth guard contract (401 + cookie clearing).
    Deny(MediaDenyReason),
}

/// Resolves the policy: only an `allowed` device session may stream.
pub fn authorize(subject: Option<MediaSubject>) -> MediaDecision {
    match subject {
        None => MediaDecision::Deny(MediaDenyReason::Unauthenticated),
        Some(subject) if subject.status == DeviceStatus::Allowed => MediaDecision::Allow {
            device_id: subject.device_id,
        },
        Some(_) => MediaDecision::Deny(MediaDenyReason::Revoked),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(status: DeviceStatus) -> MediaSubject {
        MediaSubject {
            device_id: "device-1".to_owned(),
            status,
        }
    }

    #[test]
    fn allowed_sessions_may_stream() {
        assert_eq!(
            authorize(Some(subject(DeviceStatus::Allowed))),
            MediaDecision::Allow {
                device_id: "device-1".to_owned()
            }
        );
    }

    #[test]
    fn missing_sessions_are_denied_as_unauthenticated() {
        assert_eq!(
            authorize(None),
            MediaDecision::Deny(MediaDenyReason::Unauthenticated)
        );
    }

    #[test]
    fn pending_and_blocked_sessions_are_denied_as_revoked() {
        for status in [DeviceStatus::Pending, DeviceStatus::Blocked] {
            assert_eq!(
                authorize(Some(subject(status))),
                MediaDecision::Deny(MediaDenyReason::Revoked)
            );
        }
    }

    #[test]
    fn database_status_mapping_covers_legacy_values() {
        assert_eq!(DeviceStatus::from_db("allowed"), DeviceStatus::Allowed);
        assert_eq!(DeviceStatus::from_db("pending"), DeviceStatus::Pending);
        assert_eq!(DeviceStatus::from_db("blocked"), DeviceStatus::Blocked);
        assert_eq!(
            DeviceStatus::from_db("anything-else"),
            DeviceStatus::Pending
        );
    }
}
