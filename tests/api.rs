//! End-to-end tests of the HTTP API, run in-process against temporary workspaces.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{harness, url};
use safe_grokbot_bridge::config::Config;
use safe_grokbot_bridge::{api, auth, server};
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
    // Errors keep the JSON shape even when the URL itself cannot be decoded, the endpoint does
    // not exist or does not take the method.
    let (status, body) = h.get("/v1/sessions/%FF").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request", "{body}");
    let (status, body) = h.get("/v1/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found", "{body}");
    let (status, body) = h.delete("/v1/info").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["error"]["code"], "method_not_allowed", "{body}");
    // Unknown endpoints need a token like everything else.
    let (status, _) = h.request("GET", "/v1/nope", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
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

#[tokio::test]
async fn a_token_in_x_api_key_is_accepted() {
    let h = harness("[\"files\"]", "[limits]\nauth_failures = 2\n").await;
    let call = |key: String| {
        let request = Request::builder()
            .uri("/v1/info")
            .header("x-api-key", key)
            .body(Body::empty())
            .unwrap();
        h.send(request)
    };
    assert_eq!(call(h.token.clone()).await.status(), StatusCode::OK);
    // A wrong key counts as a failed attempt like a wrong bearer token: the second one blocks the
    // address.
    assert_eq!(
        call("sgb_wrong".into()).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call("sgb_wrong".into()).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn models_only_tokens_reach_only_the_open_routes() {
    let (brain, toml) = common::extra_token("brain", "models_only = true");
    let h = harness("[\"files\", \"git\", \"sessions\"]", &toml).await;
    let closed = [
        "/v1/info".to_string(),
        url("/v1/files", "path", &h.root),
        url("/v1/files/content", "path", &h.root.join("a.txt")),
        url("/v1/git/status", "dir", &h.root),
        "/v1/sessions".to_string(),
    ];
    for uri in &closed {
        let (status, body) = h.request("GET", uri, None, Some(&brain)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(body["error"]["code"], "forbidden", "{uri}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("limited to models"),
            "{body}"
        );
    }
    let (status, _) = h
        .request(
            "POST",
            "/v1/sessions",
            Some(serde_json::json!({})),
            Some(&brain),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        h.request("GET", "/openapi.json", None, Some(&brain))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(h.get("/v1/info").await.0, StatusCode::OK);
}

#[tokio::test]
async fn the_config_file_is_never_served() {
    // A config kept inside a workspace, as with a workspace at the home folder.
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap().join("ws");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("README.md"), "# Demo\n").unwrap();
    let token = auth::generate_token();
    let file = root.join("bridge.toml");
    std::fs::write(
        &file,
        format!(
            "[server]\nstate_dir = {}\n[[tokens]]\nname = \"test\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\nallow = [\"files\", \"download\"]\n",
            common::toml_str(&dir.path().join("state")),
            auth::hash_token(&token),
            common::toml_str(&root),
        ),
    )
    .unwrap();
    let config = Config::load(&file).unwrap();
    let router = api::router(server::App::new(config).await.unwrap());
    let h = common::Harness {
        router,
        token,
        root: root.clone(),
        dir,
    };

    let (status, body) = h.get(&url("/v1/files", "path", &root)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Vec<&str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(listed, [root.join("README.md").display().to_string()]);

    let (status, body) = h.get(&url("/v1/files/content", "path", &file)).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("denied"))
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("configuration file")
    );
    let (status, body) = h.get(&url("/v1/files/download", "path", &file)).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("denied"))
    );
}

#[test]
fn every_openapi_response_is_described() {
    let doc: Value = serde_json::from_str(&api::openapi().to_json().unwrap()).unwrap();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            for (status, response) in operation["responses"].as_object().unwrap() {
                let described = response["description"]
                    .as_str()
                    .is_some_and(|d| !d.is_empty());
                assert!(described, "{method} {path} {status} has no description");
            }
        }
    }
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
