//! The real binary end to end: TLS with a pinned key, the turn hooks of Claude Code and Codex,
//! and taking over tmux sessions after a restart. The agents are fakes (Python scripts); tests
//! skip themselves when tmux, Python or curl is missing.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{script, toml_str};
use safe_grokbot_bridge::auth;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_safe-grokbot-bridge");

/// Stand-in for Claude Code in tmux: answers `ANSWER:<text>` to its prompt and to every line it
/// reads, and after each answer runs the Stop hook from `--settings` as Claude Code does.
const FAKE_CLAUDE: &str = r#"#!/usr/bin/env python3
import json, subprocess, sys
args = sys.argv[1:]
if args[:1] == ["agents"]:
    print("[]")
    sys.exit(0)
hook = json.loads(args[args.index("--settings") + 1])["hooks"]["Stop"][0]["hooks"][0]["command"]
session = args[args.index("--session-id") + 1]

def turn(text):
    print("ANSWER:" + text, flush=True)
    payload = {"session_id": session, "last_assistant_message": "ANSWER:" + text}
    subprocess.run(["sh", "-c", hook], input=json.dumps(payload), text=True)

if "--" in args:
    turn(args[args.index("--") + 1])
for line in sys.stdin:
    if line.strip():
        turn(line.strip())
"#;

/// Stand-in for Codex in tmux: the same, but it calls the `notify` program from `-c notify=[…]`
/// with Codex's `agent-turn-complete` payload as its last argument.
const FAKE_CODEX: &str = r#"#!/usr/bin/env python3
import json, subprocess, sys
args = sys.argv[1:]
notify = next(json.loads(a[len("notify="):]) for a in args if a.startswith("notify="))

def turn(text):
    print("ANSWER:" + text, flush=True)
    payload = {"type": "agent-turn-complete", "thread-id": "thread-1",
               "last-assistant-message": "ANSWER:" + text, "input-messages": [text]}
    subprocess.run(notify + [json.dumps(payload)])

if "--" in args:
    turn(args[args.index("--") + 1])
for line in sys.stdin:
    if line.strip():
        turn(line.strip())
"#;

fn available(program: &str, arg: &str) -> bool {
    let found = Command::new(program)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success());
    // CI sets this where it installed tmux; Python and curl are there too.
    if !found && std::env::var_os("SGB_REQUIRE_TMUX").is_some() {
        panic!("{program} is required (SGB_REQUIRE_TMUX is set) but not installed");
    }
    if !found {
        eprintln!("{program} is not installed; skipping");
    }
    found
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A running `safe-grokbot-bridge serve`, stopped with SIGTERM when dropped.
struct Bridge {
    child: Child,
    port: u16,
    token: String,
}

