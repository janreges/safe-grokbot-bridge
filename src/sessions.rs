//! The session registry: lists agent sessions (tmux panes, headless conversations and sessions
//! running outside tmux), starts them, sends input, reads output and stops them.
//!
//! Sessions the bridge starts are kept as [`Record`]s. Everything else is discovered on each
//! request: tmux panes running a configured agent, and Claude Code / Codex sessions found by
//! [`external`]. The state of a session is derived from the best source available: Claude
//! Code's own status, turn-complete hook events, or terminal activity.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::agents::{self, Effort, HookSpec, Launch};
use crate::config::{Agent, AgentKind, Capability, Config, RunnerChoice, Workspace};
use crate::error::ApiError;
use crate::exec::{self, shell_quote};
use crate::external::{self, ClaudeAgent, CodexThread};
use crate::files::rfc3339;
use crate::headless;
use crate::hook::{self, TurnEvent, unix_now, unix_now_ms};
use crate::paths;
use crate::procs::{self, Snapshot};
use crate::tmux::{self, Pane, Tmux};

/// Longest a client may long-poll; stays below Cloudflare's 125 s proxy read timeout.
pub const MAX_WAIT_SECS: u64 = 90;
const MAX_TEXT: usize = 100_000;
const MAX_KEYS: usize = 32;
const MAX_LINES: usize = 2000;
const DEFAULT_LINES: usize = 200;
const FINAL_SCREEN_LINES: usize = 500;
/// A pane counts as working while it produced output within this many seconds.
const ACTIVE_SECS: u64 = 3;
/// A tracked turn without output for this long is probably waiting for the user.
const WAITING_SECS: u64 = 15;
const CLAUDE_CACHE: Duration = Duration::from_secs(2);
/// How long finished, exited and stopped sessions stay listed.
const RETENTION: Duration = Duration::from_secs(3600);
/// How long a message handed to an agent's own messaging counts as unanswered.
const PENDING: Duration = Duration::from_secs(600);

/// tmux key names accepted in `keys`, besides single printable characters.
pub const KEYS: &[&str] = &[
    "Enter", "Escape", "Tab", "BTab", "Up", "Down", "Left", "Right", "PageUp", "PageDown", "Home",
    "End", "Space", "BSpace", "DC", "C-c", "C-d",
];

// ---------------------------------------------------------------------------------------------
// API types
// ---------------------------------------------------------------------------------------------

/// How a session runs.
#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    /// A live terminal UI in tmux; a person can attach to it.
    Tmux,
    /// One non-interactive agent run per message, continued by session id.
    Headless,
    /// Running outside tmux (a terminal tab, an IDE); found but not started by the bridge.
    External,
}

#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    /// The agent is working on a turn. Long-poll with `wait` until this changes.
    Working,
    /// The agent waits for the user: a permission prompt, a question or a menu. Read `screen`
    /// and answer with input.
    Waiting,
    /// Ready for the next message.
    Idle,
    /// Done and closed by the bridge (`keep_alive: false`). `last_message` holds the answer.
    Finished,
    /// The agent process or its terminal is gone, or the session was stopped.
    Exited,
    /// The last turn failed; see `error`. Headless sessions accept a new message.
    Failed,
    /// The state could not be determined.
    Unknown,
}

/// An agent session.
#[derive(Serialize, ToSchema, Debug, Clone)]
pub struct Session {
    /// Session id for the `/v1/sessions/{id}` endpoints.
    pub id: String,
    /// Agent name from the configuration.
    pub agent: String,
    pub kind: AgentKind,
    pub runner: Runner,
    /// Working directory (absolute); usable with the file and git endpoints.
    pub cwd: String,
    pub workspace: String,
    pub state: SessionState,
    /// What the agent waits for when `state` is `waiting`, e.g. `permission prompt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// Whether the session accepts input through this bridge.
    pub controllable: bool,
    /// Started through this bridge (only those can be stopped with `DELETE`).
    pub started_by_bridge: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<bool>,
    /// The agent's own id for this conversation (Claude session id, Codex thread id, Hermes
    /// session id). Pass it as `resume` to continue the conversation in a new session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Last terminal output (tmux sessions), RFC 3339.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<String>,
    /// The agent's last answer. A headless session clears it when a new message is sent and
    /// shows the newest progress message while the turn runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message_at: Option<String>,
    /// How a person can open this session in a terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_command: Option<String>,
    /// The last lines of the terminal (only in `GET /v1/sessions/{id}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Hints about this session, e.g. why it cannot take input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,

    #[serde(skip)]
    target: Option<String>,
    #[serde(skip)]
    pid: Option<u32>,
    #[serde(skip)]
    codex_home: Option<PathBuf>,
    /// Text goes through the agent's own messaging (Claude Code inbox, `codex queue`) instead
    /// of the terminal.
    #[serde(skip)]
    native: bool,
    /// The process of the tmux pane when the session was looked up.
    #[serde(skip)]
    pane_pid: Option<u32>,
}

/// Sessions in the workspaces this token may see.
#[derive(Serialize, ToSchema, Debug)]
pub struct SessionList {
    pub sessions: Vec<Session>,
    /// Sources that could not be queried; the list is incomplete when this is not empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Explains an empty list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Starts an agent session.
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    /// Existing directory (absolute path) inside a workspace with the `start` capability.
    pub cwd: String,
    /// Agent name from `GET /v1/info`, allowed in that workspace.
    pub agent: String,
    /// `tmux` for a live terminal session (a person can attach to it), `headless` for one
    /// non-interactive run per message. Default: the bridge's configured runner.
    pub runner: Option<RunnerChoice>,
    /// First message to the agent, at most 100 000 bytes.
    pub prompt: Option<String>,
    /// Model, passed with the agent's model flag (e.g. `opus`, `gpt-6.1-sol`). Replaces a model
    /// set in the agent's configured `args`.
    pub model: Option<String>,
    pub effort: Option<Effort>,
    /// Run without permission prompts (e.g. `--dangerously-skip-permissions`). Needs
    /// `allow_bypass_permissions` on the workspace.
    #[serde(default)]
    pub bypass_permissions: bool,
    /// `true` (default) keeps the session after it answers. `false` closes it once the first
    /// answer is in; the answer stays readable in `last_message`. In tmux this needs an agent
    /// that reports finished turns (Claude Code, Codex).
    pub keep_alive: Option<bool>,
    /// Continue an earlier conversation held in this same folder: the agent's session id
    /// (`agent_session_id`).
    pub resume: Option<String>,
}

/// Input for a session: text, keys, or both (text first).
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct InputRequest {
    /// Message to the agent, at most 100 000 bytes. In tmux it is pasted as one block and
    /// submitted with Enter.
    pub text: Option<String>,
    /// tmux sessions only: keys to press, e.g. `["Down", "Enter"]` or `["1"]` to answer a menu.
    /// Allowed: `Enter`, `Escape`, `Tab`, `BTab`, `Up`, `Down`, `Left`, `Right`, `PageUp`,
    /// `PageDown`, `Home`, `End`, `Space`, `BSpace`, `DC`, `C-c`, `C-d`, or one printable
    /// character.
    pub keys: Option<Vec<String>>,
    /// tmux sessions only: press Enter after pasting `text` (default `true`).
    pub submit: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// Records of sessions the bridge started
// ---------------------------------------------------------------------------------------------

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Live tmux session, or headless session waiting for a message.
    #[default]
    Idle,
    /// The tmux session is being created.
    Starting,
    /// A headless turn is running.
    Working,
    /// The last headless turn failed.
    Failed,
    /// Closed after its answer (`keep_alive: false`).
    Finished,
    /// Stopped, or the tmux session disappeared.
    Exited,
}

pub struct Record {
    pub id: String,
    pub agent: Agent,
    pub runner: Runner,
    pub cwd: PathBuf,
    /// The identity of `cwd` when the session started (Unix); headless turns run only there.
    pub cwd_id: Option<(u64, u64)>,
    pub workspace: String,
    pub keep_alive: bool,
    /// Options for every turn (model, effort, bypass); headless follow-ups add `resume`.
    pub launch: Launch,
    pub started_at: SystemTime,
    /// Turn events written by the agent hook (tmux Claude and Codex sessions).
    pub events: Option<PathBuf>,
    state: Mutex<RecordState>,
}

#[derive(Default)]
pub struct RecordState {
    pub phase: Phase,
    /// The tmux pane the agent runs in; other panes someone opens in the session are ignored.
    pub pane: Option<String>,
    /// The pane's process. When it changes (the pane was respawned, say with a shell), the
    /// pane no longer runs the agent the bridge started.
    pub pane_pid: Option<u32>,
    pub agent_session_id: Option<String>,
    pub last_message: Option<String>,
    pub last_message_at: Option<SystemTime>,
    /// Unix time in milliseconds of the last message the bridge submitted, taken before
    /// submitting it.
    pub last_input_ms: Option<u64>,
    /// Unix time in milliseconds of the last completed turn reported by the hook.
    pub last_turn_ms: Option<u64>,
    /// Recent texts the bridge sent (to recognise Codex's main thread in hook events).
    pub sent: Vec<String>,
    pub turns: u32,
    pub error: Option<String>,
    pub final_screen: Option<String>,
    pub child_pid: Option<u32>,
    pub ended_at: Option<SystemTime>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Record {
    pub fn state(&self) -> MutexGuard<'_, RecordState> {
        lock(&self.state)
    }

    /// Launch options for the next headless turn: continue the conversation once it exists.
    fn launch_for(&self, state: &RecordState) -> Launch {
        let mut launch = self.launch.clone();
        if state.turns > 0
            && let Some(id) = &state.agent_session_id
        {
            launch.resume = Some(id.clone());
            launch.claude_session_id = None;
        }
        launch
    }

    fn live(&self) -> bool {
        !matches!(self.state().phase, Phase::Finished | Phase::Exited)
    }

    /// tmux target of the agent's pane. psmux numbers panes per session, so there the target is
    /// the session, whose only pane is the agent's.
    fn target(&self) -> String {
        match &self.state().pane {
            Some(pane) if !cfg!(windows) => pane.clone(),
            _ => tmux::session_target(&self.id),
        }
    }
}

/// What a tmux session needs to survive a restart of the bridge, kept in the state directory.
#[derive(Serialize, Deserialize)]
struct RecordMeta {
    #[serde(default = "yes")]
    keep_alive: bool,
    #[serde(default)]
    last_input_ms: Option<u64>,
    #[serde(default)]
    sent: Vec<String>,
}

fn yes() -> bool {
    true
}

// ---------------------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------------------

/// Sessions reported by `claude agents --json` for every Claude profile, and the profiles whose
/// query failed.
type ClaudeList = Arc<(Vec<ClaudeAgent>, Vec<String>)>;

