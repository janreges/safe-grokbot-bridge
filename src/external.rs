//! Agent sessions running outside tmux, for example in a plain terminal tab or an IDE.
//!
//! Listing Claude Code sessions uses `claude agents --json`, Claude Code's supported scripting
//! interface. Everything else here is **experimental** (enabled by
//! `[experimental] external_sessions`) because it relies on formats that are not stable
//! public interfaces:
//!
//! - Claude Code: messages are posted to a session's inbox socket (cross-session messaging),
//!   found through Claude's session registry; answers are read from the session transcript.
//! - Codex: live interactive Codex processes are matched to their thread through the rollout
//!   files of their `CODEX_HOME`; messages are queued with `codex queue`; answers are read from
//!   the rollout file.
//!
//! Every function fails soft: when a format is not understood, a session simply shows less.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::Agent;
use crate::exec;
use crate::procs::{self, ProcInfo, Snapshot};
use crate::tmux::scrub_env;

// ---------------------------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------------------------

/// One entry of `claude agents --json`.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAgent {
    pub pid: Option<u32>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub kind: String,
    pub session_id: Option<String>,
    pub name: Option<String>,
    /// Live sessions: `busy`, `waiting` or `idle`.
    pub status: Option<String>,
    /// Background sessions: `working`, `blocked`, `done`, `failed` or `stopped`.
    pub state: Option<String>,
    pub waiting_for: Option<String>,
    pub started_at: Option<u64>,
    /// Name of the configured agent (Claude profile) that reported this session.
    #[serde(skip)]
    pub agent: String,
}

/// Running Claude Code sessions on this machine (interactive and background), as seen by one
/// configured Claude agent (profile).
pub async fn claude_agents(agent: &Agent) -> Result<Vec<ClaudeAgent>, String> {
    let mut cmd = Command::new(&agent.command);
    scrub_env(&mut cmd);
    cmd.envs(agent.env.iter().map(|(k, v)| (k, v)))
        .args(["agents", "--json"]);
    let out = exec::run(cmd, None, Duration::from_secs(10), 4 * 1024 * 1024)
        .await
        .map_err(|err| format!("{err:#}"))?;
    if !out.success() {
        return Err(out.stderr.trim().to_string());
    }
    let mut list: Vec<ClaudeAgent> =
        serde_json::from_str(&out.stdout).map_err(|err| format!("unexpected output: {err}"))?;
    for entry in &mut list {
        entry.agent = agent.name.clone();
    }
    Ok(list)
}

/// Whether a Claude Code conversation was held in `cwd`: its transcript lives in the project
/// folder Claude Code derives from `cwd`.
pub fn claude_conversation_in(claude_dir: &Path, cwd: &Path, session_id: &str) -> bool {
    claude_dir
        .join("projects")
        .join(claude_project_dir(cwd))
        .join(format!("{session_id}.jsonl"))
        .is_file()
}

/// Claude Code's config directory (`CLAUDE_CONFIG_DIR`, default `~/.claude`).
pub fn claude_dir(agent: &Agent) -> Option<PathBuf> {
    agent
        .env
        .iter()
        .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
        .map(|(_, v)| PathBuf::from(v))
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

/// Where a live Claude session accepts messages, from its registry entry
/// `<claude dir>/sessions/<pid>.json` and published key `<pid>.<hash>.key`.
pub struct ClaudeInbox {
    pub socket: PathBuf,
    pub token: Option<String>,
}

pub fn claude_inbox(claude_dir: &Path, pid: u32) -> Option<ClaudeInbox> {
    let sessions = claude_dir.join("sessions");
    let entry: Value =
        serde_json::from_str(&std::fs::read_to_string(sessions.join(format!("{pid}.json"))).ok()?)
            .ok()?;
    let socket = PathBuf::from(entry["messagingSocketPath"].as_str()?);
    let prefix = format!("{pid}.");
    let token = std::fs::read_dir(&sessions)
        .ok()?
        .flatten()
        .find_map(|file| {
            let name = file.file_name().to_string_lossy().into_owned();
            if !(name.starts_with(&prefix) && name.ends_with(".key")) {
                return None;
            }
            let key: Value =
                serde_json::from_str(&std::fs::read_to_string(file.path()).ok()?).ok()?;
            key["peerToken"].as_str().map(str::to_string)
        });
    Some(ClaudeInbox { socket, token })
}

/// The two lines posted to an inbox: the auth line (required on Windows) and a user message in
/// Claude Code's stream-json input format.
pub fn claude_inbox_lines(token: Option<&str>, text: &str) -> String {
    let mut lines = String::new();
    if let Some(token) = token {
        lines.push_str(&serde_json::json!({ "type": "auth", "token": token }).to_string());
        lines.push('\n');
    }
    let message = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": text },
    });
    lines.push_str(&message.to_string());
    lines.push('\n');
    lines
}

