//! End-to-end tests of the model endpoints, against small upstream servers started in the test.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use common::{closed_port, form, harness, harness_timed, json_body, model_toml, upstream};
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

/// Over a real connection, from a client that sends its whole body before it reads the answer
/// (as Python's `requests` and `httpx` do): it must get the refusal, not a reset, so the bridge
/// reads the body before it answers.
#[tokio::test]
async fn a_refused_client_still_sending_gets_the_answer() {
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
    let address = h.serve().await.replace("http://", "");
    let first = call(
        &h,
        "/v1/embeddings",
        json!({"model": "embed", "input": "a"}),
    );
    let second = async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while up.count() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Larger than the socket buffers, below the 32 MiB limit.
        let body = json!({"model": "embed", "input": "x".repeat(30 * 1024 * 1024)}).to_string();
        let head = format!(
            "POST /v1/embeddings HTTP/1.1\r\nhost: {address}\r\nauthorization: Bearer {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            h.token,
            body.len()
        );
        let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
        let sent = async {
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(body.as_bytes()).await?;
            let mut answer = String::new();
            stream.read_to_string(&mut answer).await?;
            Ok::<_, std::io::Error>(answer)
        }
        .await;
        release.notify_one();
        sent
    };
    let ((status, _, _), refused) = tokio::join!(first, second);
    let refused = refused.expect("the refusal arrives instead of a reset");
    assert!(refused.starts_with("HTTP/1.1 429"), "{refused}");
    assert!(refused.contains("models_busy"), "{refused}");
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

/// One step of a scripted upstream answer.
enum Step {
    Send(&'static str),
    Sleep(u64),
    Wait(Arc<tokio::sync::Notify>),
    Hang,
}

/// Notifies when it is dropped, that is when the upstream's answer is abandoned.
struct Guard(Arc<tokio::sync::Notify>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// An answer body that follows `steps`, holding `guard` until it is dropped.
fn scripted(steps: Vec<Step>, guard: Option<Guard>) -> Body {
    let stream = futures_util::stream::unfold(
        (steps.into_iter(), guard),
        |(mut steps, guard)| async move {
            loop {
                match steps.next()? {
                    Step::Send(text) => {
                        return Some((
                            Ok::<_, std::io::Error>(axum::body::Bytes::from(text)),
                            (steps, guard),
                        ));
                    }
                    Step::Sleep(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                    Step::Wait(notify) => notify.notified().await,
                    Step::Hang => std::future::pending::<()>().await,
                }
            }
        },
    );
    Body::from_stream(stream)
}

/// A chat upstream whose answer to every call is built by `answer`.
async fn scripted_upstream(
    content_type: &'static str,
    answer: impl Fn() -> Body + Clone + Send + Sync + 'static,
) -> common::Upstream {
    upstream(Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let answer = answer.clone();
            async move { ([("content-type", content_type)], answer()) }
        }),
    ))
    .await
}

/// Reads a body to its end or its first error: `(bytes read, whether it ended with an error)`.
async fn read_all(response: axum::response::Response) -> (String, bool) {
    let mut body = response.into_body();
    let mut text = Vec::new();
    loop {
        match body.frame().await {
            None => return (String::from_utf8_lossy(&text).into(), false),
            Some(Err(_)) => return (String::from_utf8_lossy(&text).into(), true),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    text.extend_from_slice(&data);
                }
            }
        }
    }
}

async fn chat(h: &common::Harness) -> axum::response::Response {
    h.send(post_to(
        h,
        "/v1/chat/completions",
        json!({"model": "qwen", "stream": true}).to_string(),
    ))
    .await
}

