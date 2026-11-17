//! Models behind the bridge: the endpoint table, model selection, defaults, the model list and
//! the relay of calls to the upstream servers.
//!
//! Everything the bridge knows about upstream model APIs (paths, auth header styles, the
//! Anthropic version header) lives in this module, the way agent knowledge lives in `agents.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use utoipa::ToSchema;

use crate::auth::Caller;
use crate::config::{Config, Model, ModelApi, ModelType};
use crate::error::ApiError;
use crate::server::AppState;

/// Body limit of a model call with a JSON body: images travel inside chat requests as base64.
pub const MAX_JSON_BODY: usize = 32 * 1024 * 1024;
/// Sent when the client names no version; Anthropic's API requires the header.
const ANTHROPIC_VERSION: &str = "2023-06-01";
const BUSY_RETRY_SECS: u64 = 5;

/// The time rules of model calls. Constants in production; tests shorten them.
#[derive(Clone, Debug)]
pub struct Timing {
    /// How long a call waits for a free slot before `models_busy`.
    pub slot_wait: Duration,
    /// Connecting to an upstream.
    pub connect: Duration,
    /// From sending the request to the head of the answer. With the slot wait it stays under
    /// Cloudflare's 125 s, so the caller gets the bridge's error and not Cloudflare's page.
    pub first_byte: Duration,
    /// Silence inside an answer that is not an event stream.
    pub silence: Duration,
    /// Silence inside an event stream, bridged by keep-alive lines.
    pub stream_silence: Duration,
    /// Interval of keep-alive lines in a silent event stream.
    pub keep_alive: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            slot_wait: Duration::from_secs(10),
            connect: Duration::from_secs(5),
            first_byte: Duration::from_secs(90),
            silence: Duration::from_secs(90),
            stream_silence: Duration::from_secs(600),
            keep_alive: Duration::from_secs(20),
        }
    }
}

/// The HTTP client and the slots shared by all model calls.
pub struct Models {
    client: reqwest::Client,
    slots: Arc<Semaphore>,
    timing: Timing,
}

impl Models {
    pub fn new(config: &Config, timing: Timing) -> anyhow::Result<Models> {
        // reqwest is built without a crypto provider of its own so that TLS stays on `ring`, like
        // the server's; it panics unless a process default is installed first.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            // Otherwise HTTP_PROXY and its relatives from the environment would route model calls.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timing.connect)
            .build()
            .context("cannot create the HTTP client for model calls")?;
        Ok(Models {
            client,
            slots: Arc::new(Semaphore::new(config.limits.max_model_requests)),
            timing,
        })
    }

    async fn slot(&self) -> Option<OwnedSemaphorePermit> {
        let acquire = self.slots.clone().acquire_owned();
        tokio::time::timeout(self.timing.slot_wait, acquire)
            .await
            .ok()?
            .ok()
    }
}

/// A model endpoint of the bridge and the upstream path it forwards to.
pub struct Endpoint {
    /// On the bridge, e.g. `/v1/chat/completions`.
    pub path: &'static str,
    /// After a model's `url`, e.g. `/chat/completions`.
    pub upstream_path: &'static str,
    pub types: &'static [ModelType],
    pub api: ModelApi,
    /// The upstream API has a stream mode, so a long answer can start early.
    pub streams: bool,
}

impl Endpoint {
    fn accepts(&self, model: &Model) -> bool {
        self.types.contains(&model.kind) && self.api == model.api
    }
}

pub const CHAT_COMPLETIONS: Endpoint = Endpoint {
    path: "/v1/chat/completions",
    upstream_path: "/chat/completions",
    types: &[ModelType::Chat, ModelType::Vision],
    api: ModelApi::Openai,
    streams: true,
};
pub const MESSAGES: Endpoint = Endpoint {
    path: "/v1/messages",
    upstream_path: "/messages",
    types: &[ModelType::Chat, ModelType::Vision],
    api: ModelApi::Anthropic,
    streams: true,
};
pub const EMBEDDINGS: Endpoint = Endpoint {
    path: "/v1/embeddings",
    upstream_path: "/embeddings",
    types: &[ModelType::Embedding],
    api: ModelApi::Openai,
    streams: false,
};
pub const RERANK: Endpoint = Endpoint {
    path: "/v1/rerank",
    upstream_path: "/rerank",
    types: &[ModelType::Reranker],
    api: ModelApi::Openai,
    streams: false,
};
pub const TRANSCRIPTIONS: Endpoint = Endpoint {
    path: "/v1/audio/transcriptions",
    upstream_path: "/audio/transcriptions",
    types: &[ModelType::SpeechToText],
    api: ModelApi::Openai,
    streams: true,
};
pub const SPEECH: Endpoint = Endpoint {
    path: "/v1/audio/speech",
    upstream_path: "/audio/speech",
    types: &[ModelType::TextToSpeech],
    api: ModelApi::Openai,
    streams: false,
};
pub const IMAGES: Endpoint = Endpoint {
    path: "/v1/images/generations",
    upstream_path: "/images/generations",
    types: &[ModelType::ImageGenerator],
    api: ModelApi::Openai,
    streams: false,
};