/// Posts a message to a Claude Code session's inbox. The receiving session applies its own
/// `crossSessionInbound` rules: it may deliver, hold (for local approval) or drop the message.
pub async fn claude_send(inbox: &ClaudeInbox, text: &str) -> Result<()> {
    let payload = claude_inbox_lines(inbox.token.as_deref(), text);
    let send = async {
        #[cfg(unix)]
        {
            let mut stream = tokio::net::UnixStream::connect(&inbox.socket).await?;
            stream.write_all(payload.as_bytes()).await?;
            stream.shutdown().await?;
        }
        #[cfg(windows)]
        {
            let mut pipe =
                tokio::net::windows::named_pipe::ClientOptions::new().open(&inbox.socket)?;
            pipe.write_all(payload.as_bytes()).await?;
            pipe.flush().await?;
        }
        anyhow::Ok(())
    };
    tokio::time::timeout(Duration::from_secs(10), send)
        .await
        .context("the session's inbox did not accept the message in time")?
        .with_context(|| {
            format!(
                "cannot write to the session inbox {}",
                inbox.socket.display()
            )
        })
}

/// Claude Code's project directory name for a working directory: every character other than
/// an ASCII letter or digit becomes `-`.
pub fn claude_project_dir(cwd: &Path) -> String {
    cwd.display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The last assistant text of a Claude session and its timestamp, from the transcript.
pub fn claude_last_message(
    claude_dir: &Path,
    cwd: &Path,
    session_id: &str,
) -> Option<(String, String)> {
    let projects = claude_dir.join("projects");
    let file_name = format!("{session_id}.jsonl");
    let mut path = projects.join(claude_project_dir(cwd)).join(&file_name);
    if !path.exists() {
        // Long paths are shortened with a hash; fall back to a scan.
        path = std::fs::read_dir(&projects)
            .ok()?
            .flatten()
            .map(|dir| dir.path().join(&file_name))
            .find(|candidate| candidate.exists())?;
    }
    let tail = read_tail(&path, 1024 * 1024)?;
    tail.lines().rev().find_map(|line| {
        let entry: Value = serde_json::from_str(line).ok()?;
        if entry["type"] != "assistant" || entry["isSidechain"] == true {
            return None;
        }
        let text: Vec<&str> = entry["message"]["content"]
            .as_array()?
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect();
        let text = text.join("\n");
        (!text.trim().is_empty()).then(|| {
            (
                text,
                entry["timestamp"].as_str().unwrap_or_default().to_string(),
            )
        })
    })
}

// ---------------------------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------------------------

/// A live interactive Codex session and the thread it most likely runs.
#[derive(Debug, Clone)]
pub struct CodexThread {
    pub id: String,
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub pid: u32,
    pub rollout: PathBuf,
    /// Several Codex sessions run in the same folder, so the thread may belong to another one.
    pub ambiguous: bool,
}

/// Codex subcommands that are not interactive sessions.
const CODEX_NON_INTERACTIVE: &[&str] = &[
    "app-server",
    "exec",
    "e",
    "mcp-server",
    "exec-server",
    "queue",
    "login",
    "logout",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "features",
    "agents",
    "remote-control",
    "cloud",
    "review",
    "mcp",
    "plugin",
    "completion",
    "update",
    "archive",
    "delete",
    "unarchive",
    "migrate-rollouts",
];

fn default_codex_home() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".codex"))
}

