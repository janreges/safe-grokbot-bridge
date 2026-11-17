//! End-to-end tests of the model endpoints, against small upstream servers started in the test.

mod common;

use axum::http::StatusCode;
use common::{harness, model_toml};
use serde_json::{Value, json};

/// A bridge with three models and a second, `models_only` token: `(harness, brain token)`.
async fn listing_harness() -> (common::Harness, String) {
    let (brain, brain_toml) = common::extra_token("brain", "models_only = true");
    let models = [
        model_toml(
            "qwen",
            "chat",
            "http://10.0.0.5:8000/v1",
            "model = \"Qwen/Qwen3-32B\"\ndescription = \"Local Qwen.\"\n\
             defaults = { chat_template_kwargs = { enable_thinking = false } }",
        ),
        model_toml("embed", "embedding", "http://10.0.0.5:8001/v1", ""),
        model_toml(
            "opus",
            "chat",
            "https://api.anthropic.com/v1",
            "api = \"anthropic\"\napi_key = \"sk-secret\"\ntokens = [\"brain\"]",
        ),
    ]
    .concat();
    let h = harness("[\"files\"]", &format!("{models}{brain_toml}")).await;
    (h, brain)
}

fn ids(body: &Value) -> Vec<String> {
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn the_list_shows_what_a_token_may_call() {
    let (h, brain) = listing_harness().await;

    let (status, body) = h.get("/v1/models").await;
    assert_eq!(
        (status, body["object"].as_str()),
        (StatusCode::OK, Some("list"))
    );
    assert_eq!(ids(&body), ["embed", "qwen"]);
    let qwen = &body["data"][1];
    assert!(qwen["created"].as_u64().unwrap() > 1_700_000_000);
    let mut expected = json!({"id": "qwen", "object": "model", "owned_by": "safe-grokbot-bridge", "type": "chat",
        "api": "openai", "endpoint": "/v1/chat/completions", "upstream_model": "Qwen/Qwen3-32B",
        "description": "Local Qwen.", "defaults": {"chat_template_kwargs": {"enable_thinking": false}}});
    expected["created"] = qwen["created"].clone();
    assert_eq!(qwen, &expected);
    assert!(!body.to_string().contains("sk-secret") && !body.to_string().contains("10.0.0.5"));

    assert_eq!(ids(&h.get("/v1/models?type=embedding").await.1), ["embed"]);
    let (_, empty) = h.get("/v1/models?type=reranker").await;
    assert!(ids(&empty).is_empty() && empty["note"].as_str().unwrap().contains("reranker"));
    let (status, body) = h.get("/v1/models?type=nonsense").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_request"))
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("speech-to-text")
    );

    let (status, body) = h.request("GET", "/v1/models", None, Some(&brain)).await;
    assert_eq!(
        (status, ids(&body)),
        (
            StatusCode::OK,
            vec!["embed".to_string(), "opus".into(), "qwen".into()]
        )
    );
    assert_eq!(
        (
            body["data"][1]["api"].as_str(),
            body["data"][1]["endpoint"].as_str()
        ),
        (Some("anthropic"), Some("/v1/messages"))
    );

    let (_, info) = h.get("/v1/info").await;
    assert_eq!(ids(&json!({"data": info["models"]})), ["embed", "qwen"]);
    assert_eq!(info["limits"]["max_model_requests"], 16);
}

#[tokio::test]
async fn an_empty_list_says_why() {
    let h = harness("[\"files\"]", "").await;
    let (status, body) = h.get("/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["data"].as_array().unwrap().is_empty());
    assert!(
        body["note"]
            .as_str()
            .unwrap()
            .contains("No models are configured")
    );
}
