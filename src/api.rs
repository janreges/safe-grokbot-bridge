//! HTTP handlers, request and response types, and the OpenAPI document.
//!
//! Doc comments on handlers and types become the OpenAPI descriptions. Their reader is usually an
//! LLM agent, so they say what a field means and what to do next.

use std::sync::OnceLock;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Extension, Json, Router, middleware};
use serde::{Deserialize, Serialize};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{IntoParams, Modify, OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::{self, Caller};
use crate::config::{AgentKind, Capability, ModelType};
use crate::error::{ApiError, ErrorBody};
use crate::exec;
use crate::files::{self, FileContent, FileList};
use crate::git::{self, GitOutput, GitRequest};
use crate::inbox;
use crate::models::{self, ModelInfo, ModelList};
use crate::paths;
use crate::server::AppState;
use crate::sessions::{InputRequest, MAX_WAIT_SECS, Runner, Session, SessionList, StartRequest};
use crate::transfer::{self, Uploaded};

const MAX_BODY: usize = 1024 * 1024;

const API_DESCRIPTION: &str = "\
A narrow, authenticated API that lets a remote AI agent work with this machine: read the folders \
it was given access to and their git history, see the coding-agent sessions (Claude Code, Codex, \
Hermes) running there, talk to them, and start new ones.

Usage notes:
- Every request except `GET /healthz` needs `Authorization: Bearer <token>`.
- Start with `GET /v1/info`: it lists the folders (workspaces) you may access and what you may do \
in each (`files`, `git`, `sessions`, `input`, `start`), plus the available agents.
- All paths are absolute. A session's `cwd` can be passed straight to the file and git endpoints.
- Agents work for minutes. After sending input, call `GET /v1/sessions/{id}?wait=60` to wait until \
the session stops working instead of polling in a tight loop.
- Models configured on this machine (chat, vision, embedding, reranker, speech-to-text, \
text-to-speech, image generation) are called through OpenAI- and Anthropic-compatible endpoints \
under `/v1`, so stock client libraries work with the bridge token as their API key. \
`GET /v1/models` lists them with the endpoint to call.
- Errors have the shape `{\"error\": {\"code\": ..., \"message\": ...}}`; the message says what to do.";

#[derive(OpenApi)]
#[openapi(
    info(title = "safe-grokbot-bridge", description = API_DESCRIPTION, license(name = "MIT")),
    paths(healthz),
    modifiers(&SecurityAddon),
    security(("bearer" = [])),
    tags(
        (name = "info", description = "Machine overview and this API's description"),
        (name = "files", description = "Read-only file access"),
        (name = "git", description = "Read-only git access"),
        (name = "inbox", description = "Messages the owner leaves for Grok Bot: read, claim, give back"),
        (name = "sessions", description = "Agent sessions: list, start, talk to, stop"),
        (name = "models", description = "Models behind the bridge, called with OpenAI- and Anthropic-compatible requests"),
    )
)]
struct ApiDoc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("sgb_<64 hex characters>")
                    .description(Some("Token created with `safe-grokbot-bridge token`."))
                    .build(),
            ),
        );
    }
}

fn api_routes() -> OpenApiRouter<AppState> {
    open_routes().merge(full_routes().route_layer(middleware::from_fn(auth::full_access)))
}

/// Endpoints every token may call, `models_only` tokens included.
fn open_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(openapi_document))
        .routes(routes!(list_models))
        .routes(routes!(chat_completions))
        .routes(routes!(messages))
        .routes(routes!(embeddings))
        .routes(routes!(rerank))
        .routes(routes!(speech))
        .routes(routes!(image_generations))
        .merge(
            OpenApiRouter::new()
                .routes(routes!(transcriptions))
                .layer(DefaultBodyLimit::max(models::MAX_FORM_BODY)),
        )
}

/// Endpoints closed to `models_only` tokens.
fn full_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(info))
        .routes(routes!(list_files))
        .routes(routes!(read_file))
        .routes(routes!(download_file))
        .merge(
            OpenApiRouter::new()
                .routes(routes!(upload_files))
                .layer(DefaultBodyLimit::max(transfer::MAX_TRANSFER_BYTES as usize)),
        )
        .routes(routes!(git_status))
        .routes(routes!(git_log))
        .routes(routes!(git_diff))
        .routes(routes!(git_show))
        .routes(routes!(list_sessions, start_session))
        .routes(routes!(get_session, stop_session))
        .routes(routes!(send_input))
        .routes(routes!(list_inbox))
        .routes(routes!(get_inbox_message))
        .routes(routes!(get_inbox_attachment))
        .routes(routes!(claim_inbox_message))
        .routes(routes!(release_inbox_message))
}

/// The OpenAPI document served at `/openapi.json`.
pub fn openapi() -> utoipa::openapi::OpenApi {
    let mut openapi = api_routes().split_for_parts().1;
    add_common_responses(&mut openapi);
    openapi
}

static OPENAPI: OnceLock<utoipa::openapi::OpenApi> = OnceLock::new();

/// This OpenAPI document.
#[utoipa::path(get, path = "/openapi.json", tag = "info", responses(
    (status = 200, description = "The OpenAPI 3.1 document of this API", body = serde_json::Value),
))]
async fn openapi_document() -> Json<utoipa::openapi::OpenApi> {
    Json(OPENAPI.get_or_init(openapi).clone())
}