/// Finds interactive Codex processes and the rollout (thread) each one is writing.
pub fn codex_threads(snapshot: &Snapshot, codex_command: &str) -> Vec<CodexThread> {
    let is_codex = |process: &ProcInfo| {
        procs::runs_command(process, codex_command)
            && !process
                .cmd
                .iter()
                .skip(1)
                .any(|arg| CODEX_NON_INTERACTIVE.contains(&arg.as_str()))
    };
    // A launcher (node) and the native binary it starts both match; keep the outermost.
    let mut groups: std::collections::BTreeMap<(PathBuf, PathBuf), Vec<&ProcInfo>> =
        std::collections::BTreeMap::new();
    for process in snapshot.processes() {
        let launched_by_codex = process
            .parent
            .and_then(|parent| snapshot.get(parent))
            .is_some_and(is_codex);
        if !is_codex(process) || launched_by_codex {
            continue;
        }
        let Some(cwd) = process
            .cwd
            .as_ref()
            .and_then(|c| dunce::canonicalize(c).ok())
        else {
            continue;
        };
        let home = process
            .environ
            .iter()
            .find_map(|var| var.strip_prefix("CODEX_HOME="))
            .map(PathBuf::from)
            .or_else(default_codex_home);
        let Some(home) = home else { continue };
        groups.entry((home, cwd)).or_default().push(process);
    }
    let mut threads = Vec::new();
    for ((home, cwd), processes) in groups {
        let ambiguous = processes.len() > 1;
        for process in processes {
            if let Some((id, rollout)) = rollout_of(&home, &cwd, process) {
                threads.push(CodexThread {
                    id,
                    cwd: cwd.clone(),
                    home: home.clone(),
                    pid: process.pid,
                    rollout,
                    ambiguous,
                });
            }
        }
    }
    threads
}

/// The folder a Codex thread was started in, from its rollout file
/// (`sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`).
pub fn codex_thread_cwd(home: &Path, thread: &str) -> Option<PathBuf> {
    let suffix = format!("-{thread}.jsonl");
    for year in sorted_dirs(&home.join("sessions")) {
        for month in sorted_dirs(&year) {
            for day in sorted_dirs(&month) {
                for file in std::fs::read_dir(&day).into_iter().flatten().flatten() {
                    if file.file_name().to_string_lossy().ends_with(&suffix) {
                        return rollout_meta(&file.path()).map(|meta| meta.cwd);
                    }
                }
            }
        }
    }
    None
}

/// The folder a Hermes session belongs to, from `hermes sessions export`.
pub async fn hermes_session_cwd(agent: &Agent, session_id: &str) -> Option<PathBuf> {
    let mut cmd = Command::new(&agent.command);
    scrub_env(&mut cmd);
    cmd.envs(agent.env.iter().map(|(k, v)| (k, v))).args([
        "sessions",
        "export",
        "-",
        "--session-id",
        session_id,
    ]);
    let out = exec::run(cmd, None, Duration::from_secs(30), 64 * 1024 * 1024)
        .await
        .ok()?;
    let first = out.stdout.lines().next()?;
    let record: Value = serde_json::from_str(first).ok()?;
    record["cwd"].as_str().map(PathBuf::from)
}

/// The rollout (thread) a live Codex process works on, as `(thread id, path)`: the thread named
/// by `codex resume <id>`, or else the newest thread in `cwd` that started after the process.
/// A process that has not started a thread yet has none.
fn rollout_of(home: &Path, cwd: &Path, process: &ProcInfo) -> Option<(String, PathBuf)> {
    let resumed = process
        .cmd
        .iter()
        .skip_while(|arg| arg.as_str() != "resume")
        .nth(1)
        .filter(|id| !id.starts_with('-'));
    let started = std::time::UNIX_EPOCH
        + std::time::Duration::from_secs(process.start_time.saturating_sub(5));
    recent_rollouts(home).into_iter().find_map(|path| {
        let meta = rollout_meta(&path)?;
        let matches = match resumed {
            Some(id) => &meta.id == id,
            None => {
                let same_dir = dunce::canonicalize(&meta.cwd)
                    .map(|c| c == cwd)
                    .unwrap_or(meta.cwd == cwd);
                same_dir && meta.started.is_some_and(|at| at >= started)
            }
        };
        matches.then_some((meta.id, path))
    })
}

/// Rollout files of the last two weeks, newest first.
fn recent_rollouts(home: &Path) -> Vec<PathBuf> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    // sessions/YYYY/MM/DD/rollout-*.jsonl
    let mut days: Vec<PathBuf> = Vec::new();
    for year in sorted_dirs(&home.join("sessions"))
        .into_iter()
        .rev()
        .take(2)
    {
        for month in sorted_dirs(&year).into_iter().rev().take(2) {
            days.extend(sorted_dirs(&month).into_iter().rev().take(14));
        }
    }
    for day in days {
        for file in std::fs::read_dir(&day).into_iter().flatten().flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "jsonl")
                && let Ok(modified) = file.metadata().and_then(|m| m.modified())
            {
                files.push((modified, path));
            }
        }
    }
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.into_iter().take(200).map(|(_, path)| path).collect()
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

struct RolloutMeta {
    id: String,
    cwd: PathBuf,
    started: Option<std::time::SystemTime>,
}

