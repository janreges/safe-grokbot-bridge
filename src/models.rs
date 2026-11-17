//! Models behind the bridge: the endpoint table, model selection, defaults and the model list.
//!
//! Everything the bridge knows about upstream model APIs (paths, auth header styles, the
//! Anthropic version header) lives in this module, the way agent knowledge lives in `agents.rs`.

use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Map, Value};
use utoipa::ToSchema;

use crate::config::{Config, Model, ModelApi, ModelType};
use crate::error::ApiError;

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