#[tokio::test]
async fn a_stream_arrives_event_by_event() {
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let up = scripted_upstream("text/event-stream", move || {
        scripted(
            vec![
                Step::Send("data: one\n\n"),
                Step::Wait(gate.clone()),
                Step::Send("data: two\n\n"),
            ],
            None,
        )
    })
    .await;
    let h = harness("[\"files\"]", &model_toml("qwen", "chat", &up.url, "")).await;
    let response = chat(&h).await;
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut body = response.into_body();
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("the first event must arrive before the stream ends")
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap(), "data: one\n\n");
    release.notify_one();
    let rest = body.collect().await.unwrap().to_bytes();
    assert_eq!(rest, "data: two\n\n");
}

#[tokio::test]
async fn silent_streams_get_keep_alive_lines_between_events() {
    let up = scripted_upstream("text/event-stream", || {
        scripted(
            vec![
                Step::Send("data: one\n\n"),
                Step::Sleep(300),
                Step::Send("data: par"),
                Step::Sleep(300),
                Step::Send("tial\r\n\r\n"),
                Step::Sleep(300),
                Step::Send("data: end\n\n"),
            ],
            None,
        )
    })
    .await;
    let timing = Timing {
        keep_alive: Duration::from_millis(50),
        ..Timing::default()
    };
    let h = harness_timed(
        "[\"files\"]",
        &model_toml("qwen", "chat", &up.url, ""),
        timing,
    )
    .await;
    let (text, failed) = read_all(chat(&h).await).await;
    assert!(!failed, "{text:?}");
    assert!(
        text.starts_with("data: one\n\n: keep-alive\n\n"),
        "{text:?}"
    );
    assert!(text.contains("data: partial\r\n\r\n"), "{text:?}");
    let between = &text[text.find("partial\r\n\r\n").unwrap()..text.find("data: end\n\n").unwrap()];
    assert!(between.contains(": keep-alive\n\n"), "{text:?}");
    assert!(text.ends_with("data: end\n\n"), "{text:?}");
}

#[tokio::test]
async fn a_stuck_answer_is_cut() {
    let timing = Timing {
        silence: Duration::from_millis(100),
        stream_silence: Duration::from_millis(300),
        keep_alive: Duration::from_millis(50),
        ..Timing::default()
    };
    let json_up = scripted_upstream("application/json", || {
        scripted(vec![Step::Send("{\"a\":"), Step::Hang], None)
    })
    .await;
    let h = harness_timed(
        "[\"files\"]",
        &model_toml("qwen", "chat", &json_up.url, ""),
        timing.clone(),
    )
    .await;
    let started = std::time::Instant::now();
    let (text, failed) = read_all(chat(&h).await).await;
    assert!(failed && started.elapsed() < Duration::from_secs(1));
    assert_eq!(text, "{\"a\":");

    let sse_up = scripted_upstream("text/event-stream", || {
        scripted(vec![Step::Send("data: one\n\n"), Step::Hang], None)
    })
    .await;
    let h = harness_timed(
        "[\"files\"]",
        &model_toml("qwen", "chat", &sse_up.url, ""),
        timing,
    )
    .await;
    let (text, failed) = read_all(chat(&h).await).await;
    assert!(failed, "{text:?}");
    assert!(text.matches(": keep-alive\n\n").count() >= 2, "{text:?}");
}

#[tokio::test]
async fn hanging_up_ends_the_upstream_call() {
    let dropped = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (guard_notify, counter) = (dropped.clone(), calls.clone());
    let up = scripted_upstream("text/event-stream", move || {
        if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            scripted(
                vec![Step::Send("data: one\n\n"), Step::Hang],
                Some(Guard(guard_notify.clone())),
            )
        } else {
            scripted(vec![Step::Send("data: done\n\n")], None)
        }
    })
    .await;
    let h = harness(
        "[\"files\"]",
        &format!(
            "[limits]\nmax_model_requests = 1\n{}",
            model_toml("qwen", "chat", &up.url, "")
        ),
    )
    .await;
    let mut body = chat(&h).await.into_body();
    let first = body.frame().await.unwrap().unwrap();
    assert_eq!(first.into_data().unwrap(), "data: one\n\n");
    drop(body);
    tokio::time::timeout(Duration::from_secs(1), dropped.notified())
        .await
        .expect("the upstream call must end when the client hangs up");
    let (text, failed) = read_all(chat(&h).await).await;
    assert_eq!((text.as_str(), failed), ("data: done\n\n", false));
}

