//! End-to-end tests of the HTTP API, run in-process against temporary workspaces.

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

struct Harness {
    router: axum::Router,
    token: String,
    root: PathBuf,
    _dir: tempfile::TempDir,
}

fn harness(allow: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("README.md"), "# Demo\nline 2\n").unwrap();
    std::fs::write(root.join(".env"), "TOKEN=secret\n").unwrap();
    let token = auth::generate_token();
    let text = format!(
        "[[tokens]]\nname = \"test\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\nallow = {allow}\n",
        auth::hash_token(&token),
        toml::Value::String(root.display().to_string())
    );
    let config = Config::parse(&text, Path::new("test.toml")).unwrap();
    let router = api::router(server::App::new(config).unwrap());
    Harness {
        router,
        token,
        root,
        _dir: dir,
    }
}

impl Harness {
    async fn get(&self, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::get(uri);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let mut request = request.body(Body::empty()).unwrap();
        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, body)
    }
}

fn url(endpoint: &str, param: &str, path: &Path) -> String {
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

#[tokio::test]
async fn healthz_is_public_and_everything_else_needs_a_token() {
    let h = harness("[\"files\"]");
    assert_eq!(
        h.get("/healthz", None).await,
        (StatusCode::OK, Value::String("ok".into()))
    );
    let (status, body) = h.get("/v1/info", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_eq!(
        h.get("/openapi.json", None).await.0,
        StatusCode::UNAUTHORIZED
    );

    let (status, body) = h.get("/v1/info", Some(&h.token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["workspaces"][0]["path"], h.root.display().to_string());
    assert_eq!(
        h.get("/openapi.json", Some(&h.token)).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn files_are_served_inside_workspaces_only() {
    let h = harness("[\"files\"]");
    let (status, body) = h
        .get(&url("/v1/files", "path", &h.root), Some(&h.token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Vec<&str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(listed, [h.root.join("README.md").display().to_string()]);

    let readme = h.root.join("README.md");
    let (status, body) = h
        .get(&url("/v1/files/content", "path", &readme), Some(&h.token))
        .await;
    assert_eq!(
        (status, body["content"].as_str()),
        (StatusCode::OK, Some("# Demo\nline 2"))
    );

    let env = h.root.join(".env");
    let (status, body) = h
        .get(&url("/v1/files/content", "path", &env), Some(&h.token))
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("denied"))
    );

    let outside = h.root.parent().unwrap();
    let (status, _) = h
        .get(&url("/v1/files", "path", outside), Some(&h.token))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn capabilities_are_enforced() {
    let h = harness("[\"git\"]");
    let (status, body) = h
        .get(&url("/v1/files", "path", &h.root), Some(&h.token))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not allow `files`")
    );
}

#[test]
fn openapi_document_is_up_to_date() {
    let generated = format!("{}\n", api::openapi().to_pretty_json().unwrap());
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/openapi.json");
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        committed == generated,
        "docs/openapi.json is out of date; run `cargo run -q -- openapi > docs/openapi.json`"
    );
}
