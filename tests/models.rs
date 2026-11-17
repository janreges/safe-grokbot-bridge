//! End-to-end tests of the model endpoints, against small upstream servers started in the test.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use common::{closed_port, harness, harness_timed, json_body, model_toml, upstream};
use http_body_util::BodyExt;
use safe_grokbot_bridge::models::Timing;
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

fn post_to(h: &common::Harness, path: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "application/json")
        .body(body.into())
        .unwrap()
}

async fn call(h: &common::Harness, path: &str, body: Value) -> (StatusCode, HeaderMap, Value) {
    let response = h.send(post_to(h, path, body.to_string())).await;
    let status = response.status();
    let headers = response.headers().clone();
    (status, headers, json_body(response).await)
}

fn message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_default()
}

/// An upstream that answers `{}` on every path.
async fn empty_upstream() -> common::Upstream {
    upstream(Router::new().fallback(|| async { Json(json!({})) })).await
}

#[tokio::test]
async fn a_chat_call_reaches_the_upstream_rewritten() {
    let up = upstream(Router::new().route(
        "/v1/chat/completions",
        post(|| async { Json(json!({"id": "c1", "model": "Qwen/Qwen3-32B"})) }),
    ))
    .await;
    let h = harness(
        "[\"files\"]",
        &model_toml(
            "qwen",
            "chat",
            &up.url,
            "model = \"Qwen/Qwen3-32B\"\napi_key = \"sk-up\"\n\
             defaults = { chat_template_kwargs = { enable_thinking = false }, temperature = 0.7 }",
        ),
    )
    .await;
    let body = json!({"model": "qwen", "messages": [{"role": "user", "content": "hi"}], "temperature": 0.2,
        "response_format": {"type": "json_schema", "json_schema": {"schema": {"properties": {"zeta": {}, "alpha": {}}}}}});
    // A query string as Anthropic's library adds it, no Content-Type as with `curl -d`, and
    // headers that must not travel on.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions?beta=true")
        .header("authorization", format!("Bearer {}", h.token))
        .header("cookie", "a=b")
        .header("cf-connecting-ip", "203.0.113.9")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = h.send(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({"id": "c1", "model": "Qwen/Qwen3-32B"})
    );

    let seen = up.last();
    assert_eq!(seen.uri, "/v1/chat/completions");
    let sent: Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(sent["model"], "Qwen/Qwen3-32B");
    assert_eq!(sent["temperature"], 0.2);
    assert_eq!(sent["chat_template_kwargs"]["enable_thinking"], false);
    let text = String::from_utf8_lossy(&seen.body);
    assert!(
        text.find("zeta").unwrap() < text.find("alpha").unwrap(),
        "member order must survive"
    );
    assert_eq!(seen.headers["authorization"], "Bearer sk-up");
    assert_eq!(seen.headers["content-type"], "application/json");
    for name in ["cookie", "cf-connecting-ip", "x-api-key"] {
        assert!(seen.headers.get(name).is_none(), "{name}");
    }
    assert!(!format!("{:?}{text}", seen.headers).contains(&h.token));
}

#[tokio::test]
async fn an_anthropic_call_uses_its_own_headers() {
    let up =
        upstream(Router::new().route("/v1/messages", post(|| async { Json(json!({"id": "m1"})) })))
            .await;
    let h = harness(
        "[\"files\"]",
        &model_toml(
            "opus",
            "chat",
            &up.url,
            "api = \"anthropic\"\napi_key = \"sk-ant\"",
        ),
    )
    .await;
    let send = |version: Option<&'static str>| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("x-api-key", h.token.clone())
            .header("anthropic-beta", "tools-x")
            .header("content-type", "application/json");
        if let Some(version) = version {
            request = request.header("anthropic-version", version);
        }
        let body = json!({"model": "opus", "max_tokens": 5, "messages": []}).to_string();
        h.send(request.body(Body::from(body)).unwrap())
    };
    assert_eq!(send(None).await.status(), StatusCode::OK);
    let seen = up.last();
    assert_eq!(seen.uri, "/v1/messages");
    assert_eq!(seen.headers["x-api-key"], "sk-ant");
    assert_eq!(seen.headers["anthropic-version"], "2023-06-01");
    assert_eq!(seen.headers["anthropic-beta"], "tools-x");
    assert!(seen.headers.get("authorization").is_none());

    assert_eq!(send(Some("2024-10-22")).await.status(), StatusCode::OK);
    assert_eq!(up.last().headers["anthropic-version"], "2024-10-22");
}

