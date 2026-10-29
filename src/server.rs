//! Shared application state and the HTTP listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

use crate::api;
use crate::auth::Auth;
use crate::config::Config;

pub struct App {
    pub config: Config,
    pub auth: Auth,
    pub started_at: SystemTime,
}

pub type AppState = Arc<App>;

impl App {
    pub fn new(config: Config) -> Result<AppState> {
        Ok(Arc::new(App {
            auth: Auth::new(&config),
            config,
            started_at: SystemTime::now(),
        }))
    }
}

/// Runs the bridge until Ctrl+C or SIGTERM.
pub async fn run(config: Config) -> Result<()> {
    let listen = config.server.listen;
    let app = App::new(config)?;
    let router = api::router(app.clone());
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    tracing::info!(
        "safe-grokbot-bridge {} listening on http://{listen} ({} workspace(s))",
        env!("CARGO_PKG_VERSION"),
        app.config.workspaces.len()
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("HTTP server failed")?;
    tracing::info!("stopped");
    Ok(())
}

/// Resolves on Ctrl+C, or SIGTERM on Unix.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down");
}