/// Every authenticated operation can answer 401 (bad token) and 429 (rate limit or lockout,
/// with `Retry-After`), and every operation with a JSON body can reject the body with 413, 415
/// or 422; declaring them once keeps the handlers readable.
fn add_common_responses(openapi: &mut utoipa::openapi::OpenApi) {
    use std::collections::btree_map::Entry;
    use utoipa::openapi::content::ContentBuilder;
    use utoipa::openapi::header::HeaderBuilder;
    use utoipa::openapi::schema::{ObjectBuilder, Type};
    use utoipa::openapi::{Ref, RefOr, ResponseBuilder};

    let error = || {
        ContentBuilder::new()
            .schema(Some(Ref::from_schema_name("ErrorBody")))
            .build()
    };
    let unauthorized = ResponseBuilder::new()
        .description("Missing or invalid token")
        .content("application/json", error())
        .build();
    let retry_after = HeaderBuilder::new()
        .schema(Some(ObjectBuilder::new().schema_type(Type::Integer)))
        .description(Some("Seconds to wait before retrying"))
        .build();
    let too_many = ResponseBuilder::new()
        .description("Rate limit exceeded, or this address is blocked after failed authentications")
        .content("application/json", error())
        .header("Retry-After", retry_after.clone())
        .build();
    let unexpected = ResponseBuilder::new()
        .description("An unexpected error in the bridge; `error.message` says what failed")
        .content("application/json", error())
        .build();
    // Model calls pass their body on unparsed, so they have errors of their own.
    let model_errors = [
        (
            "400",
            "`invalid_request`: the body is not a JSON object with a string `model`. `wrong_model_type`: the model is called through another endpoint, which the message names. Or the upstream rejected the request, in its own error format",
        ),
        ("403", "`forbidden`: the model is not open to this token"),
        (
            "404",
            "`model_not_found`: no model of that name; the message lists the ones you may call here",
        ),
        ("413", "`too_large`: the body is larger than 32 MiB"),
        (
            "429",
            "`models_busy`: every model slot stayed taken for 10 seconds (wait `Retry-After` seconds), or the upstream's own 429, passed on",
        ),
        (
            "502",
            "The bridge's own answer when the model's server rejects the configured key (`upstream_auth_failed`), is configured wrongly (`upstream_misconfigured`) or cannot be reached (`upstream_unreachable`). Sent with `x-should-retry: false`: only the machine's owner can fix it",
        ),
        (
            "504",
            "`upstream_timeout`: the model did not start to answer within 90 seconds; use `stream: true` where the endpoint has it. Sent with `x-should-retry: false`",
        ),
    ];
    let body_errors = [
        ("413", "The JSON body is larger than 1 MiB"),
        (
            "415",
            "The body is not sent as `Content-Type: application/json`",
        ),
        (
            "422",
            "The JSON body does not match the schema (a missing or unknown field, or a wrong type)",
        ),
    ];
    for (path, item) in openapi.paths.paths.iter_mut() {
        if path == "/healthz" {
            continue;
        }
        let operations = [
            &mut item.get,
            &mut item.post,
            &mut item.put,
            &mut item.delete,
        ];
        for operation in operations.into_iter().flatten() {
            let has_body = operation.request_body.is_some();
            let json_body = operation
                .request_body
                .as_ref()
                .is_some_and(|body| match body {
                    RefOr::T(body) => body.content.contains_key("application/json"),
                    RefOr::Ref(_) => true,
                });
            let model_call = has_body
                && operation
                    .tags
                    .as_ref()
                    .is_some_and(|tags| tags.iter().any(|tag| tag == "models"));
            let responses = &mut operation.responses.responses;
            responses
                .entry("default".into())
                .or_insert_with(|| unexpected.clone().into());
            responses
                .entry("401".into())
                .or_insert_with(|| unauthorized.clone().into());
            match responses.entry("429".into()) {
                Entry::Vacant(entry) => {
                    entry.insert(too_many.clone().into());
                }
                Entry::Occupied(mut entry) => {
                    if let RefOr::T(response) = entry.get_mut() {
                        response.description.push_str(
                            ", or the rate limit or an authentication lockout (wait `Retry-After` seconds)",
                        );
                        response
                            .headers
                            .insert("Retry-After".into(), retry_after.clone().into());
                    }
                }
            }
            if model_call {
                for (status, description) in model_errors {
                    responses.entry(status.into()).or_insert_with(|| {
                        ResponseBuilder::new()
                            .description(description)
                            .content("application/json", error())
                            .build()
                            .into()
                    });
                }
            } else if json_body {
                for (status, description) in body_errors {
                    responses.entry(status.into()).or_insert_with(|| {
                        ResponseBuilder::new()
                            .description(description)
                            .content("application/json", error())
                            .build()
                            .into()
                    });
                }
            }
        }
    }
}

pub fn router(app: AppState) -> Router {
    let (routes, _) = api_routes().split_for_parts();
    let protected = routes
        .fallback(unknown_route)
        .method_not_allowed_fallback(wrong_method)
        .layer(middleware::from_fn_with_state(
            app.clone(),
            auth::middleware,
        ));
    Router::new()
        .route("/healthz", get(healthz))
        .merge(protected)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(app)
}

async fn unknown_route(uri: axum::http::Uri) -> ApiError {
    ApiError::not_found(format!(
        "There is no endpoint `{}`. GET /openapi.json lists the endpoints.",
        uri.path().chars().take(200).collect::<String>()
    ))
}

async fn wrong_method(method: axum::http::Method, uri: axum::http::Uri) -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        format!(
            "`{}` does not support {method}. GET /openapi.json lists the methods of every endpoint.",
            uri.path().chars().take(200).collect::<String>()
        ),
    )
}

/// Turns a JSON body parse failure into the API's JSON error format.
fn json<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(b)| b).map_err(|rejection| {
        let status = rejection.status();
        let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
            "too_large"
        } else {
            "invalid_request"
        };
        ApiError::new(
            status,
            code,
            format!("Invalid JSON body: {}", rejection.body_text()),
        )
    })
}

/// Turns a session id that is not valid in a URL (e.g. `%FF`) into the API's JSON error format.
fn session_id(id: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    id.map(|Path(id)| id).map_err(|rejection| {
        ApiError::bad_request(format!(
            "Invalid session id in the URL: {}. Use an `id` from GET /v1/sessions.",
            rejection.body_text()
        ))
    })
}

/// Turns a query-string parse failure into the API's JSON error format.
fn query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query.map(|Query(q)| q).map_err(|rejection| {
        ApiError::bad_request(format!("Invalid query string: {}", rejection.body_text()))
    })
}

/// File listings and reads run on blocking threads. This bounds how many run at once, so a burst
/// of large reads cannot take all memory.
static BLOCKING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let permit = BLOCKING.acquire().await.map_err(ApiError::internal)?;
    // The permit goes with the work: a cancelled request does not stop a started read.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(ApiError::internal)?
}

// ---------------------------------------------------------------------------------------------
// Info
// ---------------------------------------------------------------------------------------------

/// Liveness probe. Needs no token and reveals nothing.
#[utoipa::path(get, path = "/healthz", tag = "info", security(()), responses((status = 200, description = "The bridge is running", body = String, example = "ok")))]
async fn healthz() -> impl axum::response::IntoResponse {
    // Needs no token, so it keeps no connection open either.
    ([(axum::http::header::CONNECTION, "close")], "ok")
}