#[tokio::test]
async fn every_endpoint_takes_only_its_model_types() {
    let up = empty_upstream().await;
    let models = [
        ("qwen", "chat", ""),
        ("eye", "vision", ""),
        ("opus", "chat", "api = \"anthropic\""),
        ("embed", "embedding", ""),
        ("rr", "reranker", ""),
        ("tts", "text-to-speech", ""),
        ("img", "image-generator", ""),
    ]
    .map(|(name, kind, extra)| model_toml(name, kind, &up.url, extra))
    .concat();
    let h = harness("[\"files\"]", &models).await;
    let cases: [(&str, &[&str], &str, &str); 6] = [
        (
            "/v1/chat/completions",
            &["qwen", "eye"],
            "embed",
            "POST /v1/embeddings",
        ),
        (
            "/v1/messages",
            &["opus"],
            "qwen",
            "POST /v1/chat/completions",
        ),
        ("/v1/embeddings", &["embed"], "rr", "POST /v1/rerank"),
        ("/v1/rerank", &["rr"], "tts", "POST /v1/audio/speech"),
        (
            "/v1/audio/speech",
            &["tts"],
            "img",
            "POST /v1/images/generations",
        ),
        (
            "/v1/images/generations",
            &["img"],
            "qwen",
            "POST /v1/chat/completions",
        ),
    ];
    for (path, accepted, refused, hint) in cases {
        for name in accepted {
            let (status, _, body) = call(&h, path, json!({"model": name})).await;
            assert_eq!(status, StatusCode::OK, "{path} {name}: {body}");
            assert_eq!(up.last().uri, path);
        }
        let (status, _, body) = call(&h, path, json!({"model": refused})).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("wrong_model_type")),
            "{path} {refused}"
        );
        assert!(message(&body).contains(hint), "{body}");
    }
}

#[tokio::test]
async fn bad_bodies_and_unknown_models_are_explained() {
    let up = empty_upstream().await;
    let (_, other) = common::extra_token("other", "");
    let h = harness(
        "[\"files\"]",
        &format!(
            "{}{}{other}",
            model_toml("qwen", "chat", &up.url, ""),
            model_toml("paid", "chat", &up.url, "tokens = [\"other\"]"),
        ),
    )
    .await;
    for body in ["not json", "[]", "{}", "{\"model\": 5}"] {
        let response = h.send(post_to(&h, "/v1/chat/completions", body)).await;
        let status = response.status();
        let body = json_body(response).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_request")),
            "{body}"
        );
        assert!(message(&body).contains("GET /v1/models"), "{body}");
    }
    let (status, _, body) = call(&h, "/v1/chat/completions", json!({"model": "nope"})).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("model_not_found"))
    );
    assert!(message(&body).contains("`qwen`"), "{body}");
    let (status, _, body) = call(&h, "/v1/chat/completions", json!({"model": "paid"})).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden"))
    );
    assert_eq!(up.count(), 0);
}

#[tokio::test]
async fn upstream_answers_pass_through_or_are_explained() {
    let up = upstream(Router::new().route(
        "/v1/embeddings",
        post(|Json(body): Json<Value>| async move {
            match body["input"].as_str().unwrap() {
                "bad" => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": {"message": "too long"}})),
                )
                    .into_response(),
                "busy" => (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "7")],
                    Json(json!({})),
                )
                    .into_response(),
                "empty" => StatusCode::NO_CONTENT.into_response(),
                "denied" => StatusCode::UNAUTHORIZED.into_response(),
                "gone" => {
                    (StatusCode::NOT_FOUND, Json(json!({"detail": "Not Found"}))).into_response()
                }
                "moved" => (
                    StatusCode::TEMPORARY_REDIRECT,
                    [("location", "/v1/elsewhere")],
                )
                    .into_response(),
                other => panic!("unexpected input {other}"),
            }
        }),
    ))
    .await;
    let down = format!("http://127.0.0.1:{}/v1", closed_port());
    let h = harness(
        "[\"files\"]",
        &format!(
            "{}{}",
            model_toml("embed", "embedding", &up.url, ""),
            model_toml("down", "embedding", &down, "")
        ),
    )
    .await;
    let embed = |input: &str| {
        call(
            &h,
            "/v1/embeddings",
            json!({"model": "embed", "input": input}),
        )
    };

    let (status, _, body) = embed("bad").await;
    assert_eq!(
        (status, body),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "too long"}})
        )
    );
    let (status, headers, _) = embed("busy").await;
    assert_eq!(
        (status, headers["retry-after"].to_str().unwrap()),
        (StatusCode::TOO_MANY_REQUESTS, "7")
    );
    let response = h
        .send(post_to(
            &h,
            "/v1/embeddings",
            json!({"model": "embed", "input": "empty"}).to_string(),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );

    let explained = [
        ("denied", "upstream_auth_failed", vec!["[models.embed]"]),
        ("gone", "upstream_misconfigured", vec!["404", "Not Found"]),
        ("moved", "upstream_misconfigured", vec!["307"]),
    ];
    for (input, code, fragments) in explained {
        let (status, headers, body) = embed(input).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (StatusCode::BAD_GATEWAY, Some(code)),
            "{input}: {body}"
        );
        assert_eq!(headers["x-should-retry"], "false", "{input}");
        for fragment in fragments {
            assert!(message(&body).contains(fragment), "{input}: {body}");
        }
    }
    assert!(
        !up.seen
            .lock()
            .unwrap()
            .iter()
            .any(|seen| seen.uri == "/v1/elsewhere")
    );

    let (status, headers, body) =
        call(&h, "/v1/embeddings", json!({"model": "down", "input": "x"})).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::BAD_GATEWAY, Some("upstream_unreachable")),
        "{body}"
    );
    assert_eq!(headers["x-should-retry"], "false");
    assert!(!message(&body).contains("127.0.0.1"), "{body}");
}

