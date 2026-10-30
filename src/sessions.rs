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
use crate::exec;
use crate::external::{self, ClaudeAgent};
use crate::files::rfc3339;
use crate::headless;
use crate::hook::{self, TurnEvent, unix_now};
use crate::paths;
use crate::procs::Snapshot;
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
/// Agents without turn tracking are considered done after this much silence.
const HEURISTIC_DONE_SECS: u64 = 20;
const CLAUDE_CACHE: Duration = Duration::from_secs(2);
/// How long finished, exited and stopped sessions stay listed.
const RETENTION: Duration = Duration::from_secs(3600);

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
    /// The agent's last completed answer; while a headless turn runs, its latest message.
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
}

/// Sessions in the workspaces this token may see.
#[derive(Serialize, ToSchema, Debug)]
pub struct SessionList {
    pub sessions: Vec<Session>,
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
    /// First message to the agent.
    pub prompt: Option<String>,
    /// Model, passed with the agent's model flag (e.g. `opus`, `gpt-6.1-sol`).
    pub model: Option<String>,
    pub effort: Option<Effort>,
    /// Run without permission prompts (e.g. `--dangerously-skip-permissions`). Needs
    /// `allow_bypass_permissions` on the workspace.
    #[serde(default)]
    pub bypass_permissions: bool,
    /// `true` (default) keeps the session after it answers. `false` closes it once the first
    /// answer is in; the answer stays readable in `last_message`.
    pub keep_alive: Option<bool>,
    /// Continue an earlier conversation: the agent's session id (`agent_session_id`).
    pub resume: Option<String>,
}

/// Input for a session: text, keys, or both (text first).
#[derive(Deserialize, ToSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct InputRequest {
    /// Message to the agent. In tmux it is pasted as one block and submitted with Enter.
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
    pub agent_session_id: Option<String>,
    pub last_message: Option<String>,
    pub last_message_at: Option<SystemTime>,
    /// Unix time of the last input the bridge sent.
    pub last_input_at: Option<u64>,
    /// Unix time of the last completed turn reported by the hook.
    pub last_turn_at: Option<u64>,
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
    fn next_launch(&self) -> Launch {
        let state = self.state();
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
}

// ---------------------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------------------

pub struct Sessions {
    config: Arc<Config>,
    pub tmux: Option<Tmux>,
    pub tmux_version: Option<String>,
    events_dir: PathBuf,
    exe: PathBuf,
    records: Mutex<HashMap<String, Arc<Record>>>,
    claude_cache: tokio::sync::Mutex<Option<(Instant, Arc<Vec<ClaudeAgent>>)>>,
}