const ENDPOINTS: [&Endpoint; 7] = [
    &CHAT_COMPLETIONS,
    &MESSAGES,
    &EMBEDDINGS,
    &RERANK,
    &TRANSCRIPTIONS,
    &SPEECH,
    &IMAGES,
];

/// The endpoint that calls a model. Config validation guarantees there is one.
pub fn endpoint_of(model: &Model) -> &'static Endpoint {
    ENDPOINTS
        .into_iter()
        .find(|endpoint| endpoint.accepts(model))
        .expect("config validation leaves no model without an endpoint")
}

/// Fills in the defaults the client did not send. Objects are merged key by key at every depth;
/// any other value of the client's wins as a whole.
pub fn merge_defaults(body: &mut Map<String, Value>, defaults: &Map<String, Value>) {
    for (key, default) in defaults {
        match (body.get_mut(key), default) {
            (None, _) => {
                body.insert(key.clone(), default.clone());
            }
            (Some(Value::Object(client)), Value::Object(default)) => {
                merge_defaults(client, default)
            }
            (Some(_), _) => {}
        }
    }
}

/// The model a call names, if this endpoint may call it for this token.
pub fn select<'a>(
    config: &'a Config,
    endpoint: &Endpoint,
    name: &str,
    token: &str,
) -> Result<&'a Model, ApiError> {
    let Some(model) = config.model(name) else {
        let shown: String = name.chars().take(100).collect();
        let callable: Vec<String> = config
            .models
            .iter()
            .filter(|m| endpoint.accepts(m) && m.allows_token(token))
            .map(|m| format!("`{}`", m.name))
            .collect();
        let message = if callable.is_empty() {
            format!(
                "There is no model `{shown}`, and this token may call no model through {}. GET /v1/models lists the models it may call and their endpoints.",
                endpoint.path
            )
        } else {
            format!(
                "There is no model `{shown}`. Models this token may call through {}: {}. GET /v1/models lists every model with its endpoint.",
                endpoint.path,
                callable.join(", ")
            )
        };
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "model_not_found",
            message,
        ));
    };
    if !model.allows_token(token) {
        return Err(ApiError::forbidden(format!(
            "Model `{name}` is not open to this token. GET /v1/models lists the models it may call."
        )));
    }
    if !endpoint.accepts(model) {
        let api = match model.api {
            ModelApi::Openai => "",
            ModelApi::Anthropic => " with the Anthropic API",
        };
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "wrong_model_type",
            format!(
                "`{name}` is a {} model{api}; call it with POST {}.",
                model.kind.as_str(),
                endpoint_of(model).path
            ),
        ));
    }
    Ok(model)
}

/// A model you may call through this bridge.
#[derive(Serialize, ToSchema, Debug)]
pub struct ModelInfo {
    /// The name to send as `model`.
    pub id: String,
    /// Always `model`.
    pub object: String,
    /// When the bridge started, in Unix seconds; client libraries expect the field.
    pub created: u64,
    /// Always `safe-grokbot-bridge`.
    pub owned_by: String,
    /// What the model does: `chat`, `vision` (a chat model that also accepts images),
    /// `embedding`, `reranker`, `speech-to-text`, `text-to-speech` or `image-generator`.
    pub r#type: ModelType,
    /// The request format: `openai` or `anthropic`.
    pub api: ModelApi,
    /// The endpoint that calls this model, e.g. `/v1/chat/completions`.
    pub endpoint: String,
    /// The upstream's own name for the model; the bridge sends it in place of `id`.
    pub upstream_model: String,
    /// What the model is good for, written by the machine's owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Request parameters the bridge fills in when your request does not set them. Your values
    /// win; objects are merged key by key.
    #[schema(value_type = Object)]
    pub defaults: Map<String, Value>,
}