/// Thread id, working directory and start time from a rollout's first line (`session_meta`).
fn rollout_meta(path: &Path) -> Option<RolloutMeta> {
    use std::io::{BufRead, Read};
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    // session_meta lines carry the base instructions, so they can be large.
    std::io::BufReader::new(file)
        .take(4 * 1024 * 1024)
        .read_line(&mut first)
        .ok()?;
    let meta: Value = serde_json::from_str(&first).ok()?;
    if meta["type"] != "session_meta" {
        return None;
    }
    let payload = &meta["payload"];
    Some(RolloutMeta {
        id: payload["id"].as_str()?.to_string(),
        cwd: PathBuf::from(payload["cwd"].as_str()?),
        started: payload["timestamp"]
            .as_str()
            .or(meta["timestamp"].as_str())
            .and_then(|t| humantime::parse_rfc3339_weak(t.trim_end_matches('Z')).ok()),
    })
}

/// The state of a Codex thread from its rollout: whether a turn is running, and the last answer
/// with its completion time.
pub struct CodexProgress {
    /// `None` when the rollout's recent part shows no turn event.
    pub working: Option<bool>,
    pub last_message: Option<String>,
    pub last_message_at: Option<String>,
}

pub fn codex_progress(rollout: &Path) -> Option<CodexProgress> {
    let tail = read_tail(rollout, 2 * 1024 * 1024)?;
    let mut working = None;
    let mut last = None;
    for line in tail.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry["type"] != "event_msg" {
            continue;
        }
        let payload = &entry["payload"];
        match payload["type"].as_str() {
            Some("task_started") => {
                working.get_or_insert(true);
            }
            Some("task_complete") => {
                working.get_or_insert(false);
                if last.is_none() {
                    last = Some((
                        payload["last_agent_message"].as_str().map(str::to_string),
                        entry["timestamp"].as_str().map(str::to_string),
                    ));
                }
            }
            Some("turn_aborted") => {
                working.get_or_insert(false);
            }
            _ => {}
        }
        if working.is_some() && last.is_some() {
            break;
        }
    }
    let (last_message, last_message_at) = last.unwrap_or((None, None));
    Some(CodexProgress {
        working,
        last_message,
        last_message_at,
    })
}

/// Queues a message for a Codex thread with `codex queue`. Codex delivers it when the thread is
/// idle; it never interrupts a running turn.
pub async fn codex_queue(agent: &Agent, home: &Path, thread: &str, text: &str) -> Result<String> {
    let mut cmd = Command::new(&agent.command);
    scrub_env(&mut cmd);
    cmd.envs(agent.env.iter().map(|(k, v)| (k, v)))
        .env("CODEX_HOME", home)
        .args(["queue", "--thread", thread, "--message", text]);
    let out = exec::run(cmd, None, Duration::from_secs(30), 64 * 1024).await?;
    if !out.success() {
        bail!("`codex queue` failed: {}", out.stderr.trim());
    }
    Ok(out.stdout.trim().to_string())
}