impl Bridge {
    fn start(config: &Path, port: u16, token: &str) -> Bridge {
        let child = Command::new(BIN)
            .args(["serve", "--config"])
            .arg(config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let bridge = Bridge {
            child,
            port,
            token: token.into(),
        };
        let started = Instant::now();
        while bridge.call("GET", "/healthz", None).0 != 200 {
            assert!(started.elapsed() < Duration::from_secs(20), "no bridge");
            std::thread::sleep(Duration::from_millis(100));
        }
        bridge
    }

    /// One HTTP/1.1 request; `(0, Null)` when the bridge cannot be reached.
    fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let exchange = || -> std::io::Result<String> {
            let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
            stream.set_read_timeout(Some(Duration::from_secs(120)))?;
            let body = body.as_ref().map(Value::to_string).unwrap_or_default();
            write!(
                stream,
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                self.token,
                body.len()
            )?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        };
        let Ok(response) = exchange() else {
            return (0, Value::Null);
        };
        let (head, body) = response.split_once("\r\n\r\n").unwrap_or_default();
        let status = head.split(' ').nth(1).and_then(|s| s.parse().ok());
        let body = serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.into()));
        (status.unwrap_or(0), body)
    }

    /// Long-polls a session until `done` holds; fails after a minute.
    fn wait_for(&self, id: &str, done: impl Fn(&Value) -> bool) -> Value {
        let started = Instant::now();
        loop {
            let (_, session) = self.call("GET", &format!("/v1/sessions/{id}?wait=5"), None);
            if done(&session) {
                return session;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "gave up waiting: {session}"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // SAFETY: kill only sends a signal to the child this value owns.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        let _ = self.child.wait();
    }
}

#[test]
fn turn_hooks_drive_sessions_and_a_restart_keeps_them() {
    if !available("tmux", "-V") || !available("python3", "--version") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = dunce::canonicalize(dir.path()).unwrap().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let claude = script(dir.path(), "fake-claude", FAKE_CLAUDE);
    let codex = script(dir.path(), "fake-codex", FAKE_CODEX);
    let token = auth::generate_token();
    let port = free_port();
    let socket = format!("sgb-process-{}", std::process::id());
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:{port}\"\nstate_dir = {}\n[[tokens]]\nname = \"test\"\nsha256 = \"{}\"\n[sessions]\nrunner = \"tmux\"\ntmux_socket = \"{socket}\"\n[agents.claude]\ncommand = {}\n[agents.codex]\ncommand = {}\n[[workspaces]]\npath = {}\nallow = [\"sessions\", \"input\", \"start\"]\n",
            toml_str(&dir.path().join("state")),
            auth::hash_token(&token),
            toml_str(&claude),
            toml_str(&codex),
            toml_str(&ws),
        ),
    )
    .unwrap();
    let answered =
        |text: &'static str| move |s: &Value| s["state"] == "idle" && s["last_message"] == text;

    let bridge = Bridge::start(&config, port, &token);
    // Claude Code: the answers to the prompt and to a follow-up arrive through the Stop hook.
    let (status, started) = bridge.call(
        "POST",
        "/v1/sessions",
        Some(json!({"cwd": ws, "agent": "claude", "prompt": "first"})),
    );
    assert_eq!(status, 201, "{started}");
    let id = started["id"].as_str().unwrap().to_string();
    let session = bridge.wait_for(&id, answered("ANSWER:first"));
    assert!(
        session["agent_session_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    let (status, body) = bridge.call(
        "POST",
        &format!("/v1/sessions/{id}/input"),
        Some(json!({"text": "second"})),
    );
    assert_eq!(status, 200, "{body}");
    bridge.wait_for(&id, answered("ANSWER:second"));

    // Codex, one-shot: its notify program reports the answer, and the session closes itself.
    let (status, started) = bridge.call(
        "POST",
        "/v1/sessions",
        Some(json!({"cwd": ws, "agent": "codex", "prompt": "third", "keep_alive": false})),
    );
    assert_eq!(status, 201, "{started}");
    let one_shot = started["id"].as_str().unwrap().to_string();
    bridge.wait_for(&one_shot, |s| {
        s["state"] == "finished" && s["last_message"] == "ANSWER:third"
    });

    // A restarted bridge takes the Claude session over and keeps driving it.
    drop(bridge);
    let bridge = Bridge::start(&config, port, &token);
    let (_, list) = bridge.call("GET", "/v1/sessions", None);
    let taken = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id.as_str())
        .unwrap_or_else(|| panic!("not taken over: {list}"));
    assert_eq!(
        (&taken["started_by_bridge"], &taken["controllable"]),
        (&json!(true), &json!(true)),
        "{taken}"
    );
    let (status, body) = bridge.call(
        "POST",
        &format!("/v1/sessions/{id}/input"),
        Some(json!({"text": "fourth"})),
    );
    assert_eq!(status, 200, "{body}");
    bridge.wait_for(&id, answered("ANSWER:fourth"));
    bridge.call("DELETE", &format!("/v1/sessions/{id}"), None);
    drop(bridge);
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .output();
}

#[test]
fn tls_serves_the_pinned_key_and_drops_silent_connections() {
    if !available("curl", "--version") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let token = auth::generate_token();
    let port = free_port();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:{port}\"\nstate_dir = {}\n[server.tls]\nhostnames = [\"localhost\"]\n[[tokens]]\nname = \"test\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\n",
            toml_str(&dir.path().join("state")),
            auth::hash_token(&token),
            toml_str(&ws),
        ),
    )
    .unwrap();
    // `check` creates the certificate and prints the pin that clients are told to use.
    let check = Command::new(BIN)
        .args(["check", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    let check = String::from_utf8_lossy(&check.stdout);
    let pin = check
        .lines()
        .find_map(|line| line.strip_prefix("Pin")?.trim().strip_prefix("sha256//"))
        .map(|pin| format!("sha256//{pin}"))
        .unwrap_or_else(|| panic!("no pin in: {check}"));
    let child = Command::new(BIN)
        .args(["serve", "--config"])
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _bridge = Bridge {
        child,
        port,
        token: token.clone(),
    };
    let curl = |pin: &str| {
        Command::new("curl")
            .args(["-s", "-k", "--max-time", "5", "--pinnedpubkey", pin])
            .arg(format!("https://127.0.0.1:{port}/healthz"))
            .output()
            .unwrap()
    };
    let started = Instant::now();
    let mut out = curl(&pin);
    while !out.status.success() && started.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(200));
        out = curl(&pin);
    }
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
    // Another key is refused (curl's exit code 90: pinned key mismatch).
    let wrong = curl("sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    assert_eq!(wrong.status.code(), Some(90));
    // A connection that never starts the TLS handshake is closed after about 10 s.
    let mut silent = TcpStream::connect(("127.0.0.1", port)).unwrap();
    silent
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let opened = Instant::now();
    let read = silent.read(&mut [0u8; 16]);
    let closed_after = opened.elapsed();
    assert!(matches!(read, Ok(0)) || read.is_err(), "{read:?}");
    assert!(
        (Duration::from_secs(9)..Duration::from_secs(14)).contains(&closed_after),
        "closed after {closed_after:?}"
    );
}
