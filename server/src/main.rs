//! Composition root for the `lampa-server` binary.
//!
//! W0 scope only: environment configuration, tracing, the Axum router with
//! `GET /healthz`, and graceful shutdown on SIGTERM/SIGINT. Database,
//! auth, static/media/torrent services arrive in later waves.

mod config;
mod http;
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
use crate::state::AppState;

/// Upper bound for draining in-flight requests after a shutdown signal.
/// Must stay at or below 10 seconds (P0 requirement).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Result of the spawned HTTP server task.
type ServerResult = Result<Result<(), std::io::Error>, JoinError>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::from_env()?;
    init_tracing(&config)?;

    let bind_address = config.bind_address();
    let listener = TcpListener::bind(&bind_address).await?;
    let local_address = listener.local_addr()?;
    info!(address = %local_address, "lampa-server listening");

    let app = http::router(AppState::new(config));
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
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