/// The last `max` bytes of a file, starting at a line boundary.
fn read_tail(path: &Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(if start > 0 {
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default()
    } else {
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_listing_and_registry_are_parsed() {
        let json = r#"[{"id":"1068a73f","cwd":"/w","kind":"background","sessionId":"s-bg","state":"blocked"},
                       {"pid":42,"cwd":"/w/app","kind":"interactive","sessionId":"s-1","name":"app-5b","status":"waiting","waitingFor":"permission prompt"}]"#;
        let agents: Vec<ClaudeAgent> = serde_json::from_str(json).unwrap();
        assert_eq!(agents[1].pid, Some(42));
        assert_eq!(agents[1].waiting_for.as_deref(), Some("permission prompt"));
        assert_eq!(agents[0].state.as_deref(), Some("blocked"));

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sessions")).unwrap();
        std::fs::write(
            dir.path().join("sessions/42.json"),
            r#"{"pid":42,"messagingSocketPath":"/run/user/1000/cc-socks/42.sock"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("sessions/42.abc.key"),
            r#"{"peerToken":"tok"}"#,
        )
        .unwrap();
        let inbox = claude_inbox(dir.path(), 42).unwrap();
        assert_eq!(
            inbox.socket,
            PathBuf::from("/run/user/1000/cc-socks/42.sock")
        );
        assert_eq!(inbox.token.as_deref(), Some("tok"));
        assert!(claude_inbox(dir.path(), 7).is_none());
    }

    #[test]
    fn claude_transcript_yields_the_last_answer() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = Path::new("/home/me/my.app");
        assert_eq!(claude_project_dir(cwd), "-home-me-my-app");
        let project = dir.path().join("projects").join(claude_project_dir(cwd));
        std::fs::create_dir_all(&project).unwrap();
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-10-30T08:00:00Z","message":{"content":[{"type":"text","text":"first"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-30T08:01:00Z","message":{"content":[{"type":"text","text":"final answer"}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"subagent"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"..."}]}}"#,
        ];
        std::fs::write(project.join("s-1.jsonl"), lines.join("\n")).unwrap();
        let (text, at) = claude_last_message(dir.path(), cwd, "s-1").unwrap();
        assert_eq!(
            (text.as_str(), at.as_str()),
            ("final answer", "2026-10-30T08:01:00Z")
        );
    }

    #[test]
    fn inbox_lines_use_the_stream_json_format() {
        let lines = claude_inbox_lines(Some("tok"), "hi \"there\"");
        let parsed: Vec<Value> = lines
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            parsed[0],
            serde_json::json!({"type": "auth", "token": "tok"})
        );
        assert_eq!(parsed[1]["type"], "user");
        assert_eq!(parsed[1]["message"]["content"], "hi \"there\"");
        assert_eq!(claude_inbox_lines(None, "x").lines().count(), 1);
    }

    #[test]
    fn codex_rollouts_are_matched_and_summarised() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let cwd = dunce::canonicalize(cwd.path()).unwrap();
        let day = home.path().join("sessions/2026/10/30");
        std::fs::create_dir_all(&day).unwrap();
        let meta = serde_json::json!({"type":"session_meta","payload":{"id":"t-1","cwd":cwd,"timestamp":"2026-10-30T08:00:00.000Z"}});
        let rollout = day.join("rollout-2026-10-30T08-00-00-t-1.jsonl");
        let lines = [
            meta.to_string(),
            r#"{"type":"event_msg","timestamp":"2026-10-30T08:00:05Z","payload":{"type":"task_started"}}"#.into(),
            r#"{"type":"event_msg","timestamp":"2026-10-30T08:01:00Z","payload":{"type":"task_complete","last_agent_message":"Done."}}"#.into(),
            r#"{"type":"event_msg","timestamp":"2026-10-30T08:02:00Z","payload":{"type":"task_started"}}"#.into(),
        ];
        std::fs::write(&rollout, lines.join("\n")).unwrap();
        let process = |start_time: u64, cmd: &[&str]| ProcInfo {
            pid: 7,
            parent: None,
            start_time,
            name: "codex".into(),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            cwd: Some(cwd.clone()),
            environ: vec![],
        };
        // 2026-10-30T07:59:00Z: the thread started after the process did.
        let fresh = process(1_793_347_140, &["codex"]);
        assert_eq!(
            rollout_of(home.path(), &cwd, &fresh),
            Some(("t-1".into(), rollout.clone()))
        );
        // A process started after the thread has not created its own thread yet.
        let later = process(1_793_347_500, &["codex"]);
        assert_eq!(rollout_of(home.path(), &cwd, &later), None);
        // `codex resume <id>` names its thread.
        let resumed = process(1_793_347_500, &["codex", "resume", "t-1"]);
        assert_eq!(
            rollout_of(home.path(), &cwd, &resumed),
            Some(("t-1".into(), rollout.clone()))
        );
        assert!(rollout_of(home.path(), Path::new("/elsewhere"), &fresh).is_none());
        assert_eq!(codex_thread_cwd(home.path(), "t-1"), Some(cwd.clone()));
        assert_eq!(codex_thread_cwd(home.path(), "t-2"), None);
        let progress = codex_progress(&rollout).unwrap();
        assert_eq!(progress.working, Some(true));
        assert_eq!(progress.last_message.as_deref(), Some("Done."));
        assert_eq!(
            progress.last_message_at.as_deref(),
            Some("2026-10-30T08:01:00Z")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inbox_messages_reach_the_socket() {
        use tokio::io::AsyncReadExt;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("inbox.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let reader = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut received = String::new();
            stream.read_to_string(&mut received).await.unwrap();
            received
        });
        let inbox = ClaudeInbox {
            socket,
            token: Some("tok".into()),
        };
        claude_send(&inbox, "run the tests").await.unwrap();
        let received = reader.await.unwrap();
        assert_eq!(received, claude_inbox_lines(Some("tok"), "run the tests"));
    }
}