/// A form part: name, optional file name and content type, content.
#[tokio::test]
async fn a_transcription_form_is_rebuilt_for_the_upstream() {
    let up = upstream(
        Router::new()
            .route(
                "/v1/audio/transcriptions",
                post(|mut form: axum::extract::Multipart| async move {
                    let mut echo = serde_json::Map::new();
                    while let Some(field) = form.next_field().await.unwrap() {
                        let name = field.name().unwrap().to_string();
                        if name == "file" {
                            echo.insert("file_name".into(), json!(field.file_name().unwrap()));
                            echo.insert("file_type".into(), json!(field.content_type().unwrap()));
                            let bytes = field.bytes().await.unwrap();
                            echo.insert("file_len".into(), json!(bytes.len()));
                            let sum: u64 = bytes.iter().map(|b| u64::from(*b)).sum();
                            echo.insert("file_sum".into(), json!(sum));
                        } else {
                            echo.insert(name, json!(field.text().await.unwrap()));
                        }
                    }
                    Json(Value::Object(echo))
                }),
            )
            .layer(axum::extract::DefaultBodyLimit::disable()),
    )
    .await;
    let h = harness(
        "[\"files\"]",
        &format!(
            "{}{}",
            model_toml(
                "whisper",
                "speech-to-text",
                &up.url,
                "model = \"openai/whisper-large-v3\"\napi_key = \"sk-up\"\n\
                 defaults = { language = \"cs\", temperature = 0 }",
            ),
            model_toml("qwen", "chat", &up.url, ""),
        ),
    )
    .await;
    let send = |body: Vec<u8>, content_type: &str| {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/audio/transcriptions")
            .header("authorization", format!("Bearer {}", h.token))
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap();
        h.send(request)
    };
    let audio: Vec<u8> = (0..25_600u32).map(|i| (i % 256) as u8).collect();
    let expected_sum: u64 = audio.iter().map(|b| u64::from(*b)).sum();
    let multipart = "multipart/form-data; boundary=XBOUND";

    // The order `curl -F file=@a.wav -F model=whisper` sends: the file before the model.
    let body = form(&[
        ("file", Some(("a.wav", "audio/wav")), &audio),
        ("model", None, b"whisper"),
        ("language", None, b"en"),
    ]);
    let response = send(body, multipart).await;
    let status = response.status();
    let echo = json_body(response).await;
    assert_eq!(status, StatusCode::OK, "{echo}");
    assert_eq!(
        echo,
        json!({"model": "openai/whisper-large-v3", "language": "en", "temperature": "0",
            "file_name": "a.wav", "file_type": "audio/wav", "file_len": 25600, "file_sum": expected_sum})
    );
    let seen = up.last();
    assert_eq!(seen.headers["authorization"], "Bearer sk-up");
    let content_type = seen.headers["content-type"].to_str().unwrap();
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "{content_type}"
    );
    assert!(!content_type.contains("XBOUND"));
    // Above the 1 MiB limit of the other endpoints.
    let big = vec![7u8; 2 * 1024 * 1024];
    let body = form(&[
        ("model", None, b"whisper"),
        ("file", Some(("b.wav", "audio/wav")), &big),
    ]);
    let response = send(body, multipart).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["file_len"], 2 * 1024 * 1024);

    let refusals = [
        (
            form(&[("file", Some(("a.wav", "audio/wav")), &audio)]),
            multipart,
            "invalid_request",
            "`model`",
        ),
        (
            form(&[
                ("model", None, b"qwen"),
                ("file", Some(("a.wav", "audio/wav")), &audio),
            ]),
            multipart,
            "wrong_model_type",
            "POST /v1/chat/completions",
        ),
        (
            json!({"model": "whisper"}).to_string().into_bytes(),
            "application/json",
            "invalid_request",
            "multipart",
        ),
    ];
    for (body, content_type, code, fragment) in refusals {
        let response = send(body, content_type).await;
        let status = response.status();
        let body = json_body(response).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some(code)),
            "{body}"
        );
        assert!(message(&body).contains(fragment), "{body}");
    }

    let request = Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", multipart)
        .header("content-length", "96000001")
        .body(Body::empty())
        .unwrap();
    let response = h.send(request).await;
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::PAYLOAD_TOO_LARGE, Some("too_large"))
    );
    assert!(message(&body).contains("96 MB"), "{body}");
    assert_eq!(up.count(), 2);
}