/// The models you may call, in the shape of OpenAI's model list.
#[derive(Serialize, ToSchema, Debug)]
pub struct ModelList {
    /// Always `list`.
    pub object: String,
    /// Sorted by `id`.
    pub data: Vec<ModelInfo>,
    /// Why the list is empty, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub fn info(model: &Model, created: u64) -> ModelInfo {
    ModelInfo {
        id: model.name.clone(),
        object: "model".into(),
        created,
        owned_by: "safe-grokbot-bridge".into(),
        r#type: model.kind,
        api: model.api,
        endpoint: endpoint_of(model).path.into(),
        upstream_model: model.upstream_model.clone(),
        description: model.description.clone(),
        defaults: model.defaults.clone(),
    }
}

/// The models a token may call, optionally of one type, in name order.
pub fn list(config: &Config, token: &str, kind: Option<ModelType>, created: u64) -> ModelList {
    let of_kind = |m: &&Model| kind.is_none_or(|kind| m.kind == kind);
    let data: Vec<ModelInfo> = config
        .models
        .iter()
        .filter(of_kind)
        .filter(|m| m.allows_token(token))
        .map(|m| info(m, created))
        .collect();
    let note = data.is_empty().then(|| {
        let kind_text = kind.map(|k| format!(" {}", k.as_str())).unwrap_or_default();
        if config.models.is_empty() {
            "No models are configured on this machine. The owner adds them as [models.<name>] tables in the bridge's config file.".to_string()
        } else if !config.models.iter().any(|m| of_kind(&m)) {
            format!("No{kind_text} model is configured on this machine. GET /v1/models without `type` lists the others.")
        } else {
            format!("This token may call no{kind_text} model here; the owner limits some models to named tokens.")
        }
    });
    ModelList {
        object: "list".into(),
        data,
        note,
    }
}

// ---------------------------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------------------------

/// One model call with a JSON body, from the unread request to the relayed answer.
pub async fn call_json(
    app: &AppState,
    endpoint: &'static Endpoint,
    caller: &Caller,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    // The slot comes before the body, so that memory is bounded by the number of slots.
    let Some(slot) = app.models.slot().await else {
        drain(body, MAX_JSON_BODY).await;
        return busy(&app.config).into_response();
    };
    let bytes = match read_body(body, MAX_JSON_BODY).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return ApiError::too_large(
                "The request body is larger than 32 MiB, the limit of a model call. Send fewer or smaller images, or split the input into several calls.",
            )
            .into_response();
        }
        Err(err) => {
            return ApiError::bad_request(format!("The request body could not be read: {err}"))
                .into_response();
        }
    };
    let (model, body) = match prepare_json(&app.config, endpoint, &caller.token, &bytes) {
        Ok(prepared) => prepared,
        Err(err) => return err.into_response(),
    };
    let request = app
        .models
        .client
        .post(format!("{}{}", model.url, endpoint.upstream_path))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body);
    let request = authorize(request, model, &parts.headers);
    forward(app, endpoint, model, request, slot).await
}

/// Picks the model a JSON body names and rewrites the body for its upstream.
fn prepare_json<'a>(
    config: &'a Config,
    endpoint: &Endpoint,
    token: &str,
    bytes: &[u8],
) -> Result<(&'a Model, Vec<u8>), ApiError> {
    let invalid = |detail: String| {
        ApiError::bad_request(format!(
            "The body must be a JSON object with a string `model`, the name of a model from GET /v1/models; {detail}"
        ))
    };
    let mut body: Map<String, Value> = serde_json::from_slice(bytes)
        .map_err(|err| invalid(format!("this one could not be read as such: {err}.")))?;
    let Some(Value::String(name)) = body.get("model") else {
        return Err(invalid("`model` is missing or not a string.".into()));
    };
    let model = select(config, endpoint, &name.clone(), token)?;
    body.insert("model".into(), Value::String(model.upstream_model.clone()));
    merge_defaults(&mut body, &model.defaults);
    let body = serde_json::to_vec(&body).map_err(ApiError::internal)?;
    Ok((model, body))
}