/// Overview of this machine and what this token may do here.
#[derive(Serialize, ToSchema, Debug)]
pub struct Info {
    /// Display name of this machine.
    pub name: String,
    /// Bridge version.
    pub version: String,
    /// Operating system (`linux`, `macos`, `windows`).
    pub os: String,
    pub arch: String,
    /// Current server time, RFC 3339.
    pub time: String,
    /// Configured agents. Only agents listed in a workspace's `agents` may be started there.
    pub agents: Vec<AgentInfo>,
    /// Folders this bridge exposes. A workspace covers its `path` and everything below it.
    pub workspaces: Vec<WorkspaceInfo>,
    pub tmux: TmuxInfo,
    /// Runner used when a start request does not choose one.
    pub default_runner: Runner,
    /// Whether sessions running outside tmux can take input (experimental).
    pub external_sessions: bool,
    /// Models this token may call; `GET /v1/models` lists the same with a `type` filter.
    pub models: Vec<ModelInfo>,
    pub limits: LimitsInfo,
}

#[derive(Serialize, ToSchema, Debug)]
pub struct TmuxInfo {
    /// Whether tmux sessions can be started and controlled.
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Serialize, ToSchema, Debug)]
pub struct AgentInfo {
    /// Name to use in `agent` when starting a session.
    pub name: String,
    pub kind: AgentKind,
    /// Whether the agent's command was found on this machine.
    pub available: bool,
    /// Whether the agent can run headless (one non-interactive turn per message).
    pub headless: bool,
}

#[derive(Serialize, ToSchema, Debug)]
pub struct WorkspaceInfo {
    pub name: String,
    /// Absolute folder path.
    pub path: String,
    /// What clients may do in this folder.
    pub allow: Vec<Capability>,
    /// Agents that may be started here.
    pub agents: Vec<String>,
    /// Whether sessions may be started with `bypass_permissions: true`.
    pub allow_bypass_permissions: bool,
}

#[derive(Serialize, ToSchema, Debug)]
pub struct LimitsInfo {
    /// Requests allowed per minute for this token.
    pub requests_per_minute: u32,
    /// Sessions started by the bridge that may run at the same time.
    pub max_sessions: usize,
    /// Maximum `wait` for long-polling a session, in seconds.
    pub max_wait_secs: u64,
    /// Model calls the bridge runs at the same time; further calls wait up to 10 seconds for a
    /// free slot, then get 429 `models_busy`.
    pub max_model_requests: usize,
    /// Bytes per upload request and per downloaded file (96 MB).
    pub max_transfer_bytes: u64,
}

/// Describes this machine: version, agents, the workspaces you may access and the limits.
/// Call this first.
#[utoipa::path(get, path = "/v1/info", tag = "info", responses(
    (status = 200, description = "This machine, its agents and the workspaces you may access", body = Info),
    (status = 401, description = "Missing or invalid token", body = ErrorBody),
))]
async fn info(State(app): State<AppState>, Extension(caller): Extension<Caller>) -> Json<Info> {
    let config = &app.config;
    Json(Info {
        name: config.server.name.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        time: files::rfc3339(std::time::SystemTime::now()),
        agents: config
            .agents
            .iter()
            .map(|agent| AgentInfo {
                name: agent.name.clone(),
                kind: agent.kind,
                available: exec::which_with(&agent.command, &agent.env).is_some(),
                headless: agent.kind != AgentKind::Generic,
            })
            .collect(),
        workspaces: config
            .workspaces
            .iter()
            .map(|ws| WorkspaceInfo {
                name: ws.name.clone(),
                path: ws.root.display().to_string(),
                allow: ws.allow.iter().copied().collect(),
                agents: ws.agents.clone(),
                allow_bypass_permissions: ws.allow_bypass_permissions,
            })
            .collect(),
        tmux: TmuxInfo {
            available: app.sessions.tmux.is_some(),
            version: app.sessions.tmux_version.clone(),
        },
        default_runner: app.sessions.default_runner(),
        external_sessions: config.experimental.external_sessions,
        models: models::list(config, &caller.token, None, started_secs(&app)).data,
        limits: LimitsInfo {
            requests_per_minute: config.limits.requests_per_minute,
            max_sessions: config.limits.max_sessions,
            max_wait_secs: MAX_WAIT_SECS,
            max_model_requests: config.limits.max_model_requests,
            max_transfer_bytes: transfer::MAX_TRANSFER_BYTES,
        },
    })
}

// ---------------------------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListFilesQuery {
    /// Absolute path of a directory inside a workspace with the `files` capability.
    pub path: String,
    /// Levels to descend, 1–5 (default 1: direct children only).
    pub depth: Option<usize>,
    /// Maximum number of entries, 1–2000 (default 200).
    pub limit: Option<usize>,
    /// Entries to skip, for the next page (use `next_offset` from the previous response).
    pub offset: Option<usize>,
}

