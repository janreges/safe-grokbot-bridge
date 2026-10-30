//! Session runners driven end to end with fake agents (shell scripts).

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use axum::http::StatusCode;
use common::{Harness, harness, script, toml_str};
use serde_json::{Value, json};

/// A stand-in for `claude -p --output-format stream-json`: answers `echo: <prompt>`, logs its
/// arguments, fails on the prompt `fail` and hangs on `slow`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "agents" ]; then echo '[]'; exit 0; fi
echo "$@" >> "$ARGS_LOG"
sid=""; prev=""
for a in "$@"; do
  if [ "$prev" = "--session-id" ] || [ "$prev" = "--resume" ]; then sid="$a"; fi
  prev="$a"
done
prompt=$(cat)
if [ "$prompt" = "fail" ]; then
  echo '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom","session_id":"'"$sid"'"}'
  exit 1
fi
if [ "$prompt" = "slow" ]; then sleep 30; fi
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$sid"
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"working on %s"}]}}\n' "$prompt"
printf '{"type":"result","subtype":"success","is_error":false,"result":"echo: %s","session_id":"%s"}\n' "$prompt" "$sid"
"#;

/// A bridge whose `claude` agent is [`FAKE_CLAUDE`]; returns the path of its argument log.
async fn headless_harness() -> (Harness, PathBuf) {
    let bin = tempfile::tempdir().unwrap().keep();
    let claude = script(&bin, "claude", FAKE_CLAUDE);
    let log = bin.join("args.log");
    let extra = format!(
        "[sessions]\nrunner = \"headless\"\n[agents.claude]\ncommand = {}\nenv = {{ ARGS_LOG = {} }}\n",
        toml_str(&claude),
        toml_str(&log),
    );
    (
        harness("[\"files\", \"sessions\", \"input\", \"start\"]", &extra).await,
        log,
    )
}