/// Adds the model's key and API headers. Nothing else of the client's request travels on: its
/// `Authorization` and `x-api-key` carry the bridge token.
fn authorize(
    request: reqwest::RequestBuilder,
    model: &Model,
    client: &HeaderMap,
) -> reqwest::RequestBuilder {
    match model.api {
        ModelApi::Openai => match &model.api_key {
            Some(key) => request.bearer_auth(key.expose()),
            None => request,
        },
        ModelApi::Anthropic => {
            let mut request = request;
            if let Some(key) = &model.api_key {
                request = request.header("x-api-key", key.expose());
            }
            let version = client
                .get("anthropic-version")
                .cloned()
                .unwrap_or(HeaderValue::from_static(ANTHROPIC_VERSION));
            request = request.header("anthropic-version", version);
            if let Some(beta) = client.get("anthropic-beta") {
                request = request.header("anthropic-beta", beta.clone());
            }
            request
        }
    }
}

/// Sends a prepared upstream request and relays the answer, or explains why there is none.
async fn forward(
    app: &AppState,
    endpoint: &'static Endpoint,
    model: &Model,
    request: reqwest::RequestBuilder,
    slot: OwnedSemaphorePermit,
) -> Response {
    let timing = &app.models.timing;
    let started = Instant::now();
    let upstream = match tokio::time::timeout(timing.first_byte, request.send()).await {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(err)) => return upstream_error(model, started, unreachable(model, &err)),
        Err(_) => {
            let advice = if endpoint.streams {
                "For long generations send `stream: true`: a stream starts at once, and the bridge keeps it alive through pauses."
            } else {
                "A call that takes longer cannot pass through the bridge, because the connection is cut when it stays silent; split the work into smaller calls."
            };
            let err = ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                format!(
                    "Model `{}` did not start to answer within {}. {advice}",
                    model.name,
                    humantime::format_duration(timing.first_byte)
                ),
            );
            return upstream_error(model, started, err);
        }
    };
    let status = upstream.status();
    tracing::info!(
        model = %model.name,
        status = status.as_u16(),
        ms = started.elapsed().as_millis() as u64,
        "model call"
    );
    let table = format!("[models.{}]", model.name);
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        let err = ApiError::new(
            StatusCode::BAD_GATEWAY,
            "upstream_auth_failed",
            format!(
                "The server behind model `{}` rejected the bridge's key with HTTP {status}: `api_key` in {table} of the bridge's config is wrong or missing. Tell the machine's owner; retrying will not help, and your bridge token is fine.",
                model.name
            ),
        );
        return upstream_error(model, started, err);
    }
    if status.is_redirection() || status == StatusCode::NOT_FOUND {
        let start = excerpt(upstream, timing.silence).await;
        let what = if status.is_redirection() {
            "a redirect, which the bridge does not follow".to_string()
        } else if start.is_empty() {
            "with an empty body".to_string()
        } else {
            format!("starting `{start}`")
        };
        let err = ApiError::new(
            StatusCode::BAD_GATEWAY,
            "upstream_misconfigured",
            format!(
                "The server behind model `{}` answered HTTP {status} ({what}) for {}. This means that `url` or `model` in {table} of the bridge's config is wrong (a typo, a missing `/v1`, or `http` where the server wants `https`). Tell the machine's owner; retrying will not help.",
                model.name, endpoint.upstream_path
            ),
        );
        return upstream_error(model, started, err);
    }
    relay(upstream, slot, model, timing)
}

/// The start of an answer the bridge explains instead of relaying: at most 300 characters.
async fn excerpt(mut upstream: reqwest::Response, wait: Duration) -> String {
    let mut bytes = Vec::new();
    let read = async {
        while bytes.len() < 4096 {
            match upstream.chunk().await {
                Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
                _ => break,
            }
        }
    };
    let _ = tokio::time::timeout(wait, read).await;
    let text: String = String::from_utf8_lossy(&bytes).chars().take(300).collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn unreachable(model: &Model, err: &reqwest::Error) -> ApiError {
    if err.is_builder() {
        return ApiError::new(
            StatusCode::BAD_GATEWAY,
            "upstream_misconfigured",
            format!(
                "The request to model `{}` could not be built; `url` or `api_key` in [models.{}] of the bridge's config is invalid. Tell the machine's owner.",
                model.name, model.name
            ),
        );
    }
    // The error's own text names the upstream URL, which stays private; its causes do not.
    let mut causes = Vec::new();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        causes.push(cause.to_string());
        source = cause.source();
    }
    let reason = if causes.is_empty() {
        "the connection failed".to_string()
    } else {
        causes.join(": ")
    };
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "upstream_unreachable",
        format!(
            "The server behind model `{}` cannot be reached ({reason}). It may be switched off, or `url` in [models.{}] of the bridge's config is wrong. Try another model or tell the machine's owner.",
            model.name, model.name
        ),
    )
}