/// Lists a directory in path order, page by page. Entries ignored by `.gitignore` or matching
/// the workspace's deny patterns (secrets such as `.env` files and keys) are left out.
#[utoipa::path(get, path = "/v1/files", tag = "files", params(ListFilesQuery), responses(
    (status = 200, description = "The directory listing", body = FileList),
    (status = 400, description = "Not an absolute directory path", body = ErrorBody),
    (status = 403, description = "Outside the workspaces, denied, or `files` not allowed", body = ErrorBody),
    (status = 404, description = "The directory does not exist", body = ErrorBody),
    (status = 409, description = "The directory was replaced during the listing; try again", body = ErrorBody),
))]
async fn list_files(
    State(app): State<AppState>,
    q: Result<Query<ListFilesQuery>, QueryRejection>,
) -> Result<Json<FileList>, ApiError> {
    let q = query(q)?;
    blocking(move || {
        let resolved = paths::resolve(&app.config, &q.path)?;
        paths::require(resolved.workspace, Capability::Files)?;
        files::list(
            &app.config,
            resolved.workspace,
            &resolved.path,
            q.depth.unwrap_or(1),
            q.limit.unwrap_or(200),
            q.offset.unwrap_or(0),
        )
    })
    .await
    .map(Json)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ReadFileQuery {
    /// Absolute path of a text file inside a workspace with the `files` capability.
    pub path: String,
    /// First line to return, 1-based (default 1).
    pub start_line: Option<usize>,
    /// Number of lines to return, 1–4000 (default 400).
    pub max_lines: Option<usize>,
}

/// Reads a range of lines from a text file. When `truncated` is true, continue with
/// `start_line = next_start_line`.
#[utoipa::path(get, path = "/v1/files/content", tag = "files", params(ReadFileQuery), responses(
    (status = 200, description = "The requested lines of the file", body = FileContent),
    (status = 400, description = "Not a file, or `start_line` past the end", body = ErrorBody),
    (status = 403, description = "Outside the workspaces, denied, or `files` not allowed", body = ErrorBody),
    (status = 404, description = "The file does not exist", body = ErrorBody),
    (status = 409, description = "The file was replaced while it was being opened; try again", body = ErrorBody),
    (status = 413, description = "File larger than 10 MiB", body = ErrorBody),
    (status = 415, description = "Binary file", body = ErrorBody),
))]
async fn read_file(
    State(app): State<AppState>,
    q: Result<Query<ReadFileQuery>, QueryRejection>,
) -> Result<Json<FileContent>, ApiError> {
    let q = query(q)?;
    blocking(move || {
        let resolved = paths::resolve(&app.config, &q.path)?;
        paths::require(resolved.workspace, Capability::Files)?;
        files::read(
            resolved.workspace,
            &resolved.path,
            q.start_line.unwrap_or(1),
            q.max_lines.unwrap_or(400),
        )
    })
    .await
    .map(Json)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UploadQuery {
    /// Absolute path of an existing folder inside a workspace that allows `upload`, such as a
    /// session's `cwd`. It only selects the workspace: the files go to that workspace's incoming
    /// folder, `<workspace>/.sgb-inbox/`, not into this folder.
    pub dir: String,
}

/// One or more files, each a part with a file name.
#[derive(ToSchema)]
pub struct UploadForm {
    /// Up to 20 files; the part name does not matter, `file` is usual.
    #[schema(value_type = Vec<String>, format = Binary)]
    pub file: Vec<Vec<u8>>,
}

/// Uploads files to this machine. They are stored in the incoming folder of `dir`'s workspace
/// under names the bridge generates (`2026-11-19.14-32-53.k7q2.zip`), never replacing a file,
/// and the response gives each file's absolute `path` to pass to an agent. Either every file is
/// stored or none. At most 96 MB (96 000 000 bytes) per request and 20 files. Files stay until
/// someone deletes them on the machine; no endpoint lists or deletes them.
#[utoipa::path(post, path = "/v1/files/upload", tag = "files", params(UploadQuery),
    request_body(content = inline(UploadForm), content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Every file was stored; `files` lists their paths in the order of the request", body = Uploaded),
        (status = 400, description = "`invalid_request`: not a multipart form, no file, a part without a file name, more than 20 files, or the body broke off. Nothing was stored", body = ErrorBody),
        (status = 403, description = "`dir` is outside the workspaces, or its workspace does not allow `upload`", body = ErrorBody),
        (status = 404, description = "`dir` does not exist", body = ErrorBody),
        (status = 409, description = "`inbox_unusable`: the incoming folder is a link, lies outside the workspace or cannot be created; only the machine's owner can fix it", body = ErrorBody),
        (status = 413, description = "`too_large`: the request is larger than 96 MB. Nothing was stored", body = ErrorBody),
        (status = 429, description = "`transfers_busy`: 4 uploads and downloads are running; wait `Retry-After` seconds", body = ErrorBody),
        (status = 507, description = "`disk_full`: the machine has no space left. Nothing was stored", body = ErrorBody),
    ))]
async fn upload_files(
    State(app): State<AppState>,
    q: Result<Query<UploadQuery>, QueryRejection>,
    request: axum::extract::Request,
) -> axum::response::Response {
    transfer::upload(&app, query(q).map(|q| q.dir), request).await
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DownloadQuery {
    /// Absolute path of a regular file inside a workspace that allows `download`.
    pub path: String,
}

/// Downloads a file of any kind, binary ones included, as raw bytes with `Content-Length` and
/// the file's name in `Content-Disposition`. At most 96 MB (96 000 000 bytes). To read a text
/// file line by line, use GET /v1/files/content instead.
#[utoipa::path(get, path = "/v1/files/download", tag = "files", params(DownloadQuery),
    responses(
        (status = 200, description = "The file's bytes", content_type = "application/octet-stream"),
        (status = 400, description = "Not an absolute path, a folder, or not a regular file", body = ErrorBody),
        (status = 403, description = "Outside the workspaces, denied (`denied`: secrets such as `.env` files and keys are never served), or `download` not allowed", body = ErrorBody),
        (status = 404, description = "The file does not exist", body = ErrorBody),
        (status = 409, description = "`path_changed`: the file was replaced while it was being opened; try again", body = ErrorBody),
        (status = 413, description = "`too_large`: the file is larger than 96 MB", body = ErrorBody),
        (status = 429, description = "`transfers_busy`: 4 uploads and downloads are running; wait `Retry-After` seconds", body = ErrorBody),
    ))]
async fn download_file(
    State(app): State<AppState>,
    q: Result<Query<DownloadQuery>, QueryRejection>,
) -> axum::response::Response {
    transfer::download(&app, query(q).map(|q| q.path)).await
}

// ---------------------------------------------------------------------------------------------
// Inbox
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InboxQuery {
    /// Which messages: `unread` (the default; no bot has taken them), `claimed`, `released` (a bot
    /// gave them back), or `all`.
    pub state: Option<String>,
    /// Only messages for this bot and for every bot. Pass your own bot name, for example `Homelab`.
    pub to: Option<String>,
    /// Messages to return, 1–200 (default 50). The newest come first.
    pub limit: Option<usize>,
}

/// A page of the inbox. Bodies are left out: read a message with GET /v1/inbox/{id} before you
/// claim it.
#[derive(Serialize, ToSchema)]
pub struct InboxList {
    /// Newest first.
    pub messages: Vec<inbox::Message>,
    /// How many messages match the filter in all.
    pub total: usize,
    /// True when `messages` holds fewer messages than `total`.
    pub truncated: bool,
    /// What to do next when the list is empty or cut short. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A bot's claim or release of a message.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxAction {
    /// The bot that takes or gives back the message, for example `Homelab`. At most 64 characters.
    pub bot: String,
    /// Why the message is taken, or where it should go instead. At most 1000 bytes.
    pub note: Option<String>,
}

