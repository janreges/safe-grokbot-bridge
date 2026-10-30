//! Headless sessions: every message is one non-interactive agent turn (`claude -p`,
//! `codex exec`, `hermes chat -Q`), continued with the agent's own resume mechanism.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::agents::{self, Launch, StreamEvent};
use crate::config::AgentKind;
use crate::exec;
use crate::sessions::{Phase, Record};
use crate::tmux::scrub_env;

const STDERR_TAIL: usize = 8 * 1024;

/// Starts one turn in the background. The caller has checked that no turn is running.
pub fn spawn_turn(record: Arc<Record>, launch: Launch, prompt: String, max_secs: u64) {
    {
        let mut state = record.state();
        state.phase = Phase::Working;
        state.error = None;
        state.last_input_at = Some(crate::hook::unix_now());
    }
    tokio::spawn(async move {
        let result = run_turn(&record, &launch, &prompt, max_secs).await;
        let mut state = record.state();
        state.child_pid = None;
        if state.phase != Phase::Working {
            return; // stopped meanwhile
        }
        match result {
            Ok(()) if record.keep_alive => state.phase = Phase::Idle,
            Ok(()) => {
                state.phase = Phase::Finished;
                state.ended_at = Some(SystemTime::now());
            }
            Err(message) => {
                state.phase = Phase::Failed;
                state.error = Some(message);
            }
        }
    });
}

async fn run_turn(
    record: &Record,
    launch: &Launch,
    prompt: &str,
    max_secs: u64,
) -> Result<(), String> {
    let agent = &record.agent;
    let argv = agent
        .headless_argv(&record.cwd, launch)
        .map_err(|e| e.message)?;
    let program = exec::which(&argv[0]).ok_or_else(|| {
        format!(
            "`{}` was not found on the bridge's PATH. Set an absolute `command` for agent `{}`.",
            argv[0], agent.name
        )
    })?;
    let mut cmd = Command::new(program);
    scrub_env(&mut cmd);
    cmd.args(&argv[1..])
        .current_dir(&record.cwd)
        .envs(agent.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Own process group, so stopping a turn also stops the tools the agent started.
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("Cannot start `{}`: {err}", argv[0]))?;
    let pid = child.id();
    record.state().child_pid = pid;

    if let Some(mut stdin) = child.stdin.take() {
        let prompt = prompt.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
        });
    }
    let stderr = child.stderr.take().expect("stderr is piped");
    let stderr_task = tokio::spawn(async move {
        exec::read_capped(stderr, 1024 * 1024)
            .await
            .map(|(bytes, _)| tail(&bytes))
            .unwrap_or_default()
    });
    let mut lines = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();

    let mut outcome: Option<Result<(), String>> = None;
    let mut plain_output = String::new();
    let work = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let events = match agent.kind {
                AgentKind::Claude => agents::parse_claude_line(&line),
                AgentKind::Codex => agents::parse_codex_line(&line),
                _ => {
                    plain_output.push_str(&line);
                    plain_output.push('\n');
                    continue;
                }
            };
            let mut state = record.state();
            for event in events {
                match event {
                    StreamEvent::SessionId(id) => state.agent_session_id = Some(id),
                    StreamEvent::Message(text) => {
                        state.last_message = Some(text);
                        state.last_message_at = Some(SystemTime::now());
                    }
                    StreamEvent::Done => outcome = Some(Ok(())),
                    StreamEvent::Failed(message) => outcome = Some(Err(message)),
                }
            }
        }
        child.wait().await
    };
    let status = match tokio::time::timeout(Duration::from_secs(max_secs), work).await {
        Ok(status) => status.map_err(|err| format!("Waiting for the agent failed: {err}"))?,
        Err(_) => {
            if let Some(pid) = pid {
                kill_tree(pid);
            }
            return Err(format!(
                "The turn did not finish within {max_secs} s and was stopped."
            ));
        }
    };
    let stderr = stderr_task.await.unwrap_or_default();

    if agent.kind == AgentKind::Hermes {
        let (session_id, answer) = agents::parse_hermes_output(&plain_output, &stderr);
        let mut state = record.state();
        if session_id.is_some() {
            state.agent_session_id = session_id;
        }
        if !answer.is_empty() {
            state.last_message = Some(answer);
            state.last_message_at = Some(SystemTime::now());
        }
    }
    match outcome {
        Some(result) => result,
        None if status.success() => Ok(()),
        None => Err(format!(
            "`{}` exited with {status}: {}",
            argv[0],
            if stderr.trim().is_empty() {
                "no error output"
            } else {
                stderr.trim()
            }
        )),
    }
}

fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_TAIL);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Stops a turn's process group (Unix) or process tree (Windows).
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        let pgid = pid as libc::pid_t;
        // SAFETY: killpg only sends a signal; an outdated pid fails harmlessly with ESRCH.
        unsafe {
            libc::killpg(pgid, libc::SIGTERM);
        }
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            // SAFETY: as above; signal 0 only checks whether the group still exists.
            unsafe {
                if libc::killpg(pgid, 0) == 0 {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
        });
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