/// What a listing is built from.
struct Context {
    panes: Vec<Pane>,
    snapshot: Snapshot,
    claude: Arc<Vec<ClaudeAgent>>,
    now: u64,
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
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("safe-grokbot-bridge")),
            records: Mutex::new(HashMap::new()),
            claude_cache: tokio::sync::Mutex::new(None),
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

    async fn claude_agents(&self) -> Arc<Vec<ClaudeAgent>> {
        let mut cache = self.claude_cache.lock().await;
        if let Some((at, list)) = cache.as_ref()
            && at.elapsed() < CLAUDE_CACHE
        {
            return list.clone();
        }
        let agent = self
            .config
            .agents
            .iter()
            .find(|a| a.kind == AgentKind::Claude && exec::which(&a.command).is_some());
        let list = Arc::new(match agent {
            Some(agent) => external::claude_agents(agent).await,
            None => Vec::new(),
        });
        *cache = Some((Instant::now(), list.clone()));
        list
    }

    async fn context(&self) -> Context {
        let panes = match &self.tmux {
            Some(tmux) => tmux.list_panes().await.unwrap_or_else(|err| {
                tracing::warn!("cannot list tmux panes: {err:#}");
                Vec::new()
            }),
            None => Vec::new(),
        };
        let with_environ = self.config.experimental.external_sessions;
        let snapshot = tokio::task::spawn_blocking(move || Snapshot::take(with_environ))
            .await
            .unwrap_or_default();
        let claude = self.claude_agents().await;
        Context {
            panes,
            snapshot,
            claude,
            now: unix_now(),
        }
    }

    /// Drops ended sessions after the retention period.
    fn cleanup(&self) {
        let mut records = lock(&self.records);
        records.retain(|_, record| {
            let expired = record
                .state()
                .ended_at
                .is_some_and(|at| at.elapsed().unwrap_or_default() > RETENTION);
            if expired && let Some(events) = &record.events {
                let _ = std::fs::remove_file(events);
            }
            !expired
        });
    }

    /// All sessions in workspaces with the `sessions` capability, optionally only those whose
    /// working directory is under `under`.
    pub async fn list(&self, under: Option<&Path>) -> Vec<Session> {
        self.cleanup();
        let context = self.context().await;
        let mut sessions = self.collect(&context);
        if let Some(under) = under {
            sessions.retain(|s| Path::new(&s.cwd).starts_with(under));
        }
        sessions
    }

    fn collect(&self, ctx: &Context) -> Vec<Session> {
        let records: Vec<Arc<Record>> = lock(&self.records).values().cloned().collect();
        let mut sessions = Vec::new();
        let mut in_tmux: HashSet<u32> = HashSet::new();
        let mut live: HashSet<&str> = HashSet::new();
        for pane in &ctx.panes {
            in_tmux.extend(ctx.snapshot.tree(pane.pid, 8).iter().map(|p| p.pid));
            in_tmux.insert(pane.pid);
            let record = records
                .iter()
                .find(|r| r.runner == Runner::Tmux && r.id == pane.session);
            if let Some(record) = record {
                if !record.live() {
                    continue;
                }
                live.insert(record.id.as_str());
            }
            sessions.extend(self.tmux_view(ctx, pane, record));
        }
        for record in &records {
            if record.runner == Runner::Tmux && !live.contains(record.id.as_str()) {
                let mut state = record.state();
                if state.phase == Phase::Idle {
                    // The tmux session disappeared (the agent exited or someone killed it).
                    state.phase = Phase::Exited;
                    state.ended_at = Some(SystemTime::now());
                }
            }
            if record.runner == Runner::Headless || !live.contains(record.id.as_str()) {
                sessions.extend(self.record_view(record));
            }
        }
        sessions.extend(self.claude_views(ctx, &in_tmux));
        if self.config.experimental.external_sessions {
            sessions.extend(self.codex_views(ctx, &in_tmux));
        }
        sessions
    }

    fn tmux_view(
        &self,
        ctx: &Context,
        pane: &Pane,
        record: Option<&Arc<Record>>,
    ) -> Option<Session> {
        let (agent, cwd, workspace) = match record {
            Some(record) => (
                record.agent.clone(),
                record.cwd.clone(),
                record.workspace.clone(),
            ),
            None => {
                let cwd = dunce::canonicalize(&pane.cwd).ok()?;
                let workspace = paths::workspace_for(&self.config, &cwd)?.name.clone();
                let (agent, _) = ctx.snapshot.find_agent(pane.pid, &self.config.agents)?;
                (agent.clone(), cwd, workspace)
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

        let mut session = Session::new(
            record
                .map(|r| r.id.clone())
                .unwrap_or_else(|| format!("tmux-{}", pane.id.trim_start_matches('%'))),
            &agent,
            Runner::Tmux,
            &cwd,
            &workspace,
        );
        session.controllable = ws.allows(Capability::Input) && !pane.dead;
        session.last_activity_at = (pane.activity > 0).then(|| rfc3339(unix_time(pane.activity)));
        session.attach_command = self.tmux.as_ref().map(|t| t.attach_command(&pane.session));
        session.target = Some(match record {
            Some(record) => tmux::session_target(&record.id),
            None => pane.id.clone(),
        });
        session.agent_session_id = claude.and_then(|c| c.session_id.clone());

        let (input_at, turn_at) = match record {
            Some(record) => {
                let events = record
                    .events
                    .as_deref()
                    .map(hook::read_events)
                    .unwrap_or_default();
                let mut state = record.state();
                let turns = main_thread_events(agent.kind, &events, &mut state);
                if let Some(last) = turns.last() {
                    state.last_turn_at = Some(last.at);
                    if last.message.is_some() {
                        state.last_message = last.message.clone();
                        state.last_message_at = Some(unix_time(last.at));
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
                (state.last_input_at, state.last_turn_at)
            }
            None => (None, None),
        };
        let (state, waiting_for) = tmux_state(agent.kind, pane, claude, input_at, turn_at, ctx.now);
        session.state = state;
        session.waiting_for = waiting_for;
        if state == SessionState::Waiting {
            session.note = Some("The agent is waiting for an answer. Read `screen`, then reply with `text` or `keys` (e.g. [\"Enter\"] or [\"1\"]).".into());
        }
        Some(session)
    }

    fn record_view(&self, record: &Record) -> Option<Session> {
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
            Phase::Idle => SessionState::Idle,
            Phase::Working => SessionState::Working,
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
            .and_then(|id| resume_command(&record.agent, id));
        if state.phase == Phase::Finished {
            session.note = Some("Finished (keep_alive: false). To continue this conversation, start a new session with `resume` set to `agent_session_id`.".into());
        }
        Some(session)
    }

    fn claude_views(&self, ctx: &Context, in_tmux: &HashSet<u32>) -> Vec<Session> {
        let experimental = self.config.experimental.external_sessions;
        let agent = self
            .config
            .agents
            .iter()
            .find(|a| a.kind == AgentKind::Claude);
        let (Some(agent), claude_dir) = (agent, agent.and_then(external::claude_dir)) else {
            return Vec::new();
        };
        let mut sessions = Vec::new();
        for claude in ctx.claude.iter() {
            if claude.pid.is_some_and(|pid| in_tmux.contains(&pid)) {
                continue;
            }
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
            session.attach_command = Some(if claude.kind == "background" {
                format!("claude attach {}", &session_id[..session_id.len().min(8)])
            } else {
                format!(
                    "cd {} && claude --resume {session_id}  (after closing the running session)",
                    cwd.display()
                )
            });
            let inbox = claude
                .pid
                .zip(claude_dir.as_deref())
                .and_then(|(pid, dir)| external::claude_inbox(dir, pid));
            if !experimental {
                session.note = Some("Runs outside tmux, so the bridge can only watch it. Enable [experimental] external_sessions to message it, or start a new session through the bridge.".into());
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
        let Some(agent) = self
            .config
            .agents
            .iter()
            .find(|a| a.kind == AgentKind::Codex)
        else {
            return Vec::new();
        };
        let mut sessions = Vec::new();
        for thread in external::codex_threads(&ctx.snapshot, &agent.command) {
            if in_tmux.contains(&thread.pid) {
                continue;
            }
            let Some(ws) = paths::workspace_for(&self.config, &thread.cwd) else {
                continue;
            };
            if !ws.allows(Capability::Sessions) {
                continue;
            }
            let mut session = Session::new(
                format!("codex-{}", thread.id),
                agent,
                Runner::External,
                &thread.cwd,
                &ws.name,
            );
            let progress = external::codex_progress(&thread.rollout);
            session.state = match &progress {
                Some(p) if p.working => SessionState::Working,
                Some(_) => SessionState::Idle,
                None => SessionState::Unknown,
            };
            if let Some(progress) = progress {
                session.last_message = progress.last_message;
                session.last_message_at = progress.last_message_at;
            }
            session.agent_session_id = Some(thread.id.clone());
            session.controllable = ws.allows(Capability::Input);
            session.pid = Some(thread.pid);
            session.codex_home = Some(thread.home.clone());
            session.attach_command = Some(format!(
                "CODEX_HOME={} codex resume {}",
                thread.home.display(),
                thread.id
            ));
            session.note = Some("Experimental: text is queued with `codex queue` and delivered when the thread is idle; it never interrupts a running turn.".into());
            sessions.push(session);
        }
        sessions
    }

    async fn find(&self, id: &str) -> Result<Session, ApiError> {
        self.list(None).await.into_iter().find(|s| s.id == id).ok_or_else(|| {
            ApiError::not_found(format!(
                "No session `{id}` in the workspaces you may see. List sessions with GET /v1/sessions."
            ))
        })
    }

    /// One session with its terminal output; with `wait`, first waits (up to 90 s) while the
    /// session is working.
    pub async fn get(
        &self,
        id: &str,
        lines: Option<usize>,
        wait: Option<u64>,
    ) -> Result<Session, ApiError> {
        let deadline = Instant::now() + Duration::from_secs(wait.unwrap_or(0).min(MAX_WAIT_SECS));
        loop {
            let session = self.find(id).await?;
            if session.state != SessionState::Working || Instant::now() >= deadline {
                return Ok(self
                    .with_screen(session, lines.unwrap_or(DEFAULT_LINES).clamp(1, MAX_LINES))
                    .await);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
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
        if let Some(resume) = &request.resume {
            agents::validate_session_ref(resume)?;
        }
        let runner = match request.runner.unwrap_or(self.config.sessions.runner) {
            RunnerChoice::Tmux => Runner::Tmux,
            RunnerChoice::Headless => Runner::Headless,
            RunnerChoice::Auto => self.default_runner(),
        };
        if runner == Runner::Tmux && self.tmux.is_none() {
            return Err(ApiError::unavailable(format!(
                "tmux (`{}`) is not available on this machine. Use `runner: \"headless\"`.",
                self.config.sessions.tmux
            )));
        }
        let keep_alive = request.keep_alive.unwrap_or(true);
        if agent.kind == AgentKind::Generic && (runner == Runner::Headless || !keep_alive) {
            return Err(ApiError::bad_request(format!(
                "Agent `{}` is a generic terminal program: it runs only in tmux and with keep_alive true.",
                agent.name
            )));
        }
        let active = lock(&self.records).values().filter(|r| r.live()).count();
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
        if let Some(resume) = &request.resume
            && agent.kind == AgentKind::Claude
            && let Some(open) = self
                .claude_agents()
                .await
                .iter()
                .find(|c| c.session_id.as_deref() == Some(resume) && c.pid.is_some())
        {
            return Err(ApiError::conflict(
                "session_open",
                format!(
                    "Claude session `{resume}` is open in another process (pid {}). Send it input instead of resuming it twice.",
                    open.pid.unwrap_or_default()
                ),
            ));
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
            workspace: ws.name.clone(),
            keep_alive,
            launch: launch.clone(),
            started_at: SystemTime::now(),
            events,
            state: Mutex::new(RecordState {
                agent_session_id: claude_session_id.or(request.resume.clone()),
                ..RecordState::default()
            }),
        });
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
        self.claude_cache.lock().await.take();
        match runner {
            Runner::Tmux => self.start_tmux(record, launch, request.prompt).await?,
            _ => {
                lock(&self.records).insert(id.clone(), record.clone());
                if let Some(prompt) = request.prompt {
                    self.run_turn(&record, prompt);
                }
            }
        }
        self.find(&id).await
    }

    async fn start_tmux(
        self: &Arc<Self>,
        record: Arc<Record>,
        mut launch: Launch,
        prompt: Option<String>,
    ) -> Result<(), ApiError> {
        let tmux = self.tmux.as_ref().expect("checked by start");
        if record.events.is_some() {
            crate::tls::create_private_dir(&self.events_dir).map_err(ApiError::internal)?;
        }
        launch.prompt = prompt.clone();
        let mut argv = record.agent.interactive_argv(&launch)?;
        argv[0] = resolve_program(&argv[0], &record.agent)?;
        {
            let mut state = record.state();
            if let Some(prompt) = prompt {
                state.last_input_at = Some(unix_now());
                state.sent.push(prompt);
            }
        }
        lock(&self.records).insert(record.id.clone(), record.clone());
        if let Err(err) = tmux
            .new_session(&record.id, &record.cwd, &record.agent.env, &argv)
            .await
        {
            lock(&self.records).remove(&record.id);
            return Err(ApiError::internal(format!(
                "cannot start the tmux session: {err:#}"
            )));
        }
        // An agent that exits at once (unknown flag, missing login) takes its pane with it.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let alive = tmux
            .list_panes()
            .await
            .map(|panes| panes.iter().any(|p| p.session == record.id));
        if matches!(alive, Ok(false)) {
            lock(&self.records).remove(&record.id);
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "agent_exited",
                format!(
                    "`{}` exited right after starting in {}. Run it there in a terminal to see why (e.g. it is not logged in, or the agent's `args` contain an unsupported flag).",
                    record.agent.command,
                    record.cwd.display()
                ),
            ));
        }
        if !record.keep_alive {
            self.watch_until_answered(record.id.clone());
        }
        Ok(())
    }

    fn run_turn(&self, record: &Arc<Record>, prompt: String) {
        let launch = record.next_launch();
        {
            let mut state = record.state();
            state.turns += 1;
        }
        headless::spawn_turn(
            record.clone(),
            launch,
            prompt,
            self.config.sessions.max_turn_secs,
        );
    }

    /// Closes a `keep_alive: false` tmux session once its first answer is in.
    fn watch_until_answered(self: &Arc<Self>, id: String) {
        let sessions = self.clone();
        tokio::spawn(async move {
            let mut seen_working = false;
            let mut quiet_since: Option<Instant> = None;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(record) = sessions.record(&id) else {
                    return;
                };
                if !record.live() {
                    return;
                }
                let Ok(session) = sessions.find(&id).await else {
                    return;
                };
                let done = match session.state {
                    SessionState::Exited => return,
                    SessionState::Working => {
                        seen_working = true;
                        quiet_since = None;
                        false
                    }
                    SessionState::Waiting => false,
                    _ => {
                        let state = record.state();
                        let tracked = state
                            .last_turn_at
                            .zip(state.last_input_at)
                            .is_some_and(|(turn, input)| turn >= input);
                        drop(state);
                        match record.agent.kind {
                            AgentKind::Claude | AgentKind::Codex => {
                                tracked || (seen_working && record.agent.kind == AgentKind::Claude)
                            }
                            _ => {
                                let since = *quiet_since.get_or_insert_with(Instant::now);
                                seen_working
                                    && since.elapsed() >= Duration::from_secs(HEURISTIC_DONE_SECS)
                            }
                        }
                    }
                };
                if done {
                    sessions.close_tmux(&record, Phase::Finished).await;
                    tracing::info!(id, "session answered and was closed (keep_alive: false)");
                    return;
                }
            }
        });
    }

    async fn close_tmux(&self, record: &Record, phase: Phase) {
        if let Some(tmux) = &self.tmux {
            let screen = tmux
                .capture(&tmux::session_target(&record.id), FINAL_SCREEN_LINES)
                .await
                .ok();
            let _ = tmux.kill_session(&record.id).await;
            let mut state = record.state();
            if screen.is_some() {
                state.final_screen = screen;
            }
            state.phase = phase;
            state.ended_at = Some(SystemTime::now());
        }
    }

    /// Sends text and/or keys. `caller` is the token name, for the audit log.
    pub async fn input(
        &self,
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
            return Err(ApiError::conflict(
                "ended",
                format!(
                    "Session `{id}` has ended. Start a new session (with `resume` set to its `agent_session_id` to continue the conversation)."
                ),
            ));
        }
        match session.runner {
            Runner::Headless => {
                if !keys.is_empty() {
                    return Err(ApiError::bad_request(
                        "Headless sessions take `text` only; each message is one turn.",
                    ));
                }
                let record = self
                    .record(id)
                    .ok_or_else(|| ApiError::not_found(format!("No session `{id}`.")))?;
                if record.state().phase == Phase::Working {
                    return Err(ApiError::conflict(
                        "busy",
                        format!(
                            "A turn is still running. Wait with GET /v1/sessions/{id}?wait=60, then send the next message."
                        ),
                    ));
                }
                self.run_turn(&record, text.clone().unwrap_or_default());
            }
            Runner::Tmux => {
                let tmux = self
                    .tmux
                    .as_ref()
                    .ok_or_else(|| ApiError::unavailable("tmux is not available."))?;
                let target = session
                    .target
                    .clone()
                    .ok_or_else(|| ApiError::internal("missing tmux target"))?;
                if let Some(text) = &text {
                    paste_and_submit(tmux, &target, text, request.submit.unwrap_or(true)).await?;
                    if let Some(record) = self.record(id) {
                        let mut state = record.state();
                        state.last_input_at = Some(unix_now());
                        state.sent.push(text.clone());
                        if state.sent.len() > 10 {
                            state.sent.remove(0);
                        }
                    }
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
                    return Err(ApiError::conflict(
                        "not_controllable",
                        session.note.clone().unwrap_or_else(|| {
                            "This session cannot take input through the bridge.".into()
                        }),
                    ));
                }
                let text = text.clone().unwrap_or_default();
                self.send_external(&session, &text).await?;
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
        match session.kind {
            AgentKind::Claude => {
                let agent = self
                    .config
                    .agents
                    .iter()
                    .find(|a| a.kind == AgentKind::Claude)
                    .expect("claude agent");
                let inbox = external::claude_dir(agent)
                    .zip(session.pid)
                    .and_then(|(dir, pid)| external::claude_inbox(&dir, pid))
                    .ok_or_else(|| {
                        ApiError::conflict("not_controllable", "The session's inbox is gone.")
                    })?;
                external::claude_send(&inbox, text).await.map_err(failed)
            }
            AgentKind::Codex => {
                let agent = self
                    .config
                    .agents
                    .iter()
                    .find(|a| a.kind == AgentKind::Codex)
                    .expect("codex agent");
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
    pub async fn stop(&self, id: &str, caller: &str) -> Result<Session, ApiError> {
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
            Runner::Tmux if record.live() => self.close_tmux(&record, Phase::Exited).await,
            Runner::Headless => {
                let mut state = record.state();
                if let Some(pid) = state.child_pid.take() {
                    headless::kill_tree(pid);
                }
                if !matches!(state.phase, Phase::Finished | Phase::Exited) {
                    state.phase = Phase::Exited;
                    state.ended_at = Some(SystemTime::now());
                }
            }
            _ => {}
        }
        tracing::info!(caller, id, "session stopped");
        self.get(id, None, None).await
    }

    /// Kills running headless turns; tmux sessions keep running.
    pub fn shutdown(&self) {
        for record in lock(&self.records).values() {
            if let Some(pid) = record.state().child_pid.take() {
                headless::kill_tree(pid);
            }
        }
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
        }
    }
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
    exec::which(program).map(|p| p.display().to_string()).ok_or_else(|| {
        ApiError::unavailable(format!(
            "`{program}` (agent `{}`) was not found on the bridge's PATH. Install it, or set an absolute `command` in [agents.{}].",
            agent.name, agent.name
        ))
    })
}

fn resume_command(agent: &Agent, id: &str) -> Option<String> {
    match agent.kind {
        AgentKind::Claude => Some(format!("claude --resume {id}")),
        AgentKind::Codex => Some(format!("codex resume {id}")),
        AgentKind::Hermes => Some(format!("hermes --resume {id}")),
        AgentKind::Generic => None,
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
                            state.sent.iter().any(|sent| sent.trim() == input.trim())
                        })
                    })
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

/// State of a tmux session from the best available source: Claude Code's own status, the turn
/// hook (a turn started by bridge input runs until its event arrives), or terminal activity.
fn tmux_state(
    kind: AgentKind,
    pane: &Pane,
    claude: Option<&ClaudeAgent>,
    input_at: Option<u64>,
    turn_at: Option<u64>,
    now: u64,
) -> (SessionState, Option<String>) {
    if pane.dead {
        return (SessionState::Exited, None);
    }
    let quiet = now.saturating_sub(pane.activity);
    let pending = matches!(kind, AgentKind::Claude | AgentKind::Codex)
        && input_at.is_some_and(|input| turn_at.is_none_or(|turn| turn < input));
    let reported = claude.and_then(claude_state);
    match reported {
        Some((SessionState::Waiting, why)) => return (SessionState::Waiting, why),
        Some((SessionState::Working, _)) => return (SessionState::Working, None),
        // Claude Code reports `idle` for a moment after the bridge submits input, so an idle
        // status alone does not end a pending turn; its hook event does.
        Some(state) if !pending => return state,
        _ => {}
    }
    if pending {
        return match (quiet >= WAITING_SECS, reported.is_some()) {
            (false, _) => (SessionState::Working, None),
            // Claude says idle and the terminal is quiet: the turn ended without a hook event.
            (true, true) => (SessionState::Idle, None),
            (true, false) => (
                SessionState::Waiting,
                Some("input or approval (no output for a while)".into()),
            ),
        };
    }
    if quiet < ACTIVE_SECS {
        (SessionState::Working, None)
    } else {
        (SessionState::Idle, None)
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

async fn paste_and_submit(
    tmux: &Tmux,
    target: &str,
    text: &str,
    submit: bool,
) -> Result<(), ApiError> {
    let fail = |err: anyhow::Error| ApiError::internal(format!("{err:#}"));
    let in_mode = tmux
        .list_panes()
        .await
        .map_err(fail)?
        .iter()
        .any(|p| p.in_mode && (p.id == target || tmux::session_target(&p.session) == target));
    if in_mode {
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
    tmux.send_keys(target, &["Enter"]).await.map_err(fail)?;
    // If the screen did not react, the Enter was most likely swallowed: press it once more.
    if wait_for_change(tmux, target, &pasted, 1500).await != pasted {
        return Ok(());
    }
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
        };
        let (state, why) = tmux_state(
            AgentKind::Claude,
            &pane(now),
            Some(&waiting),
            None,
            None,
            now,
        );
        assert_eq!(
            (state, why.as_deref()),
            (SessionState::Waiting, Some("permission prompt"))
        );

        // Codex: input sent at 990, no turn event yet.
        assert_eq!(
            tmux_state(AgentKind::Codex, &pane(now), None, Some(990), None, now).0,
            SessionState::Working
        );
        assert_eq!(
            tmux_state(
                AgentKind::Codex,
                &pane(now - 30),
                None,
                Some(960),
                None,
                now
            )
            .0,
            SessionState::Waiting
        );
        assert_eq!(
            tmux_state(
                AgentKind::Codex,
                &pane(now - 30),
                None,
                Some(960),
                Some(965),
                now
            )
            .0,
            SessionState::Idle
        );
        // Untracked agents: terminal activity.
        assert_eq!(
            tmux_state(AgentKind::Hermes, &pane(now - 1), None, None, None, now).0,
            SessionState::Working
        );
        assert_eq!(
            tmux_state(AgentKind::Hermes, &pane(now - 10), None, None, None, now).0,
            SessionState::Idle
        );
    }

    #[test]
    fn codex_main_thread_is_the_one_answering_bridge_input() {
        let events = vec![
            TurnEvent {
                at: 1,
                session_id: Some("title".into()),
                message: Some("{\"title\":\"x\"}".into()),
                input: Some("Generate a title … User prompt:\nfix it".into()),
            },
            TurnEvent {
                at: 2,
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