async fn wait_idle(h: &Harness, id: &str) -> Value {
    let (status, body) = h.get(&format!("/v1/sessions/{id}?wait=20")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn headless_sessions_answer_and_continue_the_conversation() {
    let (h, _) = headless_harness().await;
    let (status, started) = h
        .post(
            "/v1/sessions",
            json!({"cwd": h.root, "agent": "claude", "prompt": "hello"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    assert_eq!(started["runner"], "headless");
    let id = started["id"].as_str().unwrap().to_string();

    let done = wait_idle(&h, &id).await;
    assert_eq!(done["state"], "idle", "{done}");
    assert_eq!(done["last_message"], "echo: hello");
    let session_id = done["agent_session_id"].as_str().unwrap().to_string();
    assert_eq!(session_id.len(), 36, "a --session-id UUID");

    let (status, _) = h
        .post(
            &format!("/v1/sessions/{id}/input"),
            json!({"text": "again"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let done = wait_idle(&h, &id).await;
    assert_eq!(done["last_message"], "echo: again");

    let (_, _) = h
        .post(&format!("/v1/sessions/{id}/input"), json!({"text": "fail"}))
        .await;
    let failed = wait_idle(&h, &id).await;
    assert_eq!(failed["state"], "failed", "{failed}");
    assert_eq!(failed["error"], "boom");

    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/input"),
            json!({"keys": ["Enter"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn headless_follow_up_turns_resume_the_agent_session() {
    let (h, log) = headless_harness().await;
    let (_, started) = h
        .post(
            "/v1/sessions",
            json!({"cwd": h.root, "agent": "claude", "prompt": "one"}),
        )
        .await;
    let id = started["id"].as_str().unwrap().to_string();
    let first = wait_idle(&h, &id).await;
    let session_id = first["agent_session_id"].as_str().unwrap().to_string();
    h.post(&format!("/v1/sessions/{id}/input"), json!({"text": "two"}))
        .await;
    wait_idle(&h, &id).await;

    let log = std::fs::read_to_string(log).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert!(
        lines[0].contains(&format!("--session-id {session_id}")),
        "{log}"
    );
    assert!(
        lines[1].contains(&format!("--resume {session_id}")),
        "{log}"
    );
    assert!(
        lines[0].contains("--permission-mode auto"),
        "headless default permission mode: {log}"
    );
}

#[tokio::test]
async fn keep_alive_false_finishes_after_the_first_answer_and_stop_kills_a_turn() {
    let (h, _) = headless_harness().await;
    let (_, started) = h
        .post(
            "/v1/sessions",
            json!({"cwd": h.root, "agent": "claude", "prompt": "once", "keep_alive": false}),
        )
        .await;
    let id = started["id"].as_str().unwrap().to_string();
    let done = wait_idle(&h, &id).await;
    assert_eq!(done["state"], "finished", "{done}");
    assert_eq!(done["last_message"], "echo: once");
    let (status, body) = h
        .post(&format!("/v1/sessions/{id}/input"), json!({"text": "more"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (_, slow) = h
        .post(
            "/v1/sessions",
            json!({"cwd": h.root, "agent": "claude", "prompt": "slow"}),
        )
        .await;
    let slow_id = slow["id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, running) = h.get(&format!("/v1/sessions/{slow_id}")).await;
    assert_eq!(running["state"], "working", "{running}");
    let (status, busy) = h
        .post(
            &format!("/v1/sessions/{slow_id}/input"),
            json!({"text": "x"}),
        )
        .await;
    assert_eq!(
        (status, busy["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("busy"))
    );
    let (status, stopped) = h.delete(&format!("/v1/sessions/{slow_id}")).await;
    assert_eq!(status, StatusCode::OK, "{stopped}");
    assert_eq!(stopped["state"], "exited");
}

#[tokio::test]
async fn start_requests_are_checked_against_the_workspace() {
    let (h, _) = headless_harness().await;
    let outside = h.dir.path().join("state");
    let cases = [
        (
            json!({"cwd": "/", "agent": "claude"}),
            StatusCode::FORBIDDEN,
        ),
        (
            json!({"cwd": outside, "agent": "claude"}),
            StatusCode::FORBIDDEN,
        ),
        (
            json!({"cwd": h.root, "agent": "nope"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"cwd": h.root, "agent": "claude", "bypass_permissions": true}),
            StatusCode::FORBIDDEN,
        ),
        (
            json!({"cwd": h.root, "agent": "claude", "model": "--evil"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"cwd": h.root, "agent": "claude", "unknown": 1}),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (request, expected) in cases {
        let (status, body) = h.post("/v1/sessions", request.clone()).await;
        assert_eq!(status, expected, "{request} -> {body}");
    }
}

fn tmux_available() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn tmux_sessions_take_text_and_keys() {
    if !tmux_available() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let socket = format!("sgb-test-{}", std::process::id());
    let bin = tempfile::tempdir().unwrap().keep();
    let echo = script(
        &bin,
        "echo-agent",
        "#!/bin/sh\nwhile IFS= read -r line; do echo \"GOT:$line\"; done\n",
    );
    let extra = format!(
        "[sessions]\nrunner = \"tmux\"\ntmux_socket = \"{socket}\"\n[agents.echo]\ncommand = {}\n",
        toml_str(&echo)
    );
    let h = harness("[\"sessions\", \"input\", \"start\"]", &extra).await;

    let (status, started) = h
        .post("/v1/sessions", json!({"cwd": h.root, "agent": "echo"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    assert_eq!(started["runner"], "tmux");
    let id = started["id"].as_str().unwrap().to_string();
    assert!(started["attach_command"].as_str().unwrap().contains(&id));

    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/input"),
            json!({"text": "hello world"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/input"),
            json!({"keys": ["x", "Enter"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut screen = String::new();
    for _ in 0..30 {
        let (_, session) = h.get(&format!("/v1/sessions/{id}?lines=20")).await;
        screen = session["screen"].as_str().unwrap_or_default().to_string();
        if screen.contains("GOT:hello world") && screen.contains("GOT:x") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        screen.contains("GOT:hello world") && screen.contains("GOT:x"),
        "{screen}"
    );

    let (status, _) = h
        .post(
            &format!("/v1/sessions/{id}/input"),
            json!({"keys": ["kill-server"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, list) = h.get("/v1/sessions").await;
    let listed = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id.as_str())
        .cloned();
    assert_eq!(listed.unwrap()["started_by_bridge"], true);

    let (status, stopped) = h.delete(&format!("/v1/sessions/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{stopped}");
    assert_eq!(stopped["state"], "exited");
    assert!(
        stopped["screen"]
            .as_str()
            .unwrap_or_default()
            .contains("GOT:hello world")
    );
    let _ = std::process::Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .output();
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let tmpdir = std::env::var("TMUX_TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let _ = std::fs::remove_file(
        std::path::Path::new(&tmpdir)
            .join(format!("tmux-{uid}"))
            .join(&socket),
    );
}
