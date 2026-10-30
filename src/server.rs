//! Shared application state and the HTTP listener.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::ConnectInfo;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

use crate::api;
use crate::auth::Auth;
use crate::config::Config;
use crate::sessions::Sessions;
use crate::tls;

/// Open TLS connections in total and per client address.
const MAX_CONNECTIONS: usize = 512;
const MAX_CONNECTIONS_PER_IP: usize = 32;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(150);

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
    // Take over the tmux sessions started before a restart now, so that they count against
    // `max_sessions` from the first request on.
    let (sessions, _) = app.sessions.list(None).await;
    let running = sessions.iter().filter(|s| s.started_by_bridge).count();
    if running > 0 {
        tracing::info!("{running} sessions started by the bridge are still running");
    }
    app.sessions.prune_state();
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
    app.sessions.shutdown().await;
    tracing::info!("stopped");
    Ok(())
}

/// Accept loop for the public TLS listener. Every connection gets its own task with a TLS
/// handshake timeout, must send its first request within 15 s, and is closed after 150 s
/// without traffic (longer than the 90 s long-poll). Open connections are capped in total and
/// per client address, so a client that opens connections and stays silent cannot lock others
/// out.
async fn serve_tls(listener: TcpListener, router: Router, config: Arc<rustls::ServerConfig>) {
    let acceptor = TlsAcceptor::from(config);
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let per_ip: Arc<Mutex<HashMap<IpAddr, usize>>> = Arc::default();
    let mut dropped = DropLog::default();
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
        let Some(slot) = IpSlot::take(&per_ip, peer.ip()) else {
            dropped.record(peer, "too many connections from one address");
            continue;
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            dropped.record(peer, "connection limit reached");
            continue;
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            let _permit = permit;
            let _slot = slot;
            let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(err)) => {
                    tracing::debug!(%peer, "TLS handshake failed: {err}");
                    return;
                }
                Err(_) => return,
            };
            let activity = Arc::new(Activity::new());
            let io = TokioIo::new(Watched {
                inner: stream,
                activity: activity.clone(),
            });
            let service = router.map_request(
                move |mut request: axum::extract::Request<hyper::body::Incoming>| {
                    request.extensions_mut().insert(ConnectInfo(peer));
                    request
                },
            );
            let connection = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(io, TowerToHyperService::new(service));
            tokio::select! {
                result = watcher.watch(connection) => {
                    if let Err(err) = result {
                        tracing::debug!(%peer, "connection error: {err}");
                    }
                }
                _ = activity.expired() => tracing::debug!(%peer, "closing a silent connection"),
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

/// One of the connections a client address may hold; released on drop.
/// Logs dropped connections at most once a minute, with their number, so a client that keeps
/// opening connections cannot flood the log.
#[derive(Default)]
struct DropLog {
    last: Option<Instant>,
    dropped: u64,
}

impl DropLog {
    fn record(&mut self, peer: SocketAddr, reason: &str) {
        self.dropped += 1;
        if self
            .last
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
        {
            tracing::warn!(
                %peer,
                dropped = self.dropped,
                "{reason}; dropping connections (logged at most once a minute)"
            );
            self.last = Some(Instant::now());
            self.dropped = 0;
        }
    }
}

struct IpSlot {
    map: Arc<Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
}

impl IpSlot {
    fn take(map: &Arc<Mutex<HashMap<IpAddr, usize>>>, ip: IpAddr) -> Option<IpSlot> {
        let mut counts = map.lock().unwrap_or_else(|p| p.into_inner());
        let count = counts.entry(ip).or_insert(0);
        if *count >= MAX_CONNECTIONS_PER_IP {
            return None;
        }
        *count += 1;
        Some(IpSlot {
            map: map.clone(),
            ip,
        })
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut counts = self.map.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(count) = counts.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

/// When a connection last moved data, and whether the client ever sent any.
struct Activity {
    started: Instant,
    last_ms: AtomicU64,
    received: AtomicBool,
}

impl Activity {
    fn new() -> Activity {
        Activity {
            started: Instant::now(),
            last_ms: AtomicU64::new(0),
            received: AtomicBool::new(false),
        }
    }

    fn touch(&self, received: bool) {
        let now = self.started.elapsed().as_millis() as u64;
        self.last_ms.store(now, Ordering::Relaxed);
        if received {
            self.received.store(true, Ordering::Relaxed);
        }
    }

    /// Resolves when the connection has been silent for too long.
    async fn expired(&self) {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let elapsed = self.started.elapsed();
            let idle = elapsed.as_millis() as u64 - self.last_ms.load(Ordering::Relaxed);
            if !self.received.load(Ordering::Relaxed) && elapsed >= FIRST_REQUEST_TIMEOUT {
                return;
            }
            if idle >= IDLE_TIMEOUT.as_millis() as u64 {
                return;
            }
        }
    }
}

/// A stream that records traffic in an [`Activity`].
struct Watched<S> {
    inner: S,
    activity: Arc<Activity>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Watched<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.activity.touch(true);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watched<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, data);
        if matches!(result, Poll::Ready(Ok(n)) if n > 0) {
            self.activity.touch(false);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
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