/// Lists the inbox: messages the owner left on this machine for Grok Bot, newest first, without
/// bodies. Read each message you may take with GET /v1/inbox/{id}, then claim it, so that no other
/// bot takes it as well. Claim only messages that match the conversation you are in.
#[utoipa::path(get, path = "/v1/inbox", tag = "inbox", params(InboxQuery),
    responses(
        (status = 200, description = "The messages that match", body = InboxList),
        (status = 400, description = "Invalid `state` or `limit`", body = ErrorBody),
        (status = 403, description = "The token is limited to models", body = ErrorBody),
    ))]
async fn list_inbox(
    State(app): State<AppState>,
    q: Result<Query<InboxQuery>, QueryRejection>,
) -> Result<Json<InboxList>, ApiError> {
    let q = query(q)?;
    let state = match q.state.as_deref().unwrap_or("unread") {
        "unread" => Some(inbox::State::Unread),
        "claimed" => Some(inbox::State::Claimed),
        "released" => Some(inbox::State::Released),
        "all" => None,
        other => {
            return Err(ApiError::bad_request(format!(
                "`state` is `{other}`; use `unread`, `claimed`, `released` or `all`."
            )));
        }
    };
    let limit = q.limit.unwrap_or(inbox::LIST_DEFAULT);
    if !(1..=inbox::LIST_MAX).contains(&limit) {
        return Err(ApiError::bad_request(format!(
            "`limit` is {limit}; use a number from 1 to {}.",
            inbox::LIST_MAX
        )));
    }
    let state_dir = app.config.server.state_dir.clone();
    let to = q.to;
    let listing = blocking(move || {
        inbox::list(&state_dir, state, to.as_deref(), limit).map_err(ApiError::from)
    })
    .await?;
    let shown = listing.messages.len();
    let truncated = listing.total > shown;
    let note = if listing.total == 0 {
        Some(
            "No message matches this filter. Nothing is waiting for this bot under these \
             conditions. If the owner expected one, it may be for another bot (see `to`) or \
             in another state (see `state`)."
                .to_owned(),
        )
    } else if truncated {
        Some(format!(
            "Showing the newest {shown} of {} messages. Pass a larger `limit` (up to {}), or \
             narrow with `state` or `to`, to see the rest.",
            listing.total,
            inbox::LIST_MAX
        ))
    } else {
        None
    };
    Ok(Json(InboxList {
        messages: listing.messages,
        total: listing.total,
        truncated,
        note,
    }))
}

/// One inbox message with its full Markdown body. Reading changes nothing; claim the message to
/// take it.
#[utoipa::path(get, path = "/v1/inbox/{id}", tag = "inbox",
    params(("id" = String, Path, description = "Message id from GET /v1/inbox")),
    responses(
        (status = 200, description = "The message, with its `body`", body = inbox::Message),
        (status = 403, description = "The token is limited to models", body = ErrorBody),
        (status = 404, description = "No message with that id", body = ErrorBody),
    ))]
async fn get_inbox_message(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<inbox::Message>, ApiError> {
    let state_dir = app.config.server.state_dir.clone();
    blocking(move || inbox::get(&state_dir, &id).map_err(ApiError::from))
        .await
        .map(Json)
}

/// The bytes of one attachment of an inbox message, by the name the message lists. The limits are
/// those of a file download: at most 96 MB, and the transfer slots are shared with uploads.
#[utoipa::path(get, path = "/v1/inbox/{id}/attachments/{name}", tag = "inbox",
    params(
        ("id" = String, Path, description = "Message id from GET /v1/inbox"),
        ("name" = String, Path, description = "Attachment name, as the message lists it"),
    ),
    responses(
        (status = 200, description = "The attachment's bytes", content_type = "application/octet-stream"),
        (status = 404, description = "No message, or no attachment by that name", body = ErrorBody),
        (status = 413, description = "`too_large`: the attachment is larger than 96 MB", body = ErrorBody),
        (status = 429, description = "`transfers_busy`: 4 transfers are running; wait `Retry-After` seconds", body = ErrorBody),
    ))]
async fn get_inbox_attachment(
    State(app): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> axum::response::Response {
    transfer::inbox_attachment(&app, id, name).await
}

/// Takes an inbox message for a bot. Read the message in full before you claim it, and claim only
/// what matches the conversation you are in. A message addressed to another bot (`to`) cannot be
/// taken. Taking it again as the bot that holds it changes nothing. If another bot holds it, the
/// answer is 409 `already_claimed` and names the holder.
#[utoipa::path(post, path = "/v1/inbox/{id}/claim", tag = "inbox",
    params(("id" = String, Path, description = "Message id from GET /v1/inbox")),
    request_body = InboxAction,
    responses(
        (status = 200, description = "Claimed; the message as it is now, without `body`", body = inbox::Message),
        (status = 400, description = "Invalid `bot` or `note`", body = ErrorBody),
        (status = 403, description = "The token is limited to models", body = ErrorBody),
        (status = 404, description = "No message with that id", body = ErrorBody),
        (status = 409, description = "`already_claimed`: another bot holds it; `addressed_to_other`: it is for a different bot", body = ErrorBody),
    ))]
async fn claim_inbox_message(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<InboxAction>, JsonRejection>,
) -> Result<Json<inbox::Message>, ApiError> {
    let action = json(body)?;
    let state_dir = app.config.server.state_dir.clone();
    blocking(move || {
        inbox::claim(
            &state_dir,
            &id,
            &action.bot,
            action.note.as_deref(),
            std::time::SystemTime::now(),
        )
        .map_err(ApiError::from)
    })
    .await
    .map(Json)
}

/// Gives back a message this bot had claimed, so that another bot may take it. Only the bot that
/// holds the message may do this; the note says why, or where the message should go.
#[utoipa::path(post, path = "/v1/inbox/{id}/release", tag = "inbox",
    params(("id" = String, Path, description = "Message id from GET /v1/inbox")),
    request_body = InboxAction,
    responses(
        (status = 200, description = "Released; the message as it is now, without `body`", body = inbox::Message),
        (status = 400, description = "Invalid `bot` or `note`", body = ErrorBody),
        (status = 403, description = "The token is limited to models", body = ErrorBody),
        (status = 404, description = "No message with that id", body = ErrorBody),
        (status = 409, description = "`not_claimed`: nobody holds it; `not_yours`: another bot holds it", body = ErrorBody),
    ))]
async fn release_inbox_message(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<InboxAction>, JsonRejection>,
) -> Result<Json<inbox::Message>, ApiError> {
    let action = json(body)?;
    let state_dir = app.config.server.state_dir.clone();
    blocking(move || {
        inbox::release(
            &state_dir,
            &id,
            &action.bot,
            action.note.as_deref(),
            std::time::SystemTime::now(),
        )
        .map_err(ApiError::from)
    })
    .await
    .map(Json)
}

