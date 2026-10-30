//! Shared application state and the HTTP listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::ConnectInfo;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

use crate::api;
use crate::auth::Auth;
use crate::config::Config;
use crate::sessions::Sessions;
use crate::tls;

/// Concurrent TLS connections; protects a public listener from connection floods.
const MAX_CONNECTIONS: usize = 512;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct App {
    pub config: Arc<Config>,
    pub auth: Auth,
    pub sessions: Arc<Sessions>,
    pub started_at: SystemTime,
}

pub type AppState = Arc<App>;

impl App {
    pub async fn new(config: Config) -> Result<AppState> {
        let config = Arc::new(config);
        Ok(Arc::new(App {
            auth: Auth::new(&config),
            sessions: Arc::new(Sessions::new(config.clone()).await),
            config,
            started_at: SystemTime::now(),
        }))
    }
}

/// Runs the bridge until Ctrl+C or SIGTERM.
pub async fn run(config: Config) -> Result<()> {
    let listen = config.server.listen;
    let tls = match &config.server.tls {
        Some(settings) => Some(tls::load_or_create(
            settings,
            &config.server.state_dir,
            &config.server.name,
        )?),
        None => None,
    };
    let app = App::new(config).await?;
    let router = api::router(app.clone());
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let version = env!("CARGO_PKG_VERSION");
    match tls {
        None => {
            tracing::info!("safe-grokbot-bridge {version} listening on http://{listen}");
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("HTTP server failed")?;
        }
        Some(tls) => {
            tracing::info!("safe-grokbot-bridge {version} listening on https://{listen}");
            match &tls.pin {
                Some(pin) => tracing::info!(
                    "self-signed certificate {}; clients verify it with: curl --pinnedpubkey \"{pin}\" -k https://<host>:{}/v1/info",
                    tls.cert_path.display(),
                    listen.port()
                ),
                None => tracing::info!("TLS certificate {}", tls.cert_path.display()),
            }
            serve_tls(listener, router, tls.config).await;
        }
    }
    app.sessions.shutdown();
    tracing::info!("stopped");
    Ok(())
}

/// Accept loop for TLS. Unlike a TLS-wrapped `axum::serve`, the handshake runs in the
/// connection's own task with a timeout (a slow client cannot block other connections), the
/// header read timeout is active, and the number of open connections is capped.
async fn serve_tls(listener: TcpListener, router: Router, config: Arc<rustls::ServerConfig>) {
    let acceptor = TlsAcceptor::from(config);
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut shutdown = std::pin::pin!(shutdown_signal());
    loop {
        let (tcp, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(err) => {
                    tracing::warn!("accept failed: {err}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            tracing::warn!(%peer, "connection limit reached; dropping connection");
            continue;
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            let _permit = permit;
            let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(err)) => {
                    tracing::debug!(%peer, "TLS handshake failed: {err}");
                    return;
                }
                Err(_) => return,
            };
            let service = router.map_request(
                move |mut request: axum::extract::Request<hyper::body::Incoming>| {
                    request.extensions_mut().insert(ConnectInfo(peer));
                    request
                },
            );
            let mut builder = auto::Builder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT);
            builder.http2().timer(TokioTimer::new());
            let connection = builder.serve_connection_with_upgrades(
                TokioIo::new(stream),
                TowerToHyperService::new(service),
            );
            if let Err(err) = watcher.watch(connection.into_owned()).await {
                tracing::debug!(%peer, "connection error: {err}");
            }
        });
    }
    drop(listener);
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(Duration::from_secs(10)) => {
            tracing::warn!("closing with connections still open");
        }
    }
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
