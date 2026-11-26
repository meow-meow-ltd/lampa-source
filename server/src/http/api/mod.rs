//! W1 API route registry.
//!
//! One module per legacy route group (plus the two W1 additions, session and
//! maintenance). Every module exposes `pub fn router() -> Router<AppState>`
//! and owns its handler logic (no stub responses remain in W1):
//!
//! | Module | Routes | Owner |
//! |---|---|---|
//! | [`settings`] | `GET`/`PUT /api/settings`, `DELETE /api/settings/{key}` | T07 |
//! | [`values`] | `POST /api/values`, `GET /api/value` | T07 |
//! | [`admin_auth`] | `POST /admin/login`, `POST /admin/logout` | T07 |
//! | [`admin_devices`] | `GET /api/admin/devices`, `POST /api/admin/device/action` | T07 |
//! | [`bookmarks`] | `GET /api/bookmarks/dump|changelog`, `POST /api/bookmarks/add|remove|clear|sync` | T08 |
//! | [`timeline`] | `GET /api/timeline/dump|changelog`, `POST /api/timeline/update` | T08 |
//! | [`profiles`] | `GET /api/profiles/all`, `POST /api/profiles/create|update|delete` | T09 |
//! | [`profile_settings`] | `GET`/`POST /api/profile/settings` | T09 |
//! | [`users_backup`] | `POST /api/users/backup/export`, `GET /api/users/backup/import` | T09 |
//! | [`maintenance_api`] | `POST /api/admin/maintenance/backup` | T11 |
//! | [`pin`] | `POST /api/pin/verify` (public) | T10 |
//! | [`auth_session`] | `GET /api/auth/session` (public) | T10 |
//!
//! The paths are frozen by the T03 golden fixtures (`server/tests/fixtures/api/`)
//! and are mounted exactly once from [`router`]. Axum panics on an overlapping
//! method route, so a duplicate mount fails fast in every test that builds the
//! application router.
//!
//! Auth is *not* enforced here: [`crate::http::router`] wraps this registry in
//! [`crate::http::middleware::auth_guard`] (implemented by T10). The route
//! modules must not add their own auth layers.

pub mod admin_auth;
pub mod admin_devices;
pub mod auth_session;
pub mod bookmarks;
pub mod maintenance_api;
pub mod pin;
pub mod profile_settings;
pub mod profiles;
pub mod settings;
pub mod timeline;
pub mod users_backup;
pub mod values;

use axum::Router;

use crate::state::AppState;

/// Mounts every W1 route group exactly once.
///
/// The whole registry is returned as one `Router` so the composition in
/// `http/mod.rs` mounts it once and wraps it in the auth guard. Route-group
/// owners edit their own module only.
pub fn router() -> Router<AppState> {
    Router::new()
        .merge(admin_auth::router())
        .merge(admin_devices::router())
        .merge(auth_session::router())
        .merge(bookmarks::router())
        .merge(maintenance_api::router())
        .merge(pin::router())
        .merge(profile_settings::router())
        .merge(profiles::router())
        .merge(settings::router())
        .merge(timeline::router())
        .merge(users_backup::router())
        .merge(values::router())
}
