//! Shared test harness: an in-process router with a temporary workspace and a valid token.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use safe_grokbot_bridge::config::Config;
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
    let router = api::router(server::App::new(config).await.unwrap());
    Harness {
        router,
        token,
        root,
        dir,
    }
}

pub fn toml_str(path: &Path) -> String {
    toml::Value::String(path.display().to_string()).to_string()
}

impl Harness {
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
        let mut request = request.body(body).unwrap();
        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        let response = self.router.clone().oneshot(request).await.unwrap();
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
