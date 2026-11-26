//! Composition root for the `lampa-server` binary.
//!
//! Scope: environment configuration (including the redacted `TOTP_SECRET` and
//! the transitional auth switch `DEVICE_LEGACY_COOKIE`), tracing, the SQLite
//! layer with its fail-closed schema guard, the Axum router with `/healthz`,
//! `/readyz` and the W1 API registry under the T10 auth guard, and graceful
//! shutdown on SIGTERM/SIGINT. Static/media/torrent services arrive in later
//! waves; the listener is served with connect info so PIN rate limiting can see
//! the client IP.

mod auth;
mod config;
mod db;
mod http;
mod profiles;
mod settings;
mod state;

use std::error::Error;
use std::io::IsTerminal;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinError;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::db::Database;
use crate::state::{AppState, DbState};

/// Upper bound for draining in-flight requests after a shutdown signal.
/// Must stay at or below 10 seconds (P0 requirement).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Result of the spawned HTTP server task.
type ServerResult = Result<Result<(), std::io::Error>, JoinError>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Maintenance CLI dispatch (W1/T11): `backup`/`restore`/`check` run before
    // any server configuration, so the operational commands never touch the
    // HTTP stack. `run_cli` wraps `db::backup::cli` with tracing, the stdout
    // JSON report and exit codes (0 success, 1 failure, 2 usage).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if db::backup::is_cli_invocation(&args) {
        std::process::exit(db::backup::run_cli(&args));
    }

    let config = Config::from_env()?;
    init_tracing(&config)?;

    // Fail-closed DB startup: a failed guard never aborts liveness; it only makes
    // `/readyz` return 503 (and logs one CRITICAL line). No writes happen.
    let db_state = match Database::open(config.database_url(), config.allow_unknown_schema()).await
    {
        Ok(database) => {
            let guard = database.guard_info();
            info!(
                schema_version = guard.schema_version.unwrap_or_default(),
                allow_unknown_schema = guard.allow_unknown_schema,
                journal_mode = db::guard::REQUIRED_JOURNAL_MODE,
                "database ready (schema guard passed)"
            );
            DbState::Ready(database)
        }
        Err(error) => {
            error!(
                code = error.code(),
                detail = error.detail().unwrap_or_default(),
                "CRITICAL: database schema guard failed; serving fail-closed (/readyz returns 503)"
            );
            DbState::Unavailable(error)
        }
    };
    if config.allow_unknown_schema() {
        warn!("ALLOW_UNKNOWN_SCHEMA=1 is lab-only; it must not be set in production");
    }
    // Keep a handle for graceful shutdown; AppState clones share the same pool.
    let database_for_shutdown = db_state.database().cloned();

    let bind_address = config.bind_address();
    let listener = TcpListener::bind(&bind_address).await?;
    let local_address = listener.local_addr()?;
    info!(address = %local_address, "lampa-server listening");

    let app = http::router(AppState::new(config, db_state));
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let mut server = tokio::spawn(async move {
        // With connect info so the auth layer can resolve the client IP for the
        // PIN rate limit (trusted-proxy headers are only honoured for private
        // peers; see `auth::rate_limit`/`http::middleware::client_ip`).
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            // A dropped sender also releases the wait; both mean "stop".
            let _ = shutdown_rx.await;
        })
        .await
    });

    let signal = tokio::select! {
        signal = wait_for_shutdown_signal() => signal?,
        finished = &mut server => return finish_server(finished),
    };
    info!(
        signal,
        "shutdown signal received; draining in-flight requests"
    );
    // Ignore send errors: the server task may already have stopped on its own.
    let _ = shutdown_tx.send(());

    match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut server).await {
        Ok(finished) => finish_server(finished)?,
        Err(_) => {
            warn!(
                timeout_secs = SHUTDOWN_TIMEOUT.as_secs(),
                "graceful shutdown timed out; aborting remaining connections"
            );
        }
    }
    if let Some(database) = database_for_shutdown {
        database.close().await;
    }
    info!("shutdown complete");
    Ok(())
}

/// Initialises tracing from `RUST_LOG`; falls back to `info` on an invalid filter.
fn init_tracing(config: &Config) -> Result<(), Box<dyn Error>> {
    let (env_filter, used_fallback) = match EnvFilter::try_new(config.log_filter()) {
        Ok(env_filter) => (env_filter, false),
        Err(_) => (EnvFilter::new(config::DEFAULT_LOG_FILTER), true),
    };

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_ansi(std::io::stdout().is_terminal())
        .try_init()
        .map_err(std::io::Error::other)?;

    if used_fallback {
        warn!(
            requested = config.log_filter(),
            default = config::DEFAULT_LOG_FILTER,
            "invalid RUST_LOG value; using default filter"
        );
    }
    Ok(())
}

/// Maps the joined server task to the process result.
fn finish_server(finished: ServerResult) -> Result<(), Box<dyn Error>> {
    match finished {
        Ok(Ok(())) => {
            info!("http server stopped");
            Ok(())
        }
        Ok(Err(error)) => {
            error!(%error, "http server failed");
            Err(Box::new(error))
        }
        Err(join_error) => {
            error!(%join_error, "http server task failed");
            Err(Box::new(join_error))
        }
    }
}

/// Resolves on SIGTERM or SIGINT, returning the signal name for logging.
#[cfg(unix)]
async fn wait_for_shutdown_signal() -> std::io::Result<&'static str> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => Ok("SIGTERM"),
        _ = sigint.recv() => Ok("SIGINT"),
    }
}

/// Resolves on Ctrl-C on non-Unix platforms.
#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> std::io::Result<&'static str> {
    tokio::signal::ctrl_c().await?;
    Ok("CTRL_C")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_timeout_is_within_budget() {
        assert!(SHUTDOWN_TIMEOUT <= Duration::from_secs(10));
        assert!(SHUTDOWN_TIMEOUT > Duration::ZERO);
    }
}