// ---------------------------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GitStatusQuery {
    /// Absolute path of a directory inside a git repository and a workspace with the `git`
    /// capability.
    pub dir: String,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GitLogQuery {
    /// Absolute path of a directory inside a git repository and a workspace with the `git`
    /// capability.
    pub dir: String,
    /// Number of commits, 1–500 (default 30).
    pub limit: Option<usize>,
    /// Revision or range, e.g. `main`, `origin/main..HEAD` (default: `HEAD`).
    pub rev: Option<String>,
    /// Only commits touching this path, relative to `dir`.
    pub path: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GitDiffQuery {
    /// Absolute path of a directory inside a git repository and a workspace with the `git`
    /// capability.
    pub dir: String,
    /// Compare with this revision or range, e.g. `HEAD~3` or `main..feature`. Without it, the
    /// working tree is compared with the index.
    pub rev: Option<String>,
    /// Show staged changes (`git diff --cached`).
    pub staged: Option<bool>,
    /// Limit the diff to this path, relative to `dir`.
    pub path: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GitShowQuery {
    /// Absolute path of a directory inside a git repository and a workspace with the `git`
    /// capability.
    pub dir: String,
    /// Commit to show (default `HEAD`).
    pub rev: Option<String>,
    /// Limit the patch to this path, relative to `dir`.
    pub path: Option<String>,
}

async fn git_in(
    app: &AppState,
    dir: &str,
    request: GitRequest,
) -> Result<Json<GitOutput>, ApiError> {
    let resolved = paths::resolve(&app.config, dir)?;
    paths::require(resolved.workspace, Capability::Git)?;
    if !resolved.path.is_dir() {
        return Err(ApiError::bad_request(format!(
            "`{dir}` is not a directory. Pass the repository folder as `dir`."
        )));
    }
    let repo = git::repository(&resolved.path).await?;
    paths::require_git_scope(&app.config, &repo.toplevel, &repo.storage)?;
    git::run(&resolved.path, request).await.map(Json)
}

/// `git status --short --branch`: the current branch and changed files.
#[utoipa::path(get, path = "/v1/git/status", tag = "git", params(GitStatusQuery), responses(
    (status = 200, description = "The git output", body = GitOutput),
    (status = 400, description = "Not a git repository or invalid parameters", body = ErrorBody),
    (status = 403, description = "Outside the workspaces or `git` not allowed", body = ErrorBody),
    (status = 404, description = "The directory does not exist", body = ErrorBody),
    (status = 503, description = "git cannot be run on this machine", body = ErrorBody),
))]
async fn git_status(
    State(app): State<AppState>,
    q: Result<Query<GitStatusQuery>, QueryRejection>,
) -> Result<Json<GitOutput>, ApiError> {
    let q = query(q)?;
    git_in(&app, &q.dir, GitRequest::Status).await
}

/// Commit history, one commit per line: `<hash>\t<ISO date>\t<author>\t<subject>`.
#[utoipa::path(get, path = "/v1/git/log", tag = "git", params(GitLogQuery), responses(
    (status = 200, description = "The git output", body = GitOutput),
    (status = 400, description = "Not a git repository or invalid parameters", body = ErrorBody),
    (status = 403, description = "Outside the workspaces or `git` not allowed", body = ErrorBody),
    (status = 404, description = "The directory does not exist", body = ErrorBody),
    (status = 503, description = "git cannot be run on this machine", body = ErrorBody),
))]
async fn git_log(
    State(app): State<AppState>,
    q: Result<Query<GitLogQuery>, QueryRejection>,
) -> Result<Json<GitOutput>, ApiError> {
    let q = query(q)?;
    let request = GitRequest::Log {
        limit: q.limit.unwrap_or(30),
        rev: q.rev,
        path: q.path,
    };
    git_in(&app, &q.dir, request).await
}

/// A diff with a summary (`--stat`) followed by the patch.
#[utoipa::path(get, path = "/v1/git/diff", tag = "git", params(GitDiffQuery), responses(
    (status = 200, description = "The git output", body = GitOutput),
    (status = 400, description = "Not a git repository or invalid parameters", body = ErrorBody),
    (status = 403, description = "Outside the workspaces or `git` not allowed", body = ErrorBody),
    (status = 404, description = "The directory does not exist", body = ErrorBody),
    (status = 503, description = "git cannot be run on this machine", body = ErrorBody),
))]
async fn git_diff(
    State(app): State<AppState>,
    q: Result<Query<GitDiffQuery>, QueryRejection>,
) -> Result<Json<GitOutput>, ApiError> {
    let q = query(q)?;
    let request = GitRequest::Diff {
        rev: q.rev,
        staged: q.staged.unwrap_or(false),
        path: q.path,
    };
    git_in(&app, &q.dir, request).await
}

/// One commit: metadata, summary and patch.
#[utoipa::path(get, path = "/v1/git/show", tag = "git", params(GitShowQuery), responses(
    (status = 200, description = "The git output", body = GitOutput),
    (status = 400, description = "Not a git repository or invalid parameters", body = ErrorBody),
    (status = 403, description = "Outside the workspaces or `git` not allowed", body = ErrorBody),
    (status = 404, description = "The directory does not exist", body = ErrorBody),
    (status = 503, description = "git cannot be run on this machine", body = ErrorBody),
))]
async fn git_show(
    State(app): State<AppState>,
    q: Result<Query<GitShowQuery>, QueryRejection>,
) -> Result<Json<GitOutput>, ApiError> {
    let q = query(q)?;
    git_in(
        &app,
        &q.dir,
        GitRequest::Show {
            rev: q.rev,
            path: q.path,
        },
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListSessionsQuery {
    /// Only sessions whose working directory is under this absolute path.
    pub path: Option<String>,
}

/// Lists agent sessions in the workspaces you may see: tmux sessions (including ones a person
/// started), headless sessions started through the bridge, and Claude Code / Codex sessions
/// running outside tmux. Check here before starting a new session in a folder.
#[utoipa::path(get, path = "/v1/sessions", tag = "sessions", params(ListSessionsQuery), responses(
    (status = 200, description = "The sessions you may see", body = SessionList),
    (status = 400, description = "`path` is not an absolute path", body = ErrorBody),
    (status = 404, description = "`path` does not exist", body = ErrorBody),
    (status = 403, description = "`path` is outside the workspaces", body = ErrorBody),
))]
async fn list_sessions(
    State(app): State<AppState>,
    q: Result<Query<ListSessionsQuery>, QueryRejection>,
) -> Result<Json<SessionList>, ApiError> {
    let q = query(q)?;
    let under = match &q.path {
        Some(path) => Some(paths::resolve(&app.config, path)?.path),
        None => None,
    };
    let (sessions, warnings) = app.sessions.list(under.as_deref()).await;
    let note = match (sessions.is_empty(), warnings.is_empty()) {
        (true, true) => Some(
            "No agent sessions were found in the workspaces you may see. The bridge finds agents running in tmux, Claude Code outside tmux and, when the owner enabled experimental external sessions, Codex outside tmux; other sessions (Hermes outside tmux, for example) are not visible to it. Start one with POST /v1/sessions if a workspace allows `start`.".to_string(),
        ),
        (true, false) => Some(
            "No sessions found, but the list is incomplete: some sources failed (see `warnings`). Do not conclude that nothing runs.".to_string(),
        ),
        _ => None,
    };
    Ok(Json(SessionList {
        sessions,
        warnings,
        note,
    }))
}