#[tokio::test]
async fn binary_answers_arrive_intact() {
    let up = upstream(Router::new().route(
        "/v1/audio/speech",
        post(|| async {
            (
                [("content-type", "audio/mpeg")],
                vec![0u8, 159, 146, 150, 255],
            )
        }),
    ))
    .await;
    let h = harness(
        "[\"files\"]",
        &model_toml("tts", "text-to-speech", &up.url, ""),
    )
    .await;
    let response = h
        .send(post_to(
            &h,
            "/v1/audio/speech",
            json!({"model": "tts", "input": "hi"}).to_string(),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "audio/mpeg");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], [0u8, 159, 146, 150, 255]);
}

#[tokio::test]
async fn body_limits_differ_by_endpoint() {
    let up = empty_upstream().await;
    let h = harness(
        "[\"sessions\", \"start\"]",
        &model_toml("embed", "embedding", &up.url, ""),
    )
    .await;
    let big = |mib: usize| json!({"model": "embed", "input": "x".repeat(mib * 1024 * 1024)});
    let (status, _, _) = call(&h, "/v1/embeddings", big(2)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = call(&h, "/v1/embeddings", big(33)).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::PAYLOAD_TOO_LARGE, Some("too_large"))
    );
    assert!(message(&body).contains("32 MiB"), "{body}");
    let (status, _, _) = call(&h, "/v1/sessions", big(2)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_busy_bridge_answers_models_busy() {
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let up = upstream(Router::new().route(
        "/v1/embeddings",
        post(move || {
            let gate = gate.clone();
            async move {
                gate.notified().await;
                Json(json!({"ok": true}))
            }
        }),
    ))
    .await;
    let timing = Timing {
        slot_wait: Duration::from_millis(100),
        ..Timing::default()
    };
    let h = harness_timed(
        "[\"files\"]",
        &format!(
            "[limits]\nmax_model_requests = 1\n{}",
            model_toml("embed", "embedding", &up.url, "")
        ),
        timing,
    )
    .await;
    let first = call(
        &h,
        "/v1/embeddings",
        json!({"model": "embed", "input": "a"}),
    );
    let second = async {
        while up.count() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let big = json!({"model": "embed", "input": "x".repeat(2 * 1024 * 1024)});
        let busy = call(&h, "/v1/embeddings", big).await;
        release.notify_one();
        busy
    };
    let ((status, _, _), (busy_status, headers, body)) = tokio::join!(first, second);
    assert_eq!(
        (busy_status, body["error"]["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("models_busy")),
        "{body}"
    );
    assert_eq!(headers["retry-after"], "5");
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_silent_upstream_is_reported_in_time() {
    let up = upstream(Router::new().fallback(|| async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        Json(json!({}))
    }))
    .await;
    let timing = Timing {
        first_byte: Duration::from_millis(100),
        ..Timing::default()
    };
    let h = harness_timed(
        "[\"files\"]",
        &format!(
            "{}{}",
            model_toml("qwen", "chat", &up.url, ""),
            model_toml("embed", "embedding", &up.url, "")
        ),
        timing,
    )
    .await;
    let started = std::time::Instant::now();
    let (status, headers, body) = call(&h, "/v1/chat/completions", json!({"model": "qwen"})).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::GATEWAY_TIMEOUT, Some("upstream_timeout"))
    );
    assert_eq!(headers["x-should-retry"], "false");
    assert!(message(&body).contains("stream: true"), "{body}");
    let (status, _, body) = call(&h, "/v1/embeddings", json!({"model": "embed"})).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(!message(&body).contains("stream: true"), "{body}");
    assert!(started.elapsed() < Duration::from_secs(2));
}