/// The bridge's own answer for an upstream failure. Client libraries retry a 5xx twice unless
/// told not to, and such a call cannot succeed on a retry.
fn upstream_error(model: &Model, started: Instant, err: ApiError) -> Response {
    tracing::warn!(
        model = %model.name,
        code = err.code,
        ms = started.elapsed().as_millis() as u64,
        "model call failed"
    );
    let mut response = err.into_response();
    response
        .headers_mut()
        .insert("x-should-retry", HeaderValue::from_static("false"));
    response
}

fn busy(config: &Config) -> ApiError {
    ApiError::too_many(
        "models_busy",
        format!(
            "All {} model slots of this bridge stayed taken for 10 seconds. Wait `Retry-After` seconds and try again, or run fewer calls at once.",
            config.limits.max_model_requests
        ),
        BUSY_RETRY_SECS,
    )
}

/// Relays the upstream's status, content type, `Retry-After` and body as it arrives. The body
/// owns the upstream response and the slot, so dropping it ends the call and frees the slot.
fn relay(
    upstream: reqwest::Response,
    slot: OwnedSemaphorePermit,
    model: &Model,
    timing: &Timing,
) -> Response {
    let mut response = Response::builder().status(upstream.status());
    for name in [header::CONTENT_TYPE, header::RETRY_AFTER] {
        if let Some(value) = upstream.headers().get(&name) {
            response = response.header(name, value.clone());
        }
    }
    let events = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let state = Relay {
        upstream,
        _slot: slot,
        model: model.name.clone(),
        timing: timing.clone(),
        events,
        tail: Vec::new(),
        silent: Duration::ZERO,
        done: false,
    };
    let body = futures_util::stream::unfold(state, |mut state| async move {
        let item = state.next().await?;
        Some((item, state))
    });
    response
        .body(Body::from_stream(body))
        .unwrap_or_else(|err| ApiError::internal(err).into_response())
}

struct Relay {
    upstream: reqwest::Response,
    _slot: OwnedSemaphorePermit,
    model: String,
    timing: Timing,
    /// The answer is a server-sent event stream.
    events: bool,
    /// The last bytes sent, to tell whether the stream is at an event boundary.
    tail: Vec<u8>,
    /// How long an event stream has been silent.
    silent: Duration,
    done: bool,
}

impl Relay {
    /// The next piece of the answer. An error aborts the connection, so that the client does not
    /// take a cut answer for a complete one.
    async fn next(&mut self) -> Option<std::io::Result<Bytes>> {
        if self.done {
            return None;
        }
        let wait = if self.events {
            self.timing.keep_alive
        } else {
            self.timing.silence
        };
        loop {
            match tokio::time::timeout(wait, self.upstream.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    self.silent = Duration::ZERO;
                    self.tail.extend_from_slice(&chunk);
                    let keep = self.tail.len().saturating_sub(4);
                    self.tail.drain(..keep);
                    return Some(Ok(chunk));
                }
                Ok(Ok(None)) => return None,
                Ok(Err(err)) => {
                    tracing::warn!(model = %self.model, error = %err.without_url(), "a model answer broke off");
                    return self.fail("the model's answer broke off");
                }
                Err(_) if !self.events => {
                    tracing::warn!(model = %self.model, "a model answer stayed silent too long and was cut");
                    return self.fail("the model's answer stayed silent too long");
                }
                Err(_) => {
                    self.silent += wait;
                    if self.silent >= self.timing.stream_silence {
                        tracing::warn!(model = %self.model, "a model stream stayed silent too long and was cut");
                        return self.fail("the model's stream stayed silent too long");
                    }
                    // A comment line is only valid where an event may start.
                    let boundary = self.tail.is_empty()
                        || self.tail.ends_with(b"\n\n")
                        || self.tail.ends_with(b"\r\n\r\n");
                    if boundary {
                        return Some(Ok(Bytes::from_static(b": keep-alive\n\n")));
                    }
                }
            }
        }
    }

    fn fail(&mut self, why: &'static str) -> Option<std::io::Result<Bytes>> {
        self.done = true;
        Some(Err(std::io::Error::other(why)))
    }
}