/// Starts an agent session in a folder. With `runner: "tmux"` the agent runs in a live terminal
/// a person can attach to; with `"headless"` every message is one non-interactive run. The
/// response comes right after the start: wait for the answer with
/// `GET /v1/sessions/{id}?wait=60`.
#[utoipa::path(post, path = "/v1/sessions", tag = "sessions", request_body = StartRequest, responses(
    (status = 201, description = "Started", body = Session),
    (status = 400, description = "Invalid request", body = ErrorBody),
    (status = 403, description = "`start`, the agent or `bypass_permissions` not allowed in this folder", body = ErrorBody),
    (status = 404, description = "`cwd` does not exist", body = ErrorBody),
    (status = 409, description = "The conversation to resume is open elsewhere", body = ErrorBody),
    (status = 429, description = "Too many running sessions", body = ErrorBody),
    (status = 502, description = "The agent exited right after starting (`agent_exited`), or tmux could not start the session (`tmux_failed`)", body = ErrorBody),
    (status = 503, description = "The agent or tmux is not installed", body = ErrorBody),
))]
async fn start_session(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    body: Result<Json<StartRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Session>), ApiError> {
    let request = json(body)?;
    let session = app.sessions.start(request, &caller.token).await?;
    Ok((StatusCode::CREATED, Json(session)))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GetSessionQuery {
    /// Terminal lines to return in `screen` (1–2000, default 200).
    pub lines: Option<usize>,
    /// Wait up to this many seconds (max 90) while the session is `working`; returns as soon
    /// as it is not.
    pub wait: Option<u64>,
}

/// A session with its state, last answer and terminal screen. Use `wait` to long-poll until
/// the agent stops working.
#[utoipa::path(get, path = "/v1/sessions/{id}", tag = "sessions",
    params(("id" = String, Path, description = "Session id from the session list"), GetSessionQuery),
    responses(
        (status = 200, description = "The session", body = Session),
        (status = 400, description = "Invalid session id or query", body = ErrorBody),
        (status = 404, description = "No such session", body = ErrorBody),
    ))]
async fn get_session(
    State(app): State<AppState>,
    id: Result<Path<String>, PathRejection>,
    q: Result<Query<GetSessionQuery>, QueryRejection>,
) -> Result<Json<Session>, ApiError> {
    let id = session_id(id)?;
    let q = query(q)?;
    app.sessions.get(&id, q.lines, q.wait).await.map(Json)
}

/// Sends a message (`text`) or keystrokes (`keys`) to a session. In tmux, text is pasted as one
/// block and submitted; keys answer menus and permission prompts. Headless sessions take one
/// message per turn. Then wait for the answer with `GET /v1/sessions/{id}?wait=60`.
#[utoipa::path(post, path = "/v1/sessions/{id}/input", tag = "sessions",
    params(("id" = String, Path, description = "Session id")),
    request_body = InputRequest,
    responses(
        (status = 200, description = "Delivered; the session as it is right after", body = Session),
        (status = 400, description = "Invalid text or keys", body = ErrorBody),
        (status = 403, description = "`input` not allowed in this folder", body = ErrorBody),
        (status = 404, description = "No such session", body = ErrorBody),
        (status = 409, description = "Busy, ended, or not controllable", body = ErrorBody),
        (status = 502, description = "Delivery to a session outside tmux failed", body = ErrorBody),
    ))]
async fn send_input(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    id: Result<Path<String>, PathRejection>,
    body: Result<Json<InputRequest>, JsonRejection>,
) -> Result<Json<Session>, ApiError> {
    let id = session_id(id)?;
    let request = json(body)?;
    app.sessions
        .input(&id, request, &caller.token)
        .await
        .map(Json)
}

/// Stops a session the bridge started: closes its tmux session or kills its running turn. The
/// conversation itself stays resumable via `agent_session_id`.
#[utoipa::path(delete, path = "/v1/sessions/{id}", tag = "sessions",
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "Stopped; the final state and screen", body = Session),
        (status = 400, description = "Invalid session id", body = ErrorBody),
        (status = 403, description = "`input` not allowed in this folder", body = ErrorBody),
        (status = 404, description = "No such session", body = ErrorBody),
        (status = 409, description = "Not started by the bridge", body = ErrorBody),
        (status = 502, description = "The tmux session could not be stopped", body = ErrorBody),
    ))]
async fn stop_session(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<Session>, ApiError> {
    let id = session_id(id)?;
    app.sessions.stop(&id, &caller.token).await.map(Json)
}

// ---------------------------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------------------------

fn started_secs(app: &AppState) -> u64 {
    app.started_at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListModelsQuery {
    /// Only models of this type: `chat`, `vision`, `embedding`, `reranker`, `speech-to-text`,
    /// `text-to-speech` or `image-generator`.
    pub r#type: Option<String>,
}

/// Lists the models this token may call, in the shape of OpenAI's model list, with the endpoint
/// that calls each one and the defaults the bridge fills in. The list shows what the owner
/// configured; it does not contact the models, so a listed model can still be switched off.
#[utoipa::path(get, path = "/v1/models", tag = "models", params(ListModelsQuery), responses(
    (status = 200, description = "The models you may call; `note` says why the list is empty", body = ModelList),
    (status = 400, description = "Unknown `type`; the message lists the valid ones", body = ErrorBody),
))]
async fn list_models(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    q: Result<Query<ListModelsQuery>, QueryRejection>,
) -> Result<Json<ModelList>, ApiError> {
    let q = query(q)?;
    let kind = match q.r#type.as_deref() {
        None | Some("") => None,
        Some(text) => Some(ModelType::parse(text).ok_or_else(|| {
            let valid: Vec<&str> = ModelType::ALL.iter().map(|k| k.as_str()).collect();
            ApiError::bad_request(format!(
                "Unknown model type `{}`. Valid types: {}.",
                text.chars().take(50).collect::<String>(),
                valid.join(", ")
            ))
        })?),
    };
    Ok(Json(models::list(
        &app.config,
        &caller.token,
        kind,
        started_secs(&app),
    )))
}