pub struct Sessions {
    config: Arc<Config>,
    pub tmux: Option<Tmux>,
    pub tmux_version: Option<String>,
    events_dir: PathBuf,
    meta_dir: PathBuf,
    exe: PathBuf,
    records: Mutex<HashMap<String, Arc<Record>>>,
    claude_cache: tokio::sync::Mutex<Option<(Instant, ClaudeList)>>,
    /// Keeps the paste, Enter and keys of one input request together, so concurrent requests
    /// cannot interleave in a pane.
    tmux_input: tokio::sync::Mutex<()>,
    /// Messages handed to an agent's own messaging (inbox, `codex queue`) that have no answer
    /// yet: session id → Unix time in milliseconds the message was sent.
    pending: Mutex<HashMap<String, u64>>,
}

/// What a listing is built from.
struct Context {
    /// Records and their phase before tmux was queried. Only a record that was live then can
    /// be missing from the pane list because its pane is gone.
    records: Vec<(Arc<Record>, Phase)>,
    panes: Vec<Pane>,
    /// The pane list is reliable; when false, missing panes say nothing about their sessions.
    tmux_ok: bool,
    snapshot: Snapshot,
    claude: Vec<ClaudeAgent>,
    /// Live Codex sessions and their threads (experimental external sessions only).
    codex: Vec<CodexThread>,
    now: u64,
    /// Sources that failed; sessions from them are missing.
    problems: Vec<String>,
}