/// Reads a body of at most `limit` bytes; `None` when it is larger.
async fn read_body(body: Body, limit: usize) -> Result<Option<Bytes>, axum::Error> {
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() + chunk.len() > limit {
            return Ok(None);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Some(out.into()))
}

/// Reads and discards a body before an early answer: the HTTP server would otherwise close a
/// connection with a body in flight, and the client would see a reset instead of the answer.
async fn drain(body: Body, limit: usize) {
    let mut stream = body.into_data_stream();
    let mut read = 0;
    while let Some(Ok(chunk)) = stream.next().await {
        read += chunk.len();
        if read > limit {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn config(extra: &str) -> Config {
        let text = format!(
            "[[tokens]]\nname = \"grok\"\nsha256 = \"{zero}\"\n\
             [[tokens]]\nname = \"brain\"\nsha256 = \"{one}\"\nmodels_only = true\n\
             [[workspaces]]\npath = {ws}\n\
             [models.qwen]\ntype = \"chat\"\nurl = \"http://h/v1\"\n\
             [models.embed]\ntype = \"embedding\"\nurl = \"http://h/v1\"\n\
             [models.opus]\ntype = \"chat\"\napi = \"anthropic\"\nurl = \"https://h/v1\"\ntokens = [\"brain\"]\n{extra}",
            zero = "0".repeat(64),
            one = "1".repeat(64),
            ws = toml::Value::String(std::env::temp_dir().display().to_string()),
        );
        Config::parse(&text, Path::new("t.toml")).unwrap()
    }

    #[test]
    fn defaults_fill_gaps_and_the_client_wins() {
        let mut body = json!({"model": "x", "chat_template_kwargs": {"other": 1}, "temperature": 0.2, "stop": ["z"]});
        let defaults = json!({"chat_template_kwargs": {"enable_thinking": false, "other": 9}, "temperature": 0.7, "stop": ["a"], "top_p": 0.9});
        merge_defaults(body.as_object_mut().unwrap(), defaults.as_object().unwrap());
        assert_eq!(
            body,
            json!({"model": "x", "chat_template_kwargs": {"other": 1, "enable_thinking": false},
                   "temperature": 0.2, "stop": ["z"], "top_p": 0.9})
        );
        let keys: Vec<_> = body.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "model",
                "chat_template_kwargs",
                "temperature",
                "stop",
                "top_p"
            ]
        );
    }

    #[test]
    fn selection_explains_every_refusal() {
        let cfg = config("");
        assert_eq!(
            select(&cfg, &CHAT_COMPLETIONS, "qwen", "grok")
                .unwrap()
                .name,
            "qwen"
        );
        let missing = select(&cfg, &CHAT_COMPLETIONS, "nope", "grok").unwrap_err();
        assert_eq!(
            (missing.status.as_u16(), missing.code),
            (404, "model_not_found")
        );
        assert!(
            missing.message.contains("`qwen`")
                && !missing.message.contains("opus")
                && !missing.message.contains("embed"),
            "{}",
            missing.message
        );
        let none = select(&cfg, &RERANK, "nope", "grok").unwrap_err();
        assert!(none.code == "model_not_found" && none.message.contains("GET /v1/models"));
        let closed = select(&cfg, &MESSAGES, "opus", "grok").unwrap_err();
        assert_eq!((closed.status.as_u16(), closed.code), (403, "forbidden"));
        let wrong = select(&cfg, &CHAT_COMPLETIONS, "embed", "grok").unwrap_err();
        assert_eq!(
            (wrong.status.as_u16(), wrong.code),
            (400, "wrong_model_type")
        );
        assert!(
            wrong.message.contains("POST /v1/embeddings"),
            "{}",
            wrong.message
        );
        let api = select(&cfg, &CHAT_COMPLETIONS, "opus", "brain").unwrap_err();
        assert!(
            api.code == "wrong_model_type" && api.message.contains("POST /v1/messages"),
            "{}",
            api.message
        );
    }
}