/// Chat with a `chat` or `vision` model whose `api` is `openai`, in the OpenAI Chat Completions
/// format. `model` is a name from GET /v1/models; the bridge replaces it with the upstream's name,
/// fills in the model's `defaults` where your body does not set them, and passes everything else
/// on unchanged. The answer comes back as the model's server sent it. For long generations send
/// `stream: true`: an answer that has not started within 90 seconds is cut, a stream is kept alive.
#[utoipa::path(post, path = "/v1/chat/completions", tag = "models",
    request_body(content = serde_json::Value, description = "An OpenAI chat completion request"),
    responses(
        (status = 200, description = "The upstream's answer: a chat completion, or server-sent events with `stream: true`", body = serde_json::Value),
    ))]
async fn chat_completions(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    request: axum::extract::Request,
) -> axum::response::Response {
    models::call_json(&app, &models::CHAT_COMPLETIONS, &caller, request).await
}

/// Chat with a `chat` or `vision` model whose `api` is `anthropic`, in the Anthropic Messages
/// format. Anthropic's client library uses the bridge's base URL without `/v1` and sends the
/// bridge token as its API key in `x-api-key`. `anthropic-version` and `anthropic-beta` are
/// passed on; `model`, `defaults` and streaming work as for POST /v1/chat/completions.
#[utoipa::path(post, path = "/v1/messages", tag = "models",
    request_body(content = serde_json::Value, description = "An Anthropic Messages request"),
    responses(
        (status = 200, description = "The upstream's answer: a message, or server-sent events with `stream: true`", body = serde_json::Value),
    ))]
async fn messages(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    request: axum::extract::Request,
) -> axum::response::Response {
    models::call_json(&app, &models::MESSAGES, &caller, request).await
}

/// Embeddings from an `embedding` model, in the OpenAI format. `dimensions` sets the vector size
/// on models that support it; the model's `defaults` in GET /v1/models show what is filled in.
/// The call has no stream mode: it must start to answer within 90 seconds, so send large inputs
/// in batches.
#[utoipa::path(post, path = "/v1/embeddings", tag = "models",
    request_body(content = serde_json::Value, description = "An OpenAI embeddings request"),
    responses(
        (status = 200, description = "The upstream's answer", body = serde_json::Value),
    ))]
async fn embeddings(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    request: axum::extract::Request,
) -> axum::response::Response {
    models::call_json(&app, &models::EMBEDDINGS, &caller, request).await
}

/// Reranks documents against a query with a `reranker` model, in the format of vLLM, Jina and
/// Cohere (`query`, `documents`, optional `top_n`). The call must start to answer within 90
/// seconds.
#[utoipa::path(post, path = "/v1/rerank", tag = "models",
    request_body(content = serde_json::Value, description = "A rerank request"),
    responses(
        (status = 200, description = "The upstream's answer", body = serde_json::Value),
    ))]
async fn rerank(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    request: axum::extract::Request,
) -> axum::response::Response {
    models::call_json(&app, &models::RERANK, &caller, request).await
}

/// Speech from text with a `text-to-speech` model, in the OpenAI format (`input`, `voice`,
/// `response_format`). The audio comes back as the upstream sent it, with its content type.
#[utoipa::path(post, path = "/v1/audio/speech", tag = "models",
    request_body(content = serde_json::Value, description = "An OpenAI speech request"),
    responses(
        (status = 200, description = "The audio, with the upstream's content type such as `audio/mpeg`", content_type = "application/octet-stream"),
    ))]
async fn speech(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    request: axum::extract::Request,
) -> axum::response::Response {
    models::call_json(&app, &models::SPEECH, &caller, request).await
}

/// Images from a prompt with an `image-generator` model, in the OpenAI format. The call must
/// start to answer within 90 seconds.
#[utoipa::path(post, path = "/v1/images/generations", tag = "models",
    request_body(content = serde_json::Value, description = "An OpenAI image generation request"),
    responses(
        (status = 200, description = "The upstream's answer", body = serde_json::Value),
    ))]
async fn image_generations(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    request: axum::extract::Request,
) -> axum::response::Response {
    models::call_json(&app, &models::IMAGES, &caller, request).await
}

/// A transcription form as OpenAI's API takes it. Further fields such as `language`, `prompt`,
/// `response_format`, `temperature` or `stream` are passed on as they are.
#[derive(ToSchema)]
pub struct TranscriptionForm {
    /// A `speech-to-text` model from GET /v1/models.
    pub model: String,
    /// The audio file, with its file name and content type.
    #[schema(value_type = String, format = Binary)]
    pub file: Vec<u8>,
}

/// Transcribes audio with a `speech-to-text` model such as Whisper, in the OpenAI format: a
/// multipart form with `model` and `file`. The bridge replaces `model` with the upstream's name
/// and adds the model's `defaults` (for example `language`) as form fields where your form does
/// not set them. The file may be up to 96 MB. A long recording needs `stream: true` where the
/// upstream supports it, because an answer that has not started within 90 seconds is cut.
#[utoipa::path(post, path = "/v1/audio/transcriptions", tag = "models",
    request_body(content = inline(TranscriptionForm), content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "The upstream's answer: JSON or text, depending on `response_format`", body = serde_json::Value),
        (status = 413, description = "`too_large`: the form is larger than 96 MB", body = ErrorBody),
    ))]
async fn transcriptions(
    State(app): State<AppState>,
    Extension(caller): Extension<Caller>,
    request: axum::extract::Request,
) -> axum::response::Response {
    models::call_form(&app, &models::TRANSCRIPTIONS, &caller, request).await
}