#[tokio::test]
async fn probes_tell_the_states_of_an_upstream_apart() {
    use axum::routing::get;
    use safe_grokbot_bridge::config::Config;
    use safe_grokbot_bridge::models::{Models, Probe};

    let up = upstream(
        Router::new()
            .route(
                "/v1/models",
                get(|headers: HeaderMap| async move {
                    if headers
                        .get("authorization")
                        .is_none_or(|v| v != "Bearer good")
                    {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    Json(json!({"data": [{"id": "Qwen/Qwen3-32B"}, {"id": "other"}]}))
                        .into_response()
                }),
            )
            .route(
                "/a/models/claude-x",
                get(|headers: HeaderMap| async move {
                    if headers.get("x-api-key").is_none_or(|v| v != "good") {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    if headers.get("anthropic-version").is_none() {
                        return StatusCode::BAD_REQUEST.into_response();
                    }
                    Json(json!({"id": "claude-x"})).into_response()
                }),
            ),
    )
    .await;
    let base = up.url.trim_end_matches("/v1").to_string();
    let closed = format!("http://127.0.0.1:{}/v1", closed_port());
    let models = [
        (
            "ok",
            format!("{base}/v1"),
            "Qwen/Qwen3-32B",
            "good",
            "openai",
        ),
        (
            "badkey",
            format!("{base}/v1"),
            "Qwen/Qwen3-32B",
            "bad",
            "openai",
        ),
        ("absent", format!("{base}/v1"), "absent", "good", "openai"),
        ("nolist", format!("{base}/none"), "x", "good", "openai"),
        ("down", closed, "x", "good", "openai"),
        (
            "claude",
            format!("{base}/a"),
            "claude-x",
            "good",
            "anthropic",
        ),
        (
            "missing",
            format!("{base}/a"),
            "missing",
            "good",
            "anthropic",
        ),
    ];
    let mut text = format!(
        "[[tokens]]\nname = \"t\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\n",
        "0".repeat(64),
        common::toml_str(&std::env::temp_dir())
    );
    for (name, url, model, key, api) in &models {
        text.push_str(&model_toml(
            name,
            "chat",
            url,
            &format!("model = \"{model}\"\napi_key = \"{key}\"\napi = \"{api}\""),
        ));
    }
    let config = Config::parse(&text, std::path::Path::new("t.toml")).unwrap();
    let client = Models::new(&config, Timing::default()).unwrap();
    let probe = |name: &str| client.probe(config.model(name).unwrap());

    assert_eq!(probe("ok").await, Probe::Ok);
    assert_eq!(probe("badkey").await, Probe::KeyRejected);
    assert_eq!(
        probe("absent").await,
        Probe::UnknownModel {
            listed: vec!["Qwen/Qwen3-32B".into(), "other".into()]
        }
    );
    assert_eq!(probe("nolist").await, Probe::Unverifiable(404));
    assert!(matches!(probe("down").await, Probe::Unreachable(_)));
    assert_eq!(probe("claude").await, Probe::Ok);
    assert_eq!(
        probe("missing").await,
        Probe::UnknownModel { listed: vec![] }
    );
}
