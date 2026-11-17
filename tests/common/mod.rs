//! Shared test harness: an in-process router with a temporary workspace and a valid token.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use safe_grokbot_bridge::config::Config;
use safe_grokbot_bridge::models::Timing;
use safe_grokbot_bridge::{api, auth, server};
use serde_json::Value;
use tower::ServiceExt;

pub struct Harness {
    pub router: axum::Router,
    pub token: String,
    pub root: PathBuf,
    pub dir: tempfile::TempDir,
}

/// A bridge with one workspace (a temp dir) allowing `allow`, plus `extra` config.
pub async fn harness(allow: &str, extra: &str) -> Harness {
    harness_timed(allow, extra, Timing::default()).await
}

/// [`harness`] with the time rules of model calls shortened.
pub async fn harness_timed(allow: &str, extra: &str, timing: Timing) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap().join("ws");
    std::fs::create_dir(&root).unwrap();
    let token = auth::generate_token();
    let text = format!(
        "[server]\nstate_dir = {}\n[[tokens]]\nname = \"test\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\nallow = {allow}\n{extra}",
        toml_str(&dir.path().join("state")),
        auth::hash_token(&token),
        toml_str(&root),
    );
    let config = Config::parse(&text, Path::new("test.toml")).unwrap();
    let router = api::router(server::App::with_timing(config, timing).await.unwrap());
    Harness {
        router,
        token,
        root,
        dir,
    }
}

/// A request as an upstream received it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub uri: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// A model server started by a test.
pub struct Upstream {
    /// Base URL with the version prefix: `http://127.0.0.1:<port>/v1`.
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Upstream {
    pub fn last(&self) -> Seen {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("the upstream got no request")
    }

    pub fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

/// Serves `router` on a free local port and records every request it gets.
pub async fn upstream(router: axum::Router) -> Upstream {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let app = router.layer(axum::middleware::from_fn(
        move |request: Request<Body>, next: axum::middleware::Next| {
            let record = record.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = body.collect().await.unwrap().to_bytes();
                record.lock().unwrap().push(Seen {
                    method: parts.method.to_string(),
                    uri: parts.uri.to_string(),
                    headers: parts.headers.clone(),
                    body: body.clone(),
                });
                next.run(Request::from_parts(parts, Body::from(body))).await
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Upstream {
        url: format!("http://127.0.0.1:{port}/v1"),
        seen,
    }
}

/// A local port nothing listens on.
pub fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Collects a response body and parses it as JSON (or returns it as a JSON string).
pub async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
}

/// A second token: `(token, "[[tokens]]…" TOML with the `extra` lines)`.
pub fn extra_token(name: &str, extra: &str) -> (String, String) {
    let token = auth::generate_token();
    let toml = format!(
        "[[tokens]]\nname = \"{name}\"\nsha256 = \"{}\"\n{extra}\n",
        auth::hash_token(&token)
    );
    (token, toml)
}

/// A `[models.<name>]` table with `type`, `url` and the `extra` lines.
pub fn model_toml(name: &str, kind: &str, url: &str, extra: &str) -> String {
    format!("[models.{name}]\ntype = \"{kind}\"\nurl = \"{url}\"\n{extra}\n")
}

pub fn toml_str(path: &Path) -> String {
    toml::Value::String(path.display().to_string()).to_string()
}

impl Harness {
    /// Serves the bridge on a free local port, for tests that need a real connection.
    pub async fn serve(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = self
            .router
            .clone()
            .into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}")
    }

    /// Sends a prepared request from a local peer address, as the server would see it.
    pub async fn send(&self, mut request: Request<Body>) -> axum::response::Response {
        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        self.router.clone().oneshot(request).await.unwrap()
    }

    pub async fn request(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let body = match body {
            Some(json) => {
                request = request.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = self.send(request.body(body).unwrap()).await;
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, body)
    }

    pub async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.request("GET", uri, None, Some(&self.token.clone()))
            .await
    }

    pub async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.request("POST", uri, Some(body), Some(&self.token.clone()))
            .await
    }

    pub async fn delete(&self, uri: &str) -> (StatusCode, Value) {
        self.request("DELETE", uri, None, Some(&self.token.clone()))
            .await
    }
}

/// Percent-encodes a path for a query string.
pub fn url(endpoint: &str, param: &str, path: &Path) -> String {
    let encoded: String = path
        .display()
        .to_string()
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect();
    format!("{endpoint}?{param}={encoded}")
}

/// Writes an executable script.
#[cfg(unix)]
pub fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
