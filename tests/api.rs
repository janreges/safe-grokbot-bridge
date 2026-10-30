//! End-to-end tests of the HTTP API, run in-process against temporary workspaces.

mod common;

use axum::http::StatusCode;
use common::{harness, url};
use safe_grokbot_bridge::api;
use serde_json::Value;
use std::path::Path;

async fn files_harness(allow: &str) -> common::Harness {
    let h = harness(allow, "").await;
    std::fs::write(h.root.join("README.md"), "# Demo\nline 2\n").unwrap();
    std::fs::write(h.root.join(".env"), "TOKEN=secret\n").unwrap();
    h
}

#[tokio::test]
async fn healthz_is_public_and_everything_else_needs_a_token() {
    let h = files_harness("[\"files\"]").await;
    assert_eq!(
        h.request("GET", "/healthz", None, None).await,
        (StatusCode::OK, Value::String("ok".into()))
    );
    let (status, body) = h.request("GET", "/v1/info", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_eq!(
        h.request("GET", "/openapi.json", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );

    let (status, body) = h.get("/v1/info").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["workspaces"][0]["path"], h.root.display().to_string());
    assert_eq!(h.get("/openapi.json").await.0, StatusCode::OK);
}

#[tokio::test]
async fn files_are_served_inside_workspaces_only() {
    let h = files_harness("[\"files\"]").await;
    let (status, body) = h.get(&url("/v1/files", "path", &h.root)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Vec<&str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(listed, [h.root.join("README.md").display().to_string()]);

    let readme = h.root.join("README.md");
    let (status, body) = h.get(&url("/v1/files/content", "path", &readme)).await;
    assert_eq!(
        (status, body["content"].as_str()),
        (StatusCode::OK, Some("# Demo\nline 2"))
    );

    let env = h.root.join(".env");
    let (status, body) = h.get(&url("/v1/files/content", "path", &env)).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("denied"))
    );

    let outside = h.root.parent().unwrap();
    let (status, _) = h.get(&url("/v1/files", "path", outside)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn capabilities_are_enforced() {
    let h = files_harness("[\"git\"]").await;
    let (status, body) = h.get(&url("/v1/files", "path", &h.root)).await;
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