impl Sessions {
    pub async fn new(config: Arc<Config>) -> Sessions {
        let tmux = Tmux::new(
            &config.sessions.tmux,
            config.sessions.tmux_socket.as_deref(),
        );
        let tmux_version = tmux.version().await;
        if tmux_version.is_none() {
            tracing::info!(
                "`{}` is not available; sessions will run headless",
                config.sessions.tmux
            );
        }
        Sessions {
            tmux: tmux_version.is_some().then_some(tmux),
            tmux_version,
            events_dir: config.server.state_dir.join("events"),
            meta_dir: config.server.state_dir.join("sessions"),
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("safe-grokbot-bridge")),
            records: Mutex::new(HashMap::new()),
            claude_cache: tokio::sync::Mutex::new(None),
            tmux_input: tokio::sync::Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
            config,
        }
    }

    /// The runner used when a start request does not choose one.
    pub fn default_runner(&self) -> Runner {
        match self.config.sessions.runner {
            RunnerChoice::Tmux => Runner::Tmux,
            RunnerChoice::Headless => Runner::Headless,
            RunnerChoice::Auto if self.tmux.is_some() => Runner::Tmux,
            RunnerChoice::Auto => Runner::Headless,
        }
    }

    fn record(&self, id: &str) -> Option<Arc<Record>> {
        lock(&self.records).get(id).cloned()
    }

    fn workspace(&self, name: &str) -> Option<&Workspace> {
        self.config.workspaces.iter().find(|ws| ws.name == name)
    }

    /// `claude agents --json` of every configured Claude profile (distinct config directory),
    /// cached for a moment because listings run it on every request.
    async fn claude_agents(&self) -> ClaudeList {
        let mut cache = self.claude_cache.lock().await;
        if let Some((at, list)) = cache.as_ref()
            && at.elapsed() < CLAUDE_CACHE
        {
            return list.clone();
        }
        let mut found: Vec<ClaudeAgent> = Vec::new();
        let mut errors = Vec::new();
        let mut dirs = Vec::new();
        let profiles = self.config.agents.iter().filter(|a| {
            a.kind == AgentKind::Claude && exec::which_with(&a.command, &a.env).is_some()
        });
        // Profiles are queried at once, so a listing takes at most one query's timeout.
        let mut queries = tokio::task::JoinSet::new();
        for (index, agent) in profiles.enumerate() {
            let dir = external::claude_dir(agent);
            if dirs.contains(&dir) {
                continue;
            }
            dirs.push(dir);
            let agent = agent.clone();
            queries.spawn(async move {
                let result = external::claude_agents(&agent).await;
                (index, agent, result)
            });
        }
        let mut results = queries.join_all().await;
        results.sort_by_key(|(index, _, _)| *index);
        for (_, agent, result) in results {
            match result {
                Ok(list) => {
                    for entry in list {
                        let duplicate = entry.session_id.is_some()
                            && found.iter().any(|e| e.session_id == entry.session_id);
                        if !duplicate {
                            found.push(entry);
                        }
                    }
                }
                Err(err) => errors.push(format!(
                    "`claude agents --json` for agent `{}` failed ({err}); its Claude Code sessions outside tmux are missing and their states are estimated from terminal activity.",
                    agent.name
                )),
            }
        }
        let list = Arc::new((found, errors));
        *cache = Some((Instant::now(), list.clone()));
        list
    }

    async fn context(&self) -> Context {
        let records = lock(&self.records)
            .values()
            .map(|record| (record.clone(), record.state().phase))
            .collect();
        let mut problems = Vec::new();
        let (panes, tmux_ok) = match &self.tmux {
            Some(tmux) => match tmux.list_panes().await {
                Ok(panes) => (panes, true),
                Err(err) => {
                    problems.push(format!(
                        "tmux could not be queried ({err:#}); tmux sessions are missing from this list."
                    ));
                    (Vec::new(), false)
                }
            },
            None => (Vec::new(), true),
        };
        let with_environ = self.config.experimental.external_sessions;
        let mut codex_commands: Vec<String> = Vec::new();
        for agent in &self.config.agents {
            if with_environ
                && agent.kind == AgentKind::Codex
                && !codex_commands.contains(&agent.command)
            {
                codex_commands.push(agent.command.clone());
            }
        }
        let (snapshot, codex) = tokio::task::spawn_blocking(move || {
            let snapshot = Snapshot::take(with_environ);
            // A wrapper (`codex.mnt`) and the Codex it starts can both match a configured
            // command; each process tree counts once.
            let mut codex: Vec<CodexThread> = Vec::new();
            let mut covered: HashSet<u32> = HashSet::new();
            for command in &codex_commands {
                for thread in external::codex_threads(&snapshot, command) {
                    let tree: Vec<u32> =
                        snapshot.tree(thread.pid, 8).iter().map(|p| p.pid).collect();
                    if tree.iter().any(|pid| covered.contains(pid)) {
                        continue;
                    }
                    covered.extend(tree);
                    codex.push(thread);
                }
            }
            (snapshot, codex)
        })
        .await
        .unwrap_or_default();
        let claude = self.claude_agents().await;
        problems.extend(claude.1.iter().cloned());
        Context {
            records,
            panes,
            tmux_ok,
            snapshot,
            claude: claude.0.clone(),
            codex,
            now: unix_now(),
            problems,
        }
    }

    /// Removes the turn events and saved data of sessions that are gone, once the retention
    /// period has passed. Run at start, it catches sessions that ended while the bridge was
    /// stopped.
    pub fn prune_state(&self) {
        let records = lock(&self.records);
        for dir in [&self.events_dir, &self.meta_dir] {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                let known = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|id| records.contains_key(id));
                let expired = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|at| at.elapsed().unwrap_or_default() > RETENTION);
                if !known && expired {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    /// Drops ended sessions after the retention period.
    fn cleanup(&self) {
        let mut records = lock(&self.records);
        records.retain(|id, record| {
            let expired = record
                .state()
                .ended_at
                .is_some_and(|at| at.elapsed().unwrap_or_default() > RETENTION);
            if expired {
                if let Some(events) = &record.events {
                    let _ = std::fs::remove_file(events);
                }
                let _ = std::fs::remove_file(self.meta_dir.join(format!("{id}.json")));
            }
            !expired
        });
    }

    /// All sessions in workspaces with the `sessions` capability, optionally only those whose
    /// working directory is under `under`, and the sources that could not be queried.
    pub async fn list(self: &Arc<Self>, under: Option<&Path>) -> (Vec<Session>, Vec<String>) {
        self.cleanup();
        let context = self.context().await;
        let mut sessions = self.collect(&context);
        self.apply_pending(&mut sessions);
        if let Some(under) = under {
            sessions.retain(|s| Path::new(&s.cwd).starts_with(under));
        }
        (sessions, context.problems)
    }

    /// A message handed to an agent's own messaging shows nowhere until the agent takes it up,
    /// so until its answer arrives (at most for `PENDING`) the session counts as working.
    fn apply_pending(&self, sessions: &mut [Session]) {
        let mut pending = lock(&self.pending);
        let now = unix_now_ms();
        pending.retain(|_, sent| now.saturating_sub(*sent) < PENDING.as_millis() as u64);
        for session in sessions {
            let Some(&sent) = pending.get(&session.id) else {
                continue;
            };
            let answered = session
                .last_message_at
                .as_deref()
                .and_then(|at| humantime::parse_rfc3339_weak(at.trim_end_matches('Z')).ok())
                .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                .is_some_and(|at| at.as_millis() as u64 >= sent);
            if answered {
                pending.remove(&session.id);
            } else if matches!(session.state, SessionState::Idle | SessionState::Unknown) {
                session.state = SessionState::Working;
                let note = "The last message was handed to the agent and its answer has not arrived yet; `last_message` is still the previous answer.";
                session.note = Some(match session.note.take() {
                    Some(other) => format!("{note} {other}"),
                    None => note.into(),
                });
            }
        }
    }

    fn collect(self: &Arc<Self>, ctx: &Context) -> Vec<Session> {
        let mut sessions = Vec::new();
        let mut in_tmux: HashSet<u32> = HashSet::new();
        let mut observed: HashSet<String> = HashSet::new();
        for pane in &ctx.panes {
            in_tmux.extend(ctx.snapshot.tree(pane.pid, 8).iter().map(|p| p.pid));
            in_tmux.insert(pane.pid);
            let record = ctx
                .records
                .iter()
                .map(|(record, _)| record)
                .find(|r| r.runner == Runner::Tmux && r.id == pane.session)
                .cloned()
                .or_else(|| self.adopt(pane, ctx));
            match &record {
                Some(record) => {
                    if !record.live() || observed.contains(&record.id) {
                        continue;
                    }
                    let mut state = record.state();
                    match &state.pane {
                        // Another pane someone opened in the bridge's session: not the agent.
                        Some(agent_pane) if *agent_pane != pane.id => continue,
                        // The pane was respawned with another program: the agent is gone.
                        Some(_) if state.pane_pid.is_some_and(|pid| pid != pane.pid) => continue,
                        Some(_) => {}
                        None => {
                            let agent = std::slice::from_ref(&record.agent);
                            if ctx.snapshot.find_agent(pane.pid, agent).is_none() {
                                continue;
                            }
                            state.pane = Some(pane.id.clone());
                            state.pane_pid = Some(pane.pid);
                        }
                    }
                    if pane.dead && state.phase == Phase::Idle {
                        // A pane kept after its agent exited (`remain-on-exit`).
                        state.phase = Phase::Exited;
                        state.ended_at = Some(SystemTime::now());
                    }
                    drop(state);
                    observed.insert(record.id.clone());
                }
                // With psmux, pane ids are not unique across sessions; only sessions the bridge
                // started are supported there.
                None if cfg!(windows) => continue,
                None => {}
            }
            sessions.extend(self.tmux_view(ctx, pane, record.as_ref()));
        }
        for (record, phase) in &ctx.records {
            if observed.contains(&record.id) {
                continue;
            }
            if record.runner == Runner::Tmux && ctx.tmux_ok && *phase == Phase::Idle {
                let mut state = record.state();
                if state.phase == Phase::Idle {
                    // The agent's pane is gone (the agent exited or someone closed it).
                    state.phase = Phase::Exited;
                    state.ended_at = Some(SystemTime::now());
                }
            }
            sessions.extend(self.record_view(record, ctx.tmux_ok));
        }
        sessions.extend(self.claude_views(ctx, &in_tmux));
        sessions.extend(self.codex_views(ctx, &in_tmux));
        sessions
    }

    /// Takes over a bridge-named tmux session (`sgb-<dir>-<agent>-<hex>`) without a record: one
    /// started before a restart of the bridge, or with `safe-grokbot-bridge launch`.
    fn adopt(self: &Arc<Self>, pane: &Pane, ctx: &Context) -> Option<Arc<Record>> {
        let rest = pane.session.strip_prefix("sgb-")?;
        let (prefix, suffix) = rest.rsplit_once('-')?;
        if suffix.len() != 6 || !suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let agent = self
            .config
            .agents
            .iter()
            .filter(|a| prefix.ends_with(&format!("-{}", a.name)))
            .max_by_key(|a| a.name.len())?;
        // Take the pane the agent runs in, not a shell someone opened next to it, and only one
        // where nothing but the agent can receive typed text.
        let (_, agent_pid) = ctx
            .snapshot
            .find_agent(pane.pid, std::slice::from_ref(agent))?;
        if !self.owned_by_agent(ctx, pane, agent, agent_pid) {
            return None;
        }
        let cwd = dunce::canonicalize(&pane.cwd).ok()?;
        let workspace = paths::workspace_for(&self.config, &cwd)?;
        let meta = self.load_meta(&pane.session);
        let events = matches!(agent.kind, AgentKind::Claude | AgentKind::Codex)
            .then(|| self.events_dir.join(format!("{}.jsonl", pane.session)));
        let record = Arc::new(Record {
            id: pane.session.clone(),
            agent: agent.clone(),
            runner: Runner::Tmux,
            cwd,
            cwd_id: None,
            workspace: workspace.name.clone(),
            keep_alive: meta.keep_alive,
            launch: Launch::default(),
            started_at: unix_time(pane.created),
            events,
            state: Mutex::new(RecordState {
                pane: Some(pane.id.clone()),
                pane_pid: Some(pane.pid),
                last_input_ms: meta.last_input_ms,
                sent: meta.sent,
                ..RecordState::default()
            }),
        });
        // A start may have registered this session since the listing began.
        let existing = lock(&self.records).get(&record.id).cloned();
        if let Some(existing) = existing {
            return Some(existing);
        }
        lock(&self.records).insert(record.id.clone(), record.clone());
        if !record.keep_alive {
            self.watch_until_answered(record.id.clone());
        }
        Some(record)
    }

    /// Whether only the agent can receive what is typed into a pane: the pane's program is the
    /// file of the agent's configured command (trusted to end with the agent, even as a wrapper
    /// script), or no process between the pane and the agent may read the terminal after its
    /// child exits.
    fn owned_by_agent(&self, ctx: &Context, pane: &Pane, agent: &Agent, agent_pid: u32) -> bool {
        let configured = exec::which_with(&agent.command, &agent.env)
            .and_then(|path| dunce::canonicalize(path).ok());
        let launched = configured.is_some_and(|configured| {
            ctx.snapshot
                .get(pane.pid)
                .is_some_and(|program| procs::program_files(program).contains(&configured))
        });
        launched || !ctx.snapshot.shell_between(pane.pid, agent_pid)
    }

    fn tmux_view(
        &self,
        ctx: &Context,
        pane: &Pane,
        record: Option<&Arc<Record>>,
    ) -> Option<Session> {
        // Typing into a pane is safe only when nothing but the agent can receive the text. The
        // bridge's own panes run the agent as the pane's program (`collect` checks its pid). In
        // other panes no interactive shell may sit between the pane and the agent, because text
        // sent after the agent exits would reach that shell.
        let (agent, cwd, workspace, owned) = match record {
            Some(record) => (
                record.agent.clone(),
                record.cwd.clone(),
                record.workspace.clone(),
                true,
            ),
            None => {
                let cwd = dunce::canonicalize(&pane.cwd).ok()?;
                let workspace = paths::workspace_for(&self.config, &cwd)?.name.clone();
                let (agent, agent_pid) = ctx.snapshot.find_agent(pane.pid, &self.config.agents)?;
                let owned = self.owned_by_agent(ctx, pane, agent, agent_pid);
                (agent.clone(), cwd, workspace, owned)
            }
        };
        let ws = self.workspace(&workspace)?;
        if !ws.allows(Capability::Sessions) {
            return None;
        }
        let tree: HashSet<u32> = ctx
            .snapshot
            .tree(pane.pid, 8)
            .iter()
            .map(|p| p.pid)
            .chain([pane.pid])
            .collect();
        let claude = (agent.kind == AgentKind::Claude)
            .then(|| {
                ctx.claude
                    .iter()
                    .find(|c| c.pid.is_some_and(|pid| tree.contains(&pid)))
            })
            .flatten();
        let codex = (agent.kind == AgentKind::Codex)
            .then(|| ctx.codex.iter().find(|t| tree.contains(&t.pid)))
            .flatten();

        let mut session = Session::new(
            record
                .map(|r| r.id.clone())
                .unwrap_or_else(|| format!("tmux-{}", pane.id.trim_start_matches('%'))),
            &agent,
            Runner::Tmux,
            &cwd,
            &workspace,
        );
        // An agent behind a shell can still take text through its own messaging (experimental).
        let experimental = !owned && self.config.experimental.external_sessions;
        let claude_dir = external::claude_dir(&agent);
        let inbox = claude
            .filter(|c| {
                experimental
                    && c.session_id
                        .as_deref()
                        .zip(claude_dir.as_deref())
                        .is_some_and(|(id, dir)| claude_began_in(dir, &cwd, id))
            })
            .and_then(|c| c.pid)
            .zip(claude_dir.as_deref())
            .and_then(|(pid, dir)| external::claude_inbox(dir, pid));
        let native = experimental && (inbox.is_some() || codex.is_some_and(|t| !t.ambiguous));
        // psmux addresses a session's active pane, not a pane id, so a session with several panes
        // could route text to the wrong one.
        let alone = !cfg!(windows)
            || ctx
                .panes
                .iter()
                .filter(|p| p.session == pane.session)
                .count()
                == 1;
        session.controllable =
            ws.allows(Capability::Input) && !pane.dead && ((owned && alone) || native);
        session.native = native;
        session.pane_pid = Some(pane.pid);
        session.last_activity_at = (pane.activity > 0).then(|| rfc3339(unix_time(pane.activity)));
        session.attach_command = self.tmux.as_ref().map(|t| t.attach_command(&pane.session));
        session.target = Some(if cfg!(windows) {
            tmux::session_target(&pane.session)
        } else {
            pane.id.clone()
        });
        session.agent_session_id = claude.and_then(|c| c.session_id.clone());
        if native {
            session.pid = claude.and_then(|c| c.pid);
            if let Some(thread) = codex {
                session.pid = Some(thread.pid);
                session.codex_home = Some(thread.home.clone());
                session.agent_session_id = Some(thread.id.clone());
                if let Some(progress) = external::codex_progress(&thread.rollout) {
                    session.last_message = progress.last_message;
                    session.last_message_at = progress.last_message_at;
                }
            }
            if let Some((text, at)) = claude
                .and_then(|c| c.session_id.as_deref())
                .zip(claude_dir.as_deref())
                .and_then(|(id, dir)| external::claude_last_message(dir, &cwd, id))
            {
                session.last_message = Some(text);
                session.last_message_at = Some(at).filter(|at| !at.is_empty());
            }
        }
        session.started_at = (pane.created > 0).then(|| rfc3339(unix_time(pane.created)));

        let (input_ms, turn_ms) = match record {
            Some(record) => {
                let events = record
                    .events
                    .as_deref()
                    .map(hook::read_events)
                    .unwrap_or_default();
                let mut state = record.state();
                let turns = main_thread_events(agent.kind, &events, &mut state);
                if let Some(last) = turns.last() {
                    state.last_turn_ms = Some(last.at_ms);
                    if last.message.is_some() {
                        state.last_message = last.message.clone();
                        state.last_message_at =
                            Some(UNIX_EPOCH + Duration::from_millis(last.at_ms));
                    }
                    if agent.kind == AgentKind::Claude && last.session_id.is_some() {
                        state.agent_session_id = last.session_id.clone();
                    }
                }
                session.started_by_bridge = true;
                session.keep_alive = Some(record.keep_alive);
                session.started_at = Some(rfc3339(record.started_at));
                session.agent_session_id =
                    state.agent_session_id.clone().or(session.agent_session_id);
                session.last_message = state.last_message.clone();
                session.last_message_at = state.last_message_at.map(rfc3339);
                (state.last_input_ms, state.last_turn_ms)
            }
            None => (None, None),
        };
        let derived = tmux_state(agent.kind, pane, claude, input_ms, turn_ms, ctx.now);
        session.state = derived.state;
        session.waiting_for = derived.waiting_for;
        let mut notes = Vec::new();
        if !alone && !native {
            notes.push("This tmux session has several panes, and on Windows the bridge cannot address one of them reliably, so it does not type into it. Close the other panes to make it controllable.");
        }
        if native {
            notes.push("Experimental: this agent runs inside an interactive shell, so the bridge does not type into its terminal and delivers `text` through the agent's own messaging instead (Claude Code inbox, `codex queue`); `keys` are not supported.");
        } else if !owned {
            notes.push("This pane runs an interactive shell with the agent inside, so the bridge does not type into it: after the agent exits, the text would reach the shell. To work in this folder, start a new session with POST /v1/sessions if the workspace allows `start`, or ask the machine's owner.");
        }
        if derived.state == SessionState::Waiting {
            notes.push("The agent is waiting for an answer. Read `screen`, then reply with `text` or `keys` (e.g. [\"Enter\"] or [\"1\"]).");
        }
        if derived.estimated {
            notes.push("The state is estimated from terminal activity: a quiet agent may still be working. Check `screen`.");
        }
        session.note = (!notes.is_empty()).then(|| notes.join(" "));
        Some(session)
    }

    fn record_view(&self, record: &Record, tmux_ok: bool) -> Option<Session> {
        let ws = self.workspace(&record.workspace)?;
        if !ws.allows(Capability::Sessions) {
            return None;
        }
        let state = record.state();
        let mut session = Session::new(
            record.id.clone(),
            &record.agent,
            record.runner,
            &record.cwd,
            &record.workspace,
        );
        session.state = match state.phase {
            // A tmux session that was not observed because tmux could not be queried.
            Phase::Idle if record.runner == Runner::Tmux => SessionState::Unknown,
            Phase::Idle => SessionState::Idle,
            Phase::Starting | Phase::Working => SessionState::Working,
            Phase::Failed => SessionState::Failed,
            Phase::Finished => SessionState::Finished,
            Phase::Exited => SessionState::Exited,
        };
        session.controllable = ws.allows(Capability::Input)
            && record.runner == Runner::Headless
            && matches!(state.phase, Phase::Idle | Phase::Failed);
        session.started_by_bridge = true;
        session.keep_alive = Some(record.keep_alive);
        session.agent_session_id = state.agent_session_id.clone();
        session.started_at = Some(rfc3339(record.started_at));
        session.last_message = state.last_message.clone();
        session.last_message_at = state.last_message_at.map(rfc3339);
        session.error = state.error.clone();
        session.screen = state.final_screen.clone();
        session.attach_command = state
            .agent_session_id
            .as_ref()
            .and_then(|id| resume_command(&record.agent, &record.cwd, id));
        session.note = match state.phase {
            Phase::Finished => Some("Finished (keep_alive: false). To continue this conversation, start a new session in the same folder with `resume` set to `agent_session_id`.".into()),
            Phase::Idle if record.runner == Runner::Tmux && !tmux_ok => Some("tmux could not be queried, so the state of this session is unknown; see `warnings` in the session list.".into()),
            _ => None,
        };
        Some(session)
    }

    fn claude_views(&self, ctx: &Context, in_tmux: &HashSet<u32>) -> Vec<Session> {
        let experimental = self.config.experimental.external_sessions;
        let mut sessions = Vec::new();
        for claude in &ctx.claude {
            if claude.pid.is_some_and(|pid| in_tmux.contains(&pid)) {
                continue;
            }
            let Some(agent) = self.config.agent(&claude.agent) else {
                continue;
            };
            let Some(session_id) = &claude.session_id else {
                continue;
            };
            let Ok(cwd) = dunce::canonicalize(&claude.cwd) else {
                continue;
            };
            let Some(ws) = paths::workspace_for(&self.config, &cwd) else {
                continue;
            };
            if !ws.allows(Capability::Sessions) {
                continue;
            }
            let claude_dir = external::claude_dir(agent);
            let mut session = Session::new(
                format!("claude-{session_id}"),
                agent,
                Runner::External,
                &cwd,
                &ws.name,
            );
            let (state, waiting_for) =
                claude_state(claude).unwrap_or((SessionState::Unknown, None));
            session.state = state;
            session.waiting_for = waiting_for;
            session.agent_session_id = Some(session_id.clone());
            session.started_at = claude
                .started_at
                .map(|ms| rfc3339(UNIX_EPOCH + Duration::from_millis(ms)));
            session.pid = claude.pid;
            if claude.kind == "background" {
                session.attach_command = Some(format!(
                    "claude attach {}",
                    &session_id[..session_id.len().min(8)]
                ));
            }
            let inbox = claude
                .pid
                .zip(claude_dir.as_deref())
                .and_then(|(pid, dir)| external::claude_inbox(dir, pid));
            let began_here = claude_dir
                .as_deref()
                .is_some_and(|dir| claude_began_in(dir, &cwd, session_id));
            if !experimental {
                session.note = Some("Runs outside tmux, so the bridge can only watch it. Messaging it needs `[experimental] external_sessions` in the bridge config, which only the machine's owner can enable. Alternatively start a new session through the bridge, if this workspace allows `start`.".into());
            } else if !began_here {
                session.note = Some("This conversation began in another folder (it was resumed or moved here), so the bridge shows neither its answers nor sends it input.".into());
            } else {
                session.controllable = ws.allows(Capability::Input) && inbox.is_some();
                if let Some((text, at)) = claude_dir
                    .as_deref()
                    .and_then(|dir| external::claude_last_message(dir, &cwd, session_id))
                {
                    session.last_message = Some(text);
                    session.last_message_at = Some(at).filter(|at| !at.is_empty());
                }
                session.note = Some(if inbox.is_some() {
                    "Experimental: text is delivered to the session's inbox. A session running without permission prompts holds such messages for local approval unless its `crossSessionInbound` setting is `accept`."
                } else {
                    "Experimental messaging is on, but this session has no inbox socket (Claude Code before 2.1.224, or a session that is not running)."
                }
                .into());
            }
            sessions.push(session);
        }
        sessions
    }

    fn codex_views(&self, ctx: &Context, in_tmux: &HashSet<u32>) -> Vec<Session> {
        let mut sessions = Vec::new();
        for thread in &ctx.codex {
            // The Codex agent with this session's `CODEX_HOME`, or else the first one.
            let codex = || {
                self.config
                    .agents
                    .iter()
                    .filter(|a| a.kind == AgentKind::Codex)
            };
            let Some(agent) = codex()
                .find(|a| codex_home(a) == thread.home)
                .or_else(|| codex().next())
            else {
                continue;
            };
            if in_tmux.contains(&thread.pid) {
                continue;
            }
            let Some(ws) = paths::workspace_for(&self.config, &thread.cwd) else {
                continue;
            };
            if !ws.allows(Capability::Sessions) {
                continue;
            }
            let id = if thread.ambiguous {
                format!("codex-pid-{}", thread.pid)
            } else {
                format!("codex-{}", thread.id)
            };
            let mut session = Session::new(id, agent, Runner::External, &thread.cwd, &ws.name);
            session.pid = Some(thread.pid);
            if thread.ambiguous {
                session.note = Some("Several Codex conversations could belong to this session (other Codex sessions or recent conversations in this folder), and the bridge cannot tell which one it runs, so it shows neither its state nor its answers and sends it no input.".into());
                sessions.push(session);
                continue;
            }
            let progress = external::codex_progress(&thread.rollout);
            session.state = match progress.as_ref().and_then(|p| p.working) {
                Some(true) => SessionState::Working,
                Some(false) => SessionState::Idle,
                None => SessionState::Unknown,
            };
            if let Some(progress) = progress {
                session.last_message = progress.last_message;
                session.last_message_at = progress.last_message_at;
            }
            session.agent_session_id = Some(thread.id.clone());
            session.controllable = ws.allows(Capability::Input);
            session.codex_home = Some(thread.home.clone());
            session.attach_command = Some(command_line(
                &thread.cwd,
                &[("CODEX_HOME".into(), thread.home.display().to_string())],
                &agent.command,
                &format!("resume {}", thread.id),
            ));
            session.note = Some("Experimental: text is queued with `codex queue` and delivered when the thread is idle; it never interrupts a running turn.".into());
            sessions.push(session);
        }
        sessions
    }

    async fn find(self: &Arc<Self>, id: &str) -> Result<Session, ApiError> {
        let (sessions, problems) = self.list(None).await;
        sessions.into_iter().find(|s| s.id == id).ok_or_else(|| {
            let mut message = format!(
                "No session `{id}` in the workspaces you may see. List sessions with GET /v1/sessions."
            );
            if !problems.is_empty() {
                message.push_str(" The lookup was incomplete: ");
                message.push_str(&problems.join(" "));
            }
            ApiError::not_found(message)
        })
    }

    /// One session with its terminal output; with `wait`, first waits (up to 90 s) while the
    /// session is working.
    pub async fn get(
        self: &Arc<Self>,
        id: &str,
        lines: Option<usize>,
        wait: Option<u64>,
    ) -> Result<Session, ApiError> {
        let wait = wait.unwrap_or(0).min(MAX_WAIT_SECS);
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            let mut session = self.find(id).await?;
            let now = Instant::now();
            if session.state != SessionState::Working || now >= deadline {
                if session.state == SessionState::Working && wait > 0 {
                    let still = format!(
                        "Still working after waiting {wait} s (at most {MAX_WAIT_SECS} s per call). Call again with `wait` to keep waiting; the agent does not need another message."
                    );
                    session.note = Some(match session.note.take() {
                        Some(note) => format!("{still} {note}"),
                        None => still,
                    });
                }
                let lines = lines.unwrap_or(DEFAULT_LINES).clamp(1, MAX_LINES);
                return Ok(self.with_screen(session, lines).await);
            }
            tokio::time::sleep(Duration::from_secs(1).min(deadline - now)).await;
        }
    }

    async fn with_screen(&self, mut session: Session, lines: usize) -> Session {
        if let (Some(tmux), Some(target), true) =
            (&self.tmux, &session.target, session.runner == Runner::Tmux)
        {
            match tmux.capture(target, lines).await {
                Ok(screen) => session.screen = Some(screen),
                Err(err) => tracing::debug!("cannot capture {target}: {err:#}"),
            }
        }
        if let Some(screen) = &session.screen {
            let all: Vec<&str> = screen.lines().collect();
            if all.len() > lines {
                session.screen = Some(all[all.len() - lines..].join("\n"));
            }
        }
        session
    }

    /// Starts a session. `caller` is the token name, for the audit log.
    pub async fn start(
        self: &Arc<Self>,
        request: StartRequest,
        caller: &str,
    ) -> Result<Session, ApiError> {
        let resolved = paths::resolve(&self.config, &request.cwd)?;
        paths::require(resolved.workspace, Capability::Start)?;
        let ws = resolved.workspace;
        if !resolved.path.is_dir() {
            return Err(ApiError::bad_request(format!(
                "`{}` is not a directory.",
                request.cwd
            )));
        }
        let agent = self.config.agent(&request.agent).ok_or_else(|| {
            let names: Vec<&str> = self.config.agents.iter().map(|a| a.name.as_str()).collect();
            ApiError::bad_request(format!(
                "Unknown agent `{}`. Configured agents: {}.",
                request.agent,
                names.join(", ")
            ))
        })?;
        if !ws.agents.contains(&agent.name) {
            return Err(ApiError::forbidden(format!(
                "Agent `{}` may not be started in workspace `{}`. Allowed there: {}.",
                agent.name,
                ws.name,
                ws.agents.join(", ")
            )));
        }
        if request.bypass_permissions && !ws.allow_bypass_permissions {
            return Err(ApiError::forbidden(format!(
                "Workspace `{}` does not allow `bypass_permissions`. Start without it; the agent will ask for permission in its terminal.",
                ws.name
            )));
        }
        if let Some(prompt) = &request.prompt
            && prompt.len() > MAX_TEXT
        {
            return Err(ApiError::too_large(format!(
                "`prompt` is limited to {MAX_TEXT} bytes."
            )));
        }
        if let Some(model) = &request.model {
            agents::validate_model(model)?;
        }
        let runner = match request.runner.unwrap_or(self.config.sessions.runner) {
            RunnerChoice::Tmux => Runner::Tmux,
            RunnerChoice::Headless => Runner::Headless,
            RunnerChoice::Auto if self.tmux.is_some() => Runner::Tmux,
            RunnerChoice::Auto => Runner::Headless,
        };
        if runner == Runner::Tmux && self.tmux.is_none() {
            return Err(ApiError::unavailable(format!(
                "tmux (`{}`) is not available on this machine. Use `runner: \"headless\"`.",
                self.config.sessions.tmux
            )));
        }
        let keep_alive = request.keep_alive.unwrap_or(true);
        if agent.kind == AgentKind::Generic && runner == Runner::Headless {
            return Err(ApiError::bad_request(format!(
                "Agent `{}` is a generic terminal program and runs only in tmux.",
                agent.name
            )));
        }
        if runner == Runner::Tmux
            && !keep_alive
            && !matches!(agent.kind, AgentKind::Claude | AgentKind::Codex)
        {
            return Err(ApiError::bad_request(format!(
                "Agent `{}` does not report finished turns in tmux, so `keep_alive: false` would close it on a guess. Use `runner: \"headless\"`, or keep it alive and stop it with DELETE when it is done.",
                agent.name
            )));
        }
        if let Some(resume) = &request.resume {
            agents::validate_session_ref(resume)?;
            self.check_resume(agent, &resolved.path, resume, request.bypass_permissions)
                .await?;
            if agent.kind == AgentKind::Claude {
                let claude = self.claude_agents().await;
                let open = claude
                    .0
                    .iter()
                    .find(|c| c.session_id.as_deref() == Some(resume.as_str()) && c.pid.is_some());
                if let Some(open) = open {
                    return Err(ApiError::conflict(
                        "session_open",
                        format!(
                            "Claude session `{resume}` is open in another process (pid {}). Send input to that session instead of resuming it twice.",
                            open.pid.unwrap_or_default()
                        ),
                    ));
                }
            }
        }

        let id = new_session_name(&resolved.path, &agent.name);
        let claude_session_id = (agent.kind == AgentKind::Claude && request.resume.is_none())
            .then(|| uuid::Uuid::new_v4().to_string());
        let events = (runner == Runner::Tmux
            && matches!(agent.kind, AgentKind::Claude | AgentKind::Codex))
        .then(|| self.events_dir.join(format!("{id}.jsonl")));
        let launch = Launch {
            prompt: None,
            model: request.model.clone(),
            effort: request.effort,
            bypass: request.bypass_permissions,
            resume: request.resume.clone(),
            claude_session_id: claude_session_id.clone(),
            hook: events.clone().map(|events| HookSpec {
                exe: self.exe.clone(),
                events,
            }),
        };
        let record = Arc::new(Record {
            id: id.clone(),
            agent: agent.clone(),
            runner,
            cwd: resolved.path.clone(),
            cwd_id: std::fs::metadata(&resolved.path)
                .ok()
                .and_then(|m| crate::files::identity(&m)),
            workspace: ws.name.clone(),
            keep_alive,
            launch: launch.clone(),
            started_at: SystemTime::now(),
            events,
            state: Mutex::new(RecordState {
                phase: if runner == Runner::Tmux {
                    Phase::Starting
                } else {
                    Phase::Idle
                },
                agent_session_id: claude_session_id.or(request.resume.clone()),
                ..RecordState::default()
            }),
        });
        {
            // Count and reserve in one step, so concurrent starts cannot exceed the limit.
            let mut records = lock(&self.records);
            let active = records.values().filter(|r| r.live()).count();
            if active >= self.config.limits.max_sessions {
                return Err(ApiError::too_many(
                    "too_many_sessions",
                    format!(
                        "{active} sessions started by the bridge are still running (limit {}). Stop one with DELETE /v1/sessions/{{id}} first.",
                        self.config.limits.max_sessions
                    ),
                    30,
                ));
            }
            records.insert(id.clone(), record.clone());
        }
        tracing::info!(
            caller,
            id,
            agent = agent.name,
            runner = ?runner,
            cwd = %resolved.path.display(),
            model = request.model.as_deref().unwrap_or("-"),
            bypass = request.bypass_permissions,
            keep_alive,
            "starting session"
        );
        match runner {
            Runner::Tmux => {
                // Started in its own task, without an await between the reservation and the
                // spawn, so a client that disconnects cannot leave the session half started:
                // reserved, but without an agent or its `keep_alive` watcher.
                let sessions = self.clone();
                let started = record.clone();
                let prompt = request.prompt;
                let task = tokio::spawn(async move {
                    let result = sessions.start_tmux(&started, launch, prompt).await;
                    // A session stopped while starting stays listed as exited.
                    if result.is_err() && started.state().phase == Phase::Starting {
                        lock(&sessions.records).remove(&started.id);
                        let meta = sessions.meta_dir.join(format!("{}.json", started.id));
                        let _ = std::fs::remove_file(meta);
                    }
                    sessions.claude_cache.lock().await.take();
                    result
                });
                task.await.map_err(ApiError::internal)??;
            }
            _ => {
                if let Some(prompt) = request.prompt {
                    self.run_turn(&record, prompt)?;
                }
            }
        }
        self.find(&id).await
    }

    /// Only a conversation that was held in `cwd` may be resumed there; otherwise a client
    /// could read (and continue) conversations from folders it has no access to.
    /// `bypass` says whether the request asked for running without permission prompts (which the
    /// workspace allows).
    async fn check_resume(
        &self,
        agent: &Agent,
        cwd: &Path,
        id: &str,
        bypass: bool,
    ) -> Result<(), ApiError> {
        let origin = match agent.kind {
            AgentKind::Claude => external::claude_dir(agent)
                .and_then(|dir| external::claude_conversation_cwd(&dir, cwd, id)),
            AgentKind::Codex => {
                let home = codex_home(agent);
                let thread = id.to_string();
                tokio::task::spawn_blocking(move || external::codex_thread_cwd(&home, &thread))
                    .await
                    .ok()
                    .flatten()
            }
            AgentKind::Hermes => match external::hermes_session(agent, id).await {
                // Hermes resumes the newest session of a compressed conversation, which the
                // checks below would not have seen.
                Some(session) if session.continued => {
                    return Err(ApiError::forbidden(format!(
                        "Hermes conversation `{id}` was compressed and continues in a newer session, which Hermes would resume instead. Resume that session by its id (`hermes sessions list` on the machine shows it)."
                    )));
                }
                // Hermes turns a saved YOLO mode back on when it resumes the conversation.
                Some(session) if session.yolo && !bypass => {
                    return Err(ApiError::forbidden(format!(
                        "Hermes conversation `{id}` was saved in YOLO mode (no approval prompts), and Hermes restores that mode when it is resumed. Resume it with `bypass_permissions: true` in a workspace that allows it, or start a new conversation."
                    )));
                }
                session => session.map(|session| session.cwd),
            },
            AgentKind::Generic => return Ok(()),
        };
        match origin.and_then(|path| dunce::canonicalize(path).ok()) {
            Some(origin) if origin == cwd => Ok(()),
            _ => Err(ApiError::forbidden(format!(
                "Conversation `{id}` was not found in `{}`. Only a conversation held in the folder given as `cwd` can be resumed there.",
                cwd.display()
            ))),
        }
    }

    async fn start_tmux(
        self: &Arc<Self>,
        record: &Arc<Record>,
        mut launch: Launch,
        prompt: Option<String>,
    ) -> Result<(), ApiError> {
        let tmux = self.tmux.as_ref().expect("checked by start");
        if record.events.is_some() {
            crate::tls::create_private_dir(&self.events_dir).map_err(ApiError::internal)?;
        }
        launch.prompt = prompt.clone();
        let mut argv = record.agent.interactive_argv(&record.cwd, &launch)?;
        argv[0] = resolve_program(&argv[0], &record.agent)?;
        if let Some(prompt) = prompt {
            let mut state = record.state();
            state.last_input_ms = Some(unix_now_ms());
            state.sent.push(prompt);
        }
        self.save_meta(record);
        let pane = tmux
            .new_session(&record.id, &record.cwd, &record.agent.env, &argv)
            .await
            .map_err(|err| ApiError::internal(format!("cannot start the tmux session: {err:#}")))?;
        let stopped = {
            let mut state = record.state();
            state.pane = Some(pane.id.clone());
            state.pane_pid = Some(pane.pid);
            state.phase != Phase::Starting
        };
        if stopped {
            return Err(self.stopped_while_starting(record).await);
        }
        // An agent that exits at once (unknown flag, missing login) takes its pane with it.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let alive = tmux.list_panes().await.map(|panes| {
            panes
                .iter()
                .any(|p| p.id == pane.id && p.session == record.id)
        });
        if matches!(alive, Ok(false)) {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "agent_exited",
                format!(
                    "`{}` exited right after starting in {}. Common causes: the agent is not logged in on this machine, or its configured `args` contain an unsupported flag. Retrying will fail the same way; the machine's owner can run it there in a terminal to see the error. Another agent from GET /v1/info may work.",
                    record.agent.command,
                    record.cwd.display()
                ),
            ));
        }
        let stopped = {
            let mut state = record.state();
            let stopped = state.phase != Phase::Starting;
            if !stopped {
                state.phase = Phase::Idle;
            }
            stopped
        };
        if stopped {
            return Err(self.stopped_while_starting(record).await);
        }
        if !record.keep_alive {
            self.watch_until_answered(record.id.clone());
        }
        Ok(())
    }

    /// A DELETE that came while the session was starting found no tmux session to stop; the one
    /// created since must not keep running.
    async fn stopped_while_starting(&self, record: &Record) -> ApiError {
        let Some(tmux) = &self.tmux else {
            return ApiError::internal("tmux is not available");
        };
        if let Err(err) = tmux.kill_session(&record.id).await {
            let gone = tmux
                .list_panes()
                .await
                .is_ok_and(|panes| !panes.iter().any(|p| p.session == record.id));
            if !gone {
                // Still running: list it again, so that it can be stopped once more.
                let mut state = record.state();
                state.phase = Phase::Idle;
                state.ended_at = None;
                return ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "stop_failed",
                    format!(
                        "Session `{}` was stopped while it was starting, but its tmux session could not be closed ({err:#}). It is listed again; stop it with DELETE.",
                        record.id
                    ),
                );
            }
        }
        ApiError::conflict(
            "ended",
            format!("Session `{}` was stopped while it was starting.", record.id),
        )
    }

    /// Starts a headless turn if none is running, checking and switching the state under one
    /// lock so that concurrent requests cannot start two turns.
    fn run_turn(&self, record: &Arc<Record>, prompt: String) -> Result<(), ApiError> {
        // The folder was authorized when the session started; a turn must not run in whatever
        // has replaced it since (a symlink to another folder, or another folder). The turn
        // itself checks again through a handle it keeps.
        let same = std::fs::metadata(&record.cwd)
            .ok()
            .is_some_and(|m| crate::files::identity(&m) == record.cwd_id);
        if !same || dunce::canonicalize(&record.cwd).ok().as_deref() != Some(record.cwd.as_path()) {
            return Err(ApiError::conflict(
                "path_changed",
                format!(
                    "The session's folder {} was moved or replaced since the session started, so no further turn runs there. Start a new session in the folder you want.",
                    record.cwd.display()
                ),
            ));
        }
        let launch = {
            let mut state = record.state();
            match state.phase {
                Phase::Working | Phase::Starting => {
                    return Err(ApiError::conflict(
                        "busy",
                        format!(
                            "A turn is still running. Wait with GET /v1/sessions/{}?wait=60, then send the next message.",
                            record.id
                        ),
                    ));
                }
                Phase::Finished | Phase::Exited => return Err(ended(&record.id)),
                Phase::Idle | Phase::Failed => {}
            }
            let launch = record.launch_for(&state);
            state.phase = Phase::Working;
            state.turns += 1;
            state.error = None;
            state.last_input_ms = Some(unix_now_ms());
            state.last_message = None;
            state.last_message_at = None;
            launch
        };
        headless::spawn_turn(
            record.clone(),
            launch,
            prompt,
            self.config.sessions.max_turn_secs,
        );
        Ok(())
    }

    /// Closes a `keep_alive: false` tmux session once its first answer is in. Only Claude Code
    /// and Codex sessions get here: their turn hooks say when an answer is complete.
    fn watch_until_answered(self: &Arc<Self>, id: String) {
        let sessions = self.clone();
        tokio::spawn(async move {
            let mut seen_working = false;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(record) = sessions.record(&id) else {
                    return;
                };
                if !record.live() {
                    return;
                }
                let Ok(session) = sessions.find(&id).await else {
                    continue;
                };
                match session.state {
                    SessionState::Exited | SessionState::Finished => return,
                    SessionState::Working => seen_working = true,
                    SessionState::Idle => {
                        let answered = {
                            let state = record.state();
                            state
                                .last_turn_ms
                                .zip(state.last_input_ms)
                                .is_some_and(|(turn, input)| turn >= input)
                        };
                        // Without a hook event (hooks disabled), Claude's own idle status after
                        // having been busy is the next best signal.
                        let claude_done = seen_working && record.agent.kind == AgentKind::Claude;
                        if answered || claude_done {
                            match sessions.close_tmux(&record, Phase::Finished).await {
                                Ok(()) => {
                                    tracing::info!(
                                        id,
                                        "session answered and was closed (keep_alive: false)"
                                    );
                                    return;
                                }
                                Err(err) => tracing::warn!(
                                    id,
                                    "closing the session failed: {}",
                                    err.message
                                ),
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    /// Kills a bridge tmux session, keeping its final screen. Fails if it may still be running.
    async fn close_tmux(&self, record: &Record, phase: Phase) -> Result<(), ApiError> {
        if let Some(tmux) = &self.tmux {
            let screen = tmux
                .capture(&record.target(), FINAL_SCREEN_LINES)
                .await
                .ok();
            if let Err(err) = tmux.kill_session(&record.id).await {
                let gone = tmux
                    .list_panes()
                    .await
                    .is_ok_and(|panes| !panes.iter().any(|p| p.session == record.id));
                if !gone {
                    return Err(ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        "stop_failed",
                        format!(
                            "Could not stop tmux session `{}` ({err:#}); it may still be running. Try again.",
                            record.id
                        ),
                    ));
                }
            }
            let mut state = record.state();
            if screen.is_some() {
                state.final_screen = screen;
            }
            state.phase = phase;
            state.ended_at = Some(SystemTime::now());
        }
        Ok(())
    }

    /// Sends text and/or keys. `caller` is the token name, for the audit log.
    pub async fn input(
        self: &Arc<Self>,
        id: &str,
        request: InputRequest,
        caller: &str,
    ) -> Result<Session, ApiError> {
        let text = request
            .text
            .as_deref()
            .map(sanitize)
            .filter(|t| !t.trim().is_empty());
        let keys = request.keys.unwrap_or_default();
        if text.is_none() && keys.is_empty() {
            return Err(ApiError::bad_request(
                "Send non-empty `text`, `keys`, or both.",
            ));
        }
        if text.as_ref().is_some_and(|t| t.len() > MAX_TEXT) {
            return Err(ApiError::too_large(format!(
                "`text` is limited to {MAX_TEXT} bytes."
            )));
        }
        if keys.len() > MAX_KEYS {
            return Err(ApiError::bad_request(format!(
                "At most {MAX_KEYS} keys per request."
            )));
        }
        for key in &keys {
            if !valid_key(key) {
                return Err(ApiError::bad_request(format!(
                    "Key `{key}` is not allowed. Use one of {} or a single printable character.",
                    KEYS.join(", ")
                )));
            }
        }
        let session = self.find(id).await?;
        let ws = self
            .workspace(&session.workspace)
            .ok_or_else(|| ApiError::internal("unknown workspace"))?;
        paths::require(ws, Capability::Input)?;
        if matches!(session.state, SessionState::Finished | SessionState::Exited) {
            return Err(ended(id));
        }
        match session.runner {
            Runner::Tmux if session.native => {
                if !session.controllable {
                    return Err(not_controllable(&session));
                }
                if !keys.is_empty() {
                    return Err(ApiError::bad_request(
                        "This session takes `text` only: it is reached through the agent's own messaging, not its terminal.",
                    ));
                }
                let sent = unix_now_ms();
                self.send_external(&session, &text.clone().unwrap_or_default())
                    .await?;
                lock(&self.pending).insert(session.id.clone(), sent);
            }
            Runner::Headless => {
                if !keys.is_empty() {
                    return Err(ApiError::bad_request(
                        "Headless sessions take `text` only; each message is one turn.",
                    ));
                }
                let record = self
                    .record(id)
                    .ok_or_else(|| ApiError::not_found(format!("No session `{id}`.")))?;
                self.run_turn(&record, text.clone().unwrap_or_default())?;
            }
            Runner::Tmux => {
                let tmux = self
                    .tmux
                    .as_ref()
                    .ok_or_else(|| ApiError::unavailable("tmux is not available."))?;
                let _typing = self.tmux_input.lock().await;
                // Another request may have typed meanwhile, and the pane may have changed: look
                // the session up again now that it is this request's turn.
                let session = self.find(id).await?;
                if matches!(session.state, SessionState::Finished | SessionState::Exited) {
                    return Err(ended(id));
                }
                if session.runner != Runner::Tmux || session.native || !session.controllable {
                    return Err(not_controllable(&session));
                }
                let target = session
                    .target
                    .clone()
                    .ok_or_else(|| ApiError::internal("missing tmux target"))?;
                let pid = session.pane_pid;
                let record = self.record(id);
                if let Some(text) = &text {
                    let submit = request.submit.unwrap_or(true);
                    // A submitted message starts a turn; record it before submitting, so that the
                    // turn's completion event is always later. A draft starts nothing.
                    let previous = match (&record, submit) {
                        (Some(record), true) => {
                            let mut state = record.state();
                            state.sent.push(text.clone());
                            if state.sent.len() > 10 {
                                state.sent.remove(0);
                            }
                            Some(state.last_input_ms.replace(unix_now_ms()))
                        }
                        _ => None,
                    };
                    if let Err(err) = paste_and_submit(tmux, &target, pid, text, submit).await {
                        if let (Some(record), Some(previous)) = (&record, previous) {
                            record.state().last_input_ms = previous;
                        }
                        return Err(err);
                    }
                    if let Some(record) = &record {
                        self.save_meta(record);
                    }
                }
                if !keys.is_empty() {
                    check_pane(tmux, &target, pid).await?;
                }
                for key in &keys {
                    let sent = if KEYS.contains(&key.as_str()) {
                        tmux.send_keys(&target, &[key]).await
                    } else {
                        tmux.send_literal(&target, key).await
                    };
                    sent.map_err(|err| ApiError::internal(format!("{err:#}")))?;
                }
            }
            Runner::External => {
                if !keys.is_empty() {
                    return Err(ApiError::bad_request(
                        "Sessions outside tmux take `text` only.",
                    ));
                }
                if !session.controllable {
                    return Err(not_controllable(&session));
                }
                let text = text.clone().unwrap_or_default();
                let sent = unix_now_ms();
                self.send_external(&session, &text).await?;
                lock(&self.pending).insert(session.id.clone(), sent);
            }
        }
        self.claude_cache.lock().await.take();
        tracing::info!(
            caller,
            id,
            chars = text.as_ref().map_or(0, String::len),
            keys = keys.len(),
            "input sent"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.find(id).await
    }

    async fn send_external(&self, session: &Session, text: &str) -> Result<(), ApiError> {
        let failed = |err: anyhow::Error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "delivery_failed",
                format!("{err:#}"),
            )
        };
        let agent = self
            .config
            .agent(&session.agent)
            .ok_or_else(|| ApiError::internal("unknown agent"))?;
        match session.kind {
            AgentKind::Claude => {
                let inbox = external::claude_dir(agent)
                    .zip(session.pid)
                    .and_then(|(dir, pid)| external::claude_inbox(&dir, pid))
                    .ok_or_else(|| {
                        ApiError::conflict("not_controllable", "The session's inbox is gone.")
                    })?;
                external::claude_send(&inbox, text).await.map_err(failed)
            }
            AgentKind::Codex => {
                let home = session
                    .codex_home
                    .clone()
                    .ok_or_else(|| ApiError::internal("missing CODEX_HOME"))?;
                let thread = session.agent_session_id.clone().unwrap_or_default();
                external::codex_queue(agent, &home, &thread, text)
                    .await
                    .map(drop)
                    .map_err(failed)
            }
            _ => Err(ApiError::conflict(
                "not_controllable",
                "Unsupported external session.",
            )),
        }
    }

    /// Stops a session the bridge started.
    pub async fn stop(self: &Arc<Self>, id: &str, caller: &str) -> Result<Session, ApiError> {
        let session = self.find(id).await?;
        let ws = self
            .workspace(&session.workspace)
            .ok_or_else(|| ApiError::internal("unknown workspace"))?;
        paths::require(ws, Capability::Input)?;
        let Some(record) = self.record(id) else {
            return Err(ApiError::conflict(
                "not_started_by_bridge",
                "Only sessions started by the bridge can be stopped. Ask the agent to exit, or send keys such as [\"C-c\"].",
            ));
        };
        match record.runner {
            Runner::Tmux if record.live() => self.close_tmux(&record, Phase::Exited).await?,
            Runner::Headless => {
                let pid = {
                    let mut state = record.state();
                    if !matches!(state.phase, Phase::Finished | Phase::Exited) {
                        state.phase = Phase::Exited;
                        state.ended_at = Some(SystemTime::now());
                    }
                    state.child_pid.take()
                };
                if let Some(pid) = pid {
                    headless::kill_tree(pid);
                }
            }
            _ => {}
        }
        tracing::info!(caller, id, "session stopped");
        self.get(id, None, None).await
    }

    /// Stops running headless turns and waits for them; tmux sessions keep running.
    pub async fn shutdown(&self) {
        let pids: Vec<u32> = lock(&self.records)
            .values()
            .filter_map(|record| record.state().child_pid.take())
            .collect();
        let tasks: Vec<_> = pids
            .into_iter()
            .map(|pid| tokio::spawn(headless::terminate_tree(pid)))
            .collect();
        for task in tasks {
            let _ = task.await;
        }
    }

    fn save_meta(&self, record: &Record) {
        if record.runner != Runner::Tmux {
            return;
        }
        let meta = {
            let state = record.state();
            RecordMeta {
                keep_alive: record.keep_alive,
                last_input_ms: state.last_input_ms,
                sent: state.sent.clone(),
            }
        };
        let path = self.meta_dir.join(format!("{}.json", record.id));
        let written = crate::tls::create_private_dir(&self.meta_dir)
            .map_err(|err| err.to_string())
            .and_then(|()| serde_json::to_vec(&meta).map_err(|err| err.to_string()))
            .and_then(|bytes| std::fs::write(&path, bytes).map_err(|err| err.to_string()));
        if let Err(err) = written {
            tracing::warn!("cannot save {}: {err}", path.display());
        }
    }

    fn load_meta(&self, id: &str) -> RecordMeta {
        std::fs::read(self.meta_dir.join(format!("{id}.json")))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(RecordMeta {
                keep_alive: true,
                last_input_ms: None,
                sent: Vec::new(),
            })
    }
}

impl Session {
    fn new(id: String, agent: &Agent, runner: Runner, cwd: &Path, workspace: &str) -> Session {
        Session {
            id,
            agent: agent.name.clone(),
            kind: agent.kind,
            runner,
            cwd: cwd.display().to_string(),
            workspace: workspace.to_string(),
            state: SessionState::Unknown,
            waiting_for: None,
            controllable: false,
            started_by_bridge: false,
            keep_alive: None,
            agent_session_id: None,
            started_at: None,
            last_activity_at: None,
            last_message: None,
            last_message_at: None,
            attach_command: None,
            screen: None,
            error: None,
            note: None,
            target: None,
            pid: None,
            codex_home: None,
            native: false,
            pane_pid: None,
        }
    }
}

fn not_controllable(session: &Session) -> ApiError {
    ApiError::conflict(
        "not_controllable",
        session
            .note
            .clone()
            .unwrap_or_else(|| "This session cannot take input through the bridge.".into()),
    )
}

fn ended(id: &str) -> ApiError {
    ApiError::conflict(
        "ended",
        format!(
            "Session `{id}` has ended. Start a new session in the same folder, with `resume` set to its `agent_session_id` to continue the conversation."
        ),
    )
}

/// tmux session name for a new session: readable in `tmux ls`, unique by a random suffix.
pub fn new_session_name(cwd: &Path, agent: &str) -> String {
    let slug: String = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug: String = slug.trim_matches('-').chars().take(20).collect();
    let slug = if slug.is_empty() {
        "root".to_string()
    } else {
        slug
    };
    format!(
        "sgb-{slug}-{agent}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    )
}

/// The absolute path of an agent's program, so tmux does not depend on its own `PATH`.
pub fn resolve_program(program: &str, agent: &Agent) -> Result<String, ApiError> {
    exec::which_with(program, &agent.env)
        .map(|p| p.display().to_string())
        .ok_or_else(|| {
            ApiError::unavailable(format!(
                "`{program}` (agent `{}`) is not installed on this machine's PATH, so it cannot be started; retrying will not help. Only the machine's owner can install it or set an absolute `command` in [agents.{}]. GET /v1/info shows which agents are available.",
                agent.name, agent.name
            ))
        })
}

/// The Codex home of an agent: its `CODEX_HOME`, the bridge's, or `~/.codex`.
fn codex_home(agent: &Agent) -> PathBuf {
    agent
        .env
        .iter()
        .find(|(key, _)| key == "CODEX_HOME")
        .map(|(_, value)| PathBuf::from(value))
        .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

/// A command that continues a conversation in a terminal on this machine, in the folder the
/// conversation belongs to and with the agent's profile. Only the profile variables are shown:
/// other configured variables may hold credentials.
fn resume_command(agent: &Agent, cwd: &Path, id: &str) -> Option<String> {
    let resume = match agent.kind {
        AgentKind::Claude | AgentKind::Hermes => "--resume",
        AgentKind::Codex => "resume",
        AgentKind::Generic => return None,
    };
    let profile: Vec<(String, String)> = agent
        .env
        .iter()
        .filter(|(key, _)| tmux::PROFILE_VARS.contains(&key.as_str()))
        .cloned()
        .collect();
    Some(command_line(
        cwd,
        &profile,
        &agent.command,
        &format!("{resume} {id}"),
    ))
}

/// `cd <cwd> && VAR=value program args` for POSIX shells, or the PowerShell equivalent.
fn command_line(cwd: &Path, env: &[(String, String)], program: &str, args: &str) -> String {
    let cwd = shell_quote(&cwd.display().to_string());
    let program = shell_quote(program);
    if cfg!(windows) {
        let env: String = env
            .iter()
            .map(|(key, value)| format!("$env:{key} = {}; ", shell_quote(value)))
            .collect();
        format!("Set-Location -LiteralPath {cwd}; {env}& {program} {args}")
    } else {
        let env: String = env
            .iter()
            .map(|(key, value)| format!("{key}={} ", shell_quote(value)))
            .collect();
        format!("cd {cwd} && {env}{program} {args}")
    }
}

/// Hook events that belong to the session's main conversation. Codex also reports helper
/// threads (such as title generation); its main thread is the one whose input is a text the
/// bridge sent.
fn main_thread_events<'a>(
    kind: AgentKind,
    events: &'a [TurnEvent],
    state: &mut RecordState,
) -> Vec<&'a TurnEvent> {
    match kind {
        AgentKind::Claude => events.iter().collect(),
        AgentKind::Codex => {
            if state.agent_session_id.is_none() {
                state.agent_session_id = events
                    .iter()
                    .find(|e| {
                        e.input.as_deref().is_some_and(|input| {
                            state.sent.iter().any(|sent| same_input(sent, input))
                        })
                    })
                    .and_then(|e| e.session_id.clone());
            }
            if state.agent_session_id.is_none() {
                // Without bridge input to match (e.g. a session started by hand), skip Codex's
                // title generation, whose answer is a `{"title": …}` object.
                state.agent_session_id = events
                    .iter()
                    .rev()
                    .find(|e| !is_codex_title(e.message.as_deref()))
                    .and_then(|e| e.session_id.clone());
            }
            match &state.agent_session_id {
                Some(id) => events
                    .iter()
                    .filter(|e| e.session_id.as_ref() == Some(id))
                    .collect(),
                None => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Whether a Claude Code conversation began in `cwd`. One that began elsewhere (resumed here, or
/// moved with `/cd`) carries that folder's history, so its transcript is not shown here. A new
/// conversation has no transcript yet, and so no history from anywhere.
fn claude_began_in(claude_dir: &Path, cwd: &Path, session_id: &str) -> bool {
    match external::claude_transcript(claude_dir, cwd, session_id) {
        None => true,
        Some(transcript) => external::transcript_cwd(&transcript)
            .and_then(|origin| dunce::canonicalize(origin).ok())
            .is_some_and(|origin| origin == cwd),
    }
}

/// Whether a hook event's input is a text the bridge sent. The hook keeps only the first 4 KiB of
/// an input and marks the cut.
fn same_input(sent: &str, input: &str) -> bool {
    match input.strip_suffix(hook::TRUNCATED) {
        Some(start) => sent.trim_start().starts_with(start.trim_start()),
        None => sent.trim() == input.trim(),
    }
}

fn is_codex_title(message: Option<&str>) -> bool {
    message
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .and_then(|v| {
            v.as_object()
                .map(|o| o.len() == 1 && o.contains_key("title"))
        })
        .unwrap_or(false)
}

fn claude_state(claude: &ClaudeAgent) -> Option<(SessionState, Option<String>)> {
    match claude.status.as_deref() {
        Some("busy") => Some((SessionState::Working, None)),
        Some("waiting") => Some((SessionState::Waiting, claude.waiting_for.clone())),
        Some("idle") => Some((SessionState::Idle, None)),
        _ => match claude.state.as_deref() {
            Some("working") => Some((SessionState::Working, None)),
            Some("blocked") => Some((SessionState::Waiting, claude.waiting_for.clone())),
            Some("done") => Some((SessionState::Idle, None)),
            Some("failed") => Some((SessionState::Failed, None)),
            Some("stopped") => Some((SessionState::Exited, None)),
            _ => None,
        },
    }
}

struct Derived {
    state: SessionState,
    waiting_for: Option<String>,
    /// Based only on terminal activity.
    estimated: bool,
}

/// State of a tmux session from the best available source: Claude Code's own status, the turn
/// hook (a turn started by bridge input runs until its event arrives), or terminal activity.
fn tmux_state(
    kind: AgentKind,
    pane: &Pane,
    claude: Option<&ClaudeAgent>,
    input_ms: Option<u64>,
    turn_ms: Option<u64>,
    now: u64,
) -> Derived {
    let known = |state, waiting_for| Derived {
        state,
        waiting_for,
        estimated: false,
    };
    if pane.dead {
        return known(SessionState::Exited, None);
    }
    let quiet = now.saturating_sub(pane.activity);
    let pending = matches!(kind, AgentKind::Claude | AgentKind::Codex)
        && input_ms.is_some_and(|input| turn_ms.is_none_or(|turn| turn < input));
    let reported = claude.and_then(claude_state);
    match reported {
        Some((SessionState::Waiting, why)) => return known(SessionState::Waiting, why),
        Some((SessionState::Working, _)) => return known(SessionState::Working, None),
        // Claude Code reports `idle` for a moment after the bridge submits input, so an idle
        // status alone does not end a pending turn; its hook event does.
        Some((state, why)) if !pending => return known(state, why),
        _ => {}
    }
    if pending {
        return match (quiet >= WAITING_SECS, reported.is_some()) {
            (false, _) => known(SessionState::Working, None),
            // Claude says idle and the terminal is quiet: the turn ended without a hook event.
            (true, true) => known(SessionState::Idle, None),
            (true, false) => known(
                SessionState::Waiting,
                Some("input or approval (no output for a while)".into()),
            ),
        };
    }
    // After a turn the hook reported, a quiet terminal means idle; a busy one is a guess.
    let reported_turn = matches!(kind, AgentKind::Claude | AgentKind::Codex) && turn_ms.is_some();
    Derived {
        state: if quiet < ACTIVE_SECS {
            SessionState::Working
        } else {
            SessionState::Idle
        },
        waiting_for: None,
        estimated: !(reported_turn && quiet >= ACTIVE_SECS),
    }
}

fn valid_key(key: &str) -> bool {
    KEYS.contains(&key) || (key.chars().count() == 1 && key.chars().all(|c| c.is_ascii_graphic()))
}

/// Removes control characters (except tab and newline) and normalizes line endings, so pasted
/// text cannot carry terminal escape sequences, e.g. one that ends a bracketed paste early.
pub fn sanitize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// Fails unless the pane still runs `pid`, the process it was authorized with. Checked right
/// before typing, because a pane can be respawned with another program, such as a shell, at any
/// time.
async fn check_pane(tmux: &Tmux, target: &str, pid: Option<u32>) -> Result<Pane, ApiError> {
    let panes = tmux
        .list_panes()
        .await
        .map_err(|err| ApiError::internal(format!("{err:#}")))?;
    let pane = panes
        .iter()
        .find(|p| p.id == target || tmux::session_target(&p.session) == target)
        .cloned()
        .ok_or_else(|| ApiError::conflict("ended", "The session's tmux pane is gone."))?;
    let crowded = cfg!(windows) && panes.iter().filter(|p| p.session == pane.session).count() > 1;
    if pid.is_some_and(|pid| pid != pane.pid) || pane.dead || crowded {
        return Err(ApiError::conflict(
            "not_controllable",
            "The session's pane no longer runs the agent it was started with, so the bridge does not type into it.",
        ));
    }
    Ok(pane)
}

async fn paste_and_submit(
    tmux: &Tmux,
    target: &str,
    pid: Option<u32>,
    text: &str,
    submit: bool,
) -> Result<(), ApiError> {
    let fail = |err: anyhow::Error| ApiError::internal(format!("{err:#}"));
    if check_pane(tmux, target, pid).await?.in_mode {
        tmux.cancel_mode(target).await.map_err(fail)?;
    }
    let before = tmux.capture(target, 40).await.map_err(fail)?;
    tmux.paste(target, text).await.map_err(fail)?;
    if !submit {
        return Ok(());
    }
    // Press Enter only once the paste has been rendered; an Enter that arrives while a TUI is
    // still processing the paste can be swallowed.
    let pasted = wait_for_change(tmux, target, &before, 2000).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    check_pane(tmux, target, pid).await?;
    tmux.send_keys(target, &["Enter"]).await.map_err(fail)?;
    // If the screen did not react, the Enter was most likely swallowed: press it once more.
    if wait_for_change(tmux, target, &pasted, 1500).await != pasted {
        return Ok(());
    }
    check_pane(tmux, target, pid).await?;
    tmux.send_keys(target, &["Enter"]).await.map_err(fail)
}

/// Polls the pane until its content differs from `before` (or the time is up); returns the
/// latest content.
async fn wait_for_change(tmux: &Tmux, target: &str, before: &str, max_ms: u64) -> String {
    let mut current = before.to_string();
    for _ in 0..max_ms / 100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(screen) = tmux.capture(target, 40).await {
            current = screen;
            if current != before {
                break;
            }
        }
    }
    current
}

fn unix_time(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(activity: u64) -> Pane {
        Pane {
            id: "%1".into(),
            pid: 1,
            session: "s".into(),
            created: 0,
            activity,
            in_mode: false,
            dead: false,
            command: "x".into(),
            cwd: PathBuf::from("/"),
        }
    }

    #[test]
    fn tmux_state_prefers_claude_status_then_hooks_then_activity() {
        let now = 1_000;
        let waiting = ClaudeAgent {
            pid: Some(1),
            cwd: PathBuf::from("/"),
            kind: "interactive".into(),
            session_id: None,
            name: None,
            status: Some("waiting".into()),
            state: None,
            waiting_for: Some("permission prompt".into()),
            started_at: None,
            agent: "claude".into(),
        };
        let derived = tmux_state(
            AgentKind::Claude,
            &pane(now),
            Some(&waiting),
            None,
            None,
            now,
        );
        assert_eq!(
            (
                derived.state,
                derived.waiting_for.as_deref(),
                derived.estimated
            ),
            (SessionState::Waiting, Some("permission prompt"), false)
        );
        // A lagging `idle` status does not end a turn the bridge just started …
        let idle = ClaudeAgent {
            status: Some("idle".into()),
            waiting_for: None,
            ..waiting.clone()
        };
        let state = |kind, activity, claude, input, turn| {
            tmux_state(kind, &pane(activity), claude, input, turn, now).state
        };
        assert_eq!(
            state(AgentKind::Claude, now, Some(&idle), Some(999), None),
            SessionState::Working
        );
        // … but a long quiet period with an idle status does, and so does the hook event.
        assert_eq!(
            state(AgentKind::Claude, now - 20, Some(&idle), Some(970), None),
            SessionState::Idle
        );
        assert_eq!(
            state(AgentKind::Claude, now, Some(&idle), Some(990), Some(995)),
            SessionState::Idle
        );
        // Codex: input sent, no turn event yet.
        assert_eq!(
            state(AgentKind::Codex, now, None, Some(990), None),
            SessionState::Working
        );
        assert_eq!(
            state(AgentKind::Codex, now - 30, None, Some(960), None),
            SessionState::Waiting
        );
        let answered = tmux_state(
            AgentKind::Codex,
            &pane(now - 30),
            None,
            Some(960),
            Some(965),
            now,
        );
        assert_eq!(
            (answered.state, answered.estimated),
            (SessionState::Idle, false)
        );
        // Untracked agents: terminal activity, marked as an estimate.
        assert_eq!(
            state(AgentKind::Hermes, now - 1, None, None, None),
            SessionState::Working
        );
        let quiet = tmux_state(AgentKind::Hermes, &pane(now - 10), None, None, None, now);
        assert_eq!((quiet.state, quiet.estimated), (SessionState::Idle, true));
    }

    #[test]
    fn codex_main_thread_is_the_one_answering_bridge_input() {
        let events = vec![
            TurnEvent {
                at_ms: 1,
                session_id: Some("title".into()),
                message: Some("{\"title\":\"x\"}".into()),
                input: Some("Generate a title … User prompt:\nfix it".into()),
            },
            TurnEvent {
                at_ms: 2,
                session_id: Some("main".into()),
                message: Some("Fixed.".into()),
                input: Some("fix it".into()),
            },
        ];
        let mut state = RecordState {
            sent: vec!["fix it\n".into()],
            ..RecordState::default()
        };
        let main = main_thread_events(AgentKind::Codex, &events, &mut state);
        assert_eq!(main.len(), 1);
        assert_eq!(state.agent_session_id.as_deref(), Some("main"));
        // Without sent texts (adopted after a restart) the title thread is skipped too.
        let reordered = vec![events[1].clone(), events[0].clone()];
        let mut adopted = RecordState::default();
        main_thread_events(AgentKind::Codex, &reordered, &mut adopted);
        assert_eq!(adopted.agent_session_id.as_deref(), Some("main"));
    }

    #[test]
    fn claude_transcripts_are_shown_only_where_their_conversation_began() {
        let claude = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let here = dunce::canonicalize(work.path()).unwrap();
        let elsewhere = here.join("private");
        std::fs::create_dir(&elsewhere).unwrap();
        // A new session has no transcript yet.
        assert!(claude_began_in(claude.path(), &here, "new"));
        let project = claude
            .path()
            .join("projects")
            .join(external::claude_project_dir(&here));
        std::fs::create_dir_all(&project).unwrap();
        let transcript = |id: &str, cwd: &Path| {
            let line = serde_json::json!({"type": "user", "cwd": cwd, "sessionId": id});
            std::fs::write(project.join(format!("{id}.jsonl")), line.to_string()).unwrap();
        };
        transcript("local", &here);
        transcript("moved", &elsewhere);
        assert!(claude_began_in(claude.path(), &here, "local"));
        assert!(!claude_began_in(claude.path(), &here, "moved"));
    }

    #[test]
    fn hook_inputs_cut_at_4_kib_still_match_what_was_sent() {
        let long = "x".repeat(10_000);
        let cut = format!("{}{}", &long[..4096], hook::TRUNCATED);
        assert!(same_input(&long, &cut));
        assert!(same_input("fix it\n", "fix it"));
        assert!(!same_input("fix it", "fix that"));
        assert!(!same_input("short", &format!("other{}", hook::TRUNCATED)));
    }

    #[test]
    fn keys_and_text_are_checked() {
        assert!(valid_key("Enter") && valid_key("1") && valid_key("y") && valid_key("C-c"));
        assert!(
            !valid_key("C-x C-c")
                && !valid_key("Enter;kill-server")
                && !valid_key(" ")
                && !valid_key("")
        );
        assert_eq!(sanitize("a\r\nb\x1b[201~c\td"), "a\nb[201~c\td");
        let name = new_session_name(Path::new("/home/me/My Project.v2"), "claude");
        assert!(
            name.starts_with("sgb-my-project-v2-claude-")
                && name.len() == "sgb-my-project-v2-claude-".len() + 6,
            "{name}"
        );
    }
}
