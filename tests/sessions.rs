//! Session runners driven end to end with fake agents (shell scripts).

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use axum::http::StatusCode;
use common::{Harness, harness, script, toml_str};
use serde_json::{Value, json};

/// A stand-in for `claude -p --output-format stream-json`: answers `echo: <prompt>`, logs its
/// arguments, fails on the prompt `fail`, hangs (after writing its pid) on `slow` and leaves a
/// background process behind on `spawn`.
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
if [ "$prompt" = "slow" ]; then echo $$ > "$ARGS_LOG.pid"; sleep 30; fi
if [ "$prompt" = "spawn" ]; then sleep 300 & echo $! > "$ARGS_LOG.child"; fi
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$sid"
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"working on %s"}]}}\n' "$prompt"
printf '{"type":"result","subtype":"success","is_error":false,"result":"echo: %s","session_id":"%s"}\n' "$prompt" "$sid"
"#;

struct Headless {
    h: Harness,
    log: PathBuf,
    _bin: tempfile::TempDir,
}

/// A bridge whose `claude` agent is [`FAKE_CLAUDE`].
async fn headless_harness() -> Headless {
    let bin = tempfile::tempdir().unwrap();
    let claude = script(bin.path(), "claude", FAKE_CLAUDE);
    let log = bin.path().join("args.log");
    let extra = format!(
        "[sessions]\nrunner = \"headless\"\n[agents.claude]\ncommand = {}\nenv = {{ ARGS_LOG = {} }}\n",
        toml_str(&claude),
        toml_str(&log),
    );
    let h = harness("[\"files\", \"sessions\", \"input\", \"start\"]", &extra).await;
    Headless { h, log, _bin: bin }
}

