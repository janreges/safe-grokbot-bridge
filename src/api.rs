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
use crate::config::{AgentKind, Capability};
use crate::error::{ApiError, ErrorBody};
use crate::exec;
use crate::files::{self, FileContent, FileList};
use crate::git::{self, GitOutput, GitRequest};
use crate::paths;
use crate::server::AppState;
use crate::sessions::{InputRequest, MAX_WAIT_SECS, Runner, Session, SessionList, StartRequest};

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
        (name = "sessions", description = "Agent sessions: list, start, talk to, stop"),
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
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(openapi_document))
        .routes(routes!(info))
        .routes(routes!(list_files))
        .routes(routes!(read_file))
        .routes(routes!(git_status))
        .routes(routes!(git_log))
        .routes(routes!(git_diff))
        .routes(routes!(git_show))
        .routes(routes!(list_sessions, start_session))
        .routes(routes!(get_session, stop_session))
        .routes(routes!(send_input))
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
            let responses = &mut operation.responses.responses;
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
            if has_body {
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
async fn healthz() -> &'static str {
    "ok"
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
}

/// Describes this machine: version, agents, the workspaces you may access and the limits.
/// Call this first.
#[utoipa::path(get, path = "/v1/info", tag = "info", responses(
    (status = 200, description = "This machine, its agents and the workspaces you may access", body = Info),
    (status = 401, description = "Missing or invalid token", body = ErrorBody),
))]
async fn info(State(app): State<AppState>) -> Json<Info> {
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
                available: exec::which(&agent.command).is_some(),
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
        limits: LimitsInfo {
            requests_per_minute: config.limits.requests_per_minute,
            max_sessions: config.limits.max_sessions,
            max_wait_secs: MAX_WAIT_SECS,
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
    (status = 409, description = "The conversation to resume is open elsewhere", body = ErrorBody),
    (status = 429, description = "Too many running sessions", body = ErrorBody),
    (status = 502, description = "The agent exited right after starting", body = ErrorBody),
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
