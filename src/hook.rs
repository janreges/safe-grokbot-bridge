//! Turn-complete events.
//!
//! Sessions the bridge starts in tmux get a per-session hook that runs
//! `safe-grokbot-bridge hook <agent> <events-file>` when the agent finishes a turn:
//! Claude Code's `Stop` hook passes a JSON payload on stdin, Codex's `notify` program receives it
//! as the last argument. The hook appends one line to the session's events file, which the
//! bridge reads to learn that a turn ended and what the agent answered. It never fails, so it
//! can never block an agent.

use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_MESSAGE: usize = 64 * 1024;
const MAX_INPUT: usize = 4 * 1024;
const MAX_FILE_READ: u64 = 2 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TurnEvent {
    /// Unix time in milliseconds.
    pub at_ms: u64,
    /// Claude session id or Codex thread id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The agent's final message of the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The user input that started the turn (Codex only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
}

pub fn unix_now() -> u64 {
    unix_now_ms() / 1000
}

pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Appended to a message or input that was cut.
pub const TRUNCATED: &str = "\n[… truncated]";

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATED}", &text[..end])
}

/// Extracts a turn event from an agent's hook payload. Payloads that are not a completed turn
/// (or not understood) yield `None`.
pub fn parse_payload(agent: &str, payload: &str) -> Option<TurnEvent> {
    let v: Value = serde_json::from_str(payload).ok()?;
    let text = |value: &Value, max| value.as_str().map(|s| truncate(s, max));
    match agent {
        "claude" => Some(TurnEvent {
            at_ms: unix_now_ms(),
            session_id: text(&v["session_id"], 256),
            message: text(&v["last_assistant_message"], MAX_MESSAGE),
            input: None,
        }),
        "codex" if v["type"] == "agent-turn-complete" => Some(TurnEvent {
            at_ms: unix_now_ms(),
            session_id: text(&v["thread-id"], 256),
            message: text(&v["last-assistant-message"], MAX_MESSAGE),
            input: v["input-messages"]
                .as_array()
                .and_then(|inputs| inputs.last())
                .and_then(|input| text(input, MAX_INPUT)),
        }),
        _ => None,
    }
}

pub fn append(file: &Path, event: &TurnEvent) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?
        .write_all(line.as_bytes())
}

/// All events of a session, oldest first. A missing or unreadable file yields no events.
pub fn read_events(file: &Path) -> Vec<TurnEvent> {
    let Ok(mut f) = std::fs::File::open(file) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let cut = len > MAX_FILE_READ;
    if cut {
        use std::io::Seek;
        let _ = f.seek(std::io::SeekFrom::Start(len - MAX_FILE_READ));
    }
    let mut bytes = Vec::new();
    let _ = f.take(MAX_FILE_READ).read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    // Reading from the middle of the file starts inside a line (maybe inside a character).
    text.lines()
        .skip(usize::from(cut))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The `hook` command. `payload` is the last argument (Codex); otherwise stdin is read (Claude).
pub fn run_cli(agent: &str, file: &Path, payload: Option<String>) {
    let payload = payload.unwrap_or_else(|| {
        let mut buf = String::new();
        let _ = std::io::stdin()
            .take(8 * 1024 * 1024)
            .read_to_string(&mut buf);
        buf
    });
    if let Some(event) = parse_payload(agent, &payload) {
        // Best effort: an error here must not disturb the agent.
        let _ = append(file, &event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_are_parsed() {
        let claude = r#"{"session_id":"s-1","hook_event_name":"Stop","last_assistant_message":"SPIKE-OK","cwd":"/w"}"#;
        let event = parse_payload("claude", claude).unwrap();
        assert_eq!(
            (event.session_id.as_deref(), event.message.as_deref()),
            (Some("s-1"), Some("SPIKE-OK"))
        );

        let codex = r#"{"type":"agent-turn-complete","thread-id":"t-1","turn-id":"u","cwd":"/w","input-messages":["first","Reply OK"],"last-assistant-message":"OK"}"#;
        let event = parse_payload("codex", codex).unwrap();
        assert_eq!(event.input.as_deref(), Some("Reply OK"));
        assert_eq!(event.message.as_deref(), Some("OK"));
        assert!(parse_payload("codex", r#"{"type":"something-else"}"#).is_none());
        assert!(parse_payload("claude", "garbage").is_none());
    }

    #[test]
    fn events_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("events/s.jsonl");
        run_cli(
            "codex",
            &file,
            Some(
                r#"{"type":"agent-turn-complete","thread-id":"t","last-assistant-message":"one"}"#
                    .into(),
            ),
        );
        run_cli("codex", &file, Some("not json".into()));
        run_cli(
            "codex",
            &file,
            Some(
                r#"{"type":"agent-turn-complete","thread-id":"t","last-assistant-message":"two"}"#
                    .into(),
            ),
        );
        let events = read_events(&file);
        let messages: Vec<_> = events
            .iter()
            .map(|e| e.message.as_deref().unwrap())
            .collect();
        assert_eq!(messages, ["one", "two"]);
        assert!(read_events(&dir.path().join("missing")).is_empty());
        assert_eq!(truncate("ééé", 3), "é\n[… truncated]");

        // A long history is read from its end, even when the cut falls inside a character.
        let long = dir.path().join("long.jsonl");
        let old = TurnEvent {
            at_ms: 1,
            session_id: None,
            message: Some("ž".repeat(1000)),
            input: None,
        };
        for _ in 0..1500 {
            append(&long, &old).unwrap();
        }
        let last = TurnEvent {
            message: Some("last".into()),
            ..old.clone()
        };
        append(&long, &last).unwrap();
        let events = read_events(&long);
        assert_eq!(
            events.last().and_then(|e| e.message.as_deref()),
            Some("last")
        );
    }
}