async fn wait_idle(h: &Harness, id: &str) -> Value {
    let (status, body) = h.get(&format!("/v1/sessions/{id}?wait=20")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn headless_sessions_answer_and_continue_the_conversation() {
    let Headless { h, log, _bin } = headless_harness().await;
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
    // The resume command shown to clients leaves out configured variables, which may be secrets.
    let resume = done["attach_command"].as_str().unwrap();
    assert!(
        resume.contains(&session_id) && !resume.contains("ARGS_LOG"),
        "{resume}"
    );
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

    // Follow-up turns resume the agent's conversation.
    let args = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    assert!(
        lines[0].contains(&format!("--session-id {session_id}")),
        "{args}"
    );
    assert!(
        lines[1].contains(&format!("--resume {session_id}")),
        "{args}"
    );
    assert!(lines[0].contains("--permission-mode auto"), "{args}");

    let (_, _) = h
        .post(&format!("/v1/sessions/{id}/input"), json!({"text": "fail"}))
        .await;
    let failed = wait_idle(&h, &id).await;
    assert_eq!(failed["state"], "failed", "{failed}");
    assert_eq!(failed["error"], "boom");
    assert!(
        failed["last_message"].is_null(),
        "a failed turn shows no stale answer"
    );

    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/input"),
            json!({"keys": ["Enter"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn concurrent_messages_start_only_one_turn() {
    let Headless { h, log, _bin } = headless_harness().await;
    let (_, started) = h
        .post("/v1/sessions", json!({"cwd": h.root, "agent": "claude"}))
        .await;
    let id = started["id"].as_str().unwrap().to_string();
    let uri = format!("/v1/sessions/{id}/input");
    // A slow turn keeps running while the second request arrives, however late it is.
    let (a, b) = tokio::join!(
        h.post(&uri, json!({"text": "slow"})),
        h.post(&uri, json!({"text": "slow"}))
    );
    let mut statuses = [a.0, b.0];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "{a:?} {b:?}"
    );
    let pid_file = PathBuf::from(format!("{}.pid", log.display()));
    for _ in 0..50 {
        if pid_file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let runs = std::fs::read_to_string(&log).unwrap().lines().count();
    assert_eq!(runs, 1);
    h.delete(&format!("/v1/sessions/{id}")).await;
}

#[tokio::test]
async fn keep_alive_false_finishes_after_the_first_answer_and_stop_kills_a_turn() {
    let Headless { h, log, _bin } = headless_harness().await;
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
    let pid_file = PathBuf::from(format!("{}.pid", log.display()));
    for _ in 0..50 {
        if pid_file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
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
    // The agent process itself is gone, not just reported as stopped.
    for _ in 0..50 {
        if !alive(&pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive(&pid), "the turn's process {pid} still runs");
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test]
async fn a_finished_turn_leaves_no_processes_behind() {
    let Headless { h, log, _bin } = headless_harness().await;
    let (_, started) = h
        .post(
            "/v1/sessions",
            json!({"cwd": h.root, "agent": "claude", "prompt": "spawn"}),
        )
        .await;
    let id = started["id"].as_str().unwrap().to_string();
    let done = wait_idle(&h, &id).await;
    assert_eq!(done["state"], "idle", "{done}");
    assert_eq!(done["last_message"], "echo: spawn");
    let child = std::fs::read_to_string(format!("{}.child", log.display())).unwrap();
    assert!(
        !alive(child.trim()),
        "the turn's background process still runs"
    );
}

#[tokio::test]
async fn a_replaced_folder_gets_no_further_turns() {
    let Headless { h, _bin, .. } = headless_harness().await;
    let project = h.root.join("project");
    std::fs::create_dir(&project).unwrap();
    let (_, started) = h
        .post(
            "/v1/sessions",
            json!({"cwd": project, "agent": "claude", "prompt": "hello"}),
        )
        .await;
    let id = started["id"].as_str().unwrap().to_string();
    wait_idle(&h, &id).await;
    // Another folder now has the session's path.
    std::fs::rename(&project, h.root.join("project-old")).unwrap();
    std::fs::create_dir(&project).unwrap();
    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/input"),
            json!({"text": "again"}),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("path_changed")),
        "{body}"
    );
}

#[tokio::test]
async fn start_requests_are_checked_against_the_workspace() {
    let Headless { h, .. } = headless_harness().await;
    let outside = h.dir.path().join("state");
    std::fs::create_dir_all(&outside).unwrap();
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
            json!({"cwd": h.root, "agent": "claude", "resume": "11111111-2222-3333-4444-555555555555"}),
            StatusCode::FORBIDDEN,
        ),
        (
            json!({"cwd": h.root, "agent": "claude", "unknown": 1}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ];
    for (request, expected) in cases {
        let (status, body) = h.post("/v1/sessions", request.clone()).await;
        assert_eq!(status, expected, "{request} -> {body}");
    }
}

fn tmux_available() -> bool {
    let available = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success());
    // CI sets this, so a missing tmux fails instead of silently skipping the test.
    if !available && std::env::var_os("SGB_REQUIRE_TMUX").is_some() {
        panic!("tmux is required (SGB_REQUIRE_TMUX is set) but not installed");
    }
    available
}

#[tokio::test]
async fn tmux_sessions_take_text_and_keys_and_ignore_other_panes() {
    if !tmux_available() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let socket = format!("sgb-test-{}", std::process::id());
    let bin = tempfile::tempdir().unwrap();
    let echo = script(
        bin.path(),
        "echo-agent",
        "#!/bin/sh\nwhile IFS= read -r line; do echo \"GOT:$line\"; done\n",
    );
    let extra = format!(
        "[sessions]\nrunner = \"tmux\"\ntmux_socket = \"{socket}\"\n[agents.echo]\ncommand = {}\n",
        toml_str(&echo)
    );
    let h = harness("[\"sessions\", \"input\", \"start\"]", &extra).await;
    let tmux = |args: &[&str]| {
        std::process::Command::new("tmux")
            .args(["-L", &socket])
            .args(args)
            .output()
            .unwrap()
    };

    let (status, started) = h
        .post("/v1/sessions", json!({"cwd": h.root, "agent": "echo"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    assert_eq!(started["runner"], "tmux");
    let id = started["id"].as_str().unwrap().to_string();
    assert!(started["attach_command"].as_str().unwrap().contains(&id));

    // A shell someone opens next to the agent is neither listed nor typed into.
    tmux(&[
        "split-window",
        "-t",
        &format!("={id}:"),
        "-c",
        h.root.to_str().unwrap(),
    ]);

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

    // With `synchronize-panes` on, tmux would copy typed keys to the shell next to the agent.
    let window = format!("={id}:");
    tmux(&[
        "set-window-option",
        "-t",
        &window,
        "synchronize-panes",
        "on",
    ]);
    let (status, body) = h
        .post(&format!("/v1/sessions/{id}/input"), json!({"keys": ["x"]}))
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_controllable")),
        "{body}"
    );
    let (_, session) = h.get(&format!("/v1/sessions/{id}")).await;
    assert_eq!(session["controllable"], false, "{session}");
    tmux(&[
        "set-window-option",
        "-t",
        &window,
        "synchronize-panes",
        "off",
    ]);

    let (_, list) = h.get("/v1/sessions").await;
    let ids: Vec<&str> = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [id.as_str()], "{list}");

    // An agent running inside an interactive shell pane is listed but not controllable.
    tmux(&[
        "new-session",
        "-d",
        "-s",
        "byhand",
        "-c",
        h.root.to_str().unwrap(),
    ]);
    tmux(&[
        "send-keys",
        "-t",
        "=byhand:",
        &echo.display().to_string(),
        "Enter",
    ]);
    let mut by_hand = Value::Null;
    for _ in 0..30 {
        let (_, list) = h.get("/v1/sessions").await;
        if let Some(found) = list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] != id.as_str())
        {
            by_hand = found.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(by_hand["controllable"], false, "{by_hand}");
    let other = by_hand["id"].as_str().unwrap();
    let (status, body) = h
        .post(
            &format!("/v1/sessions/{other}/input"),
            json!({"text": "ls"}),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_controllable"))
    );

    let (status, stopped) = h.delete(&format!("/v1/sessions/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{stopped}");
    assert_eq!(stopped["state"], "exited");
    assert!(
        stopped["screen"]
            .as_str()
            .unwrap_or_default()
            .contains("GOT:hello world")
    );
    // A pane respawned with another program (here a shell) no longer runs the agent the
    // bridge started: the session counts as exited and takes no input.
    let (_, second) = h
        .post("/v1/sessions", json!({"cwd": h.root, "agent": "echo"}))
        .await;
    let second = second["id"].as_str().unwrap().to_string();
    tmux(&["respawn-pane", "-k", "-t", &format!("={second}:"), "sh"]);
    let (status, body) = h
        .post(
            &format!("/v1/sessions/{second}/input"),
            json!({"text": "echo typed-into-a-shell"}),
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("ended")),
        "{body}"
    );

    // tmux reads the start directory as a format; a folder name must stay literal.
    let odd = h.root.join("c#{session_name}");
    std::fs::create_dir(&odd).unwrap();
    let (status, third) = h
        .post("/v1/sessions", json!({"cwd": odd, "agent": "echo"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{third}");
    let third = third["id"].as_str().unwrap().to_string();
    let out = tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={third}:"),
        "#{pane_current_path}",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        odd.display().to_string()
    );
    // A folder name ending in `;` must not end the tmux command early.
    let semicolon = h.root.join("odd;");
    std::fs::create_dir(&semicolon).unwrap();
    let (status, fourth) = h
        .post("/v1/sessions", json!({"cwd": semicolon, "agent": "echo"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{fourth}");

    let _ = tmux(&["kill-server"]);
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let tmpdir = std::env::var("TMUX_TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let _ = std::fs::remove_file(
        std::path::Path::new(&tmpdir)
            .join(format!("tmux-{uid}"))
            .join(&socket),
    );
}
