//! Headless sessions: every message is one non-interactive agent turn (`claude -p`,
//! `codex exec`, `hermes chat -Q`), continued with the agent's own resume mechanism.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::agents::{self, Launch, StreamEvent};
use crate::config::AgentKind;
use crate::exec;
use crate::sessions::{Phase, Record};
use crate::tmux::scrub_env;

const STDERR_TAIL: usize = 8 * 1024;
/// Longest stream line kept (a Claude/Codex event, or a Hermes output line).
const MAX_LINE: usize = 4 * 1024 * 1024;
/// Longest answer kept in memory.
const MAX_MESSAGE: usize = 256 * 1024;
/// Plain (Hermes) output kept for parsing.
const MAX_PLAIN: usize = 4 * 1024 * 1024;
/// After the agent exits, how long its pipes may stay open (held by a leftover descendant).
const DRAIN: Duration = Duration::from_secs(5);

/// Runs one turn in the background. The caller has already moved the record to
/// `Phase::Working` (under its lock), so concurrent requests cannot start a second turn.
pub fn spawn_turn(record: Arc<Record>, launch: Launch, prompt: String, max_secs: u64) {
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
    let program = exec::which_with(&argv[0], &agent.env).ok_or_else(|| {
        format!(
            "`{}` was not found on the bridge's PATH. Only the machine's owner can install it or set an absolute `command` for agent `{}`.",
            argv[0], agent.name
        )
    })?;
    let mut cmd = Command::new(program);
    scrub_env(&mut cmd);
    // The agent starts in the folder the session was authorized for, entered through a handle
    // checked against the folder's identity, so a path swapped meanwhile cannot redirect it.
    #[cfg(unix)]
    let folder = {
        let folder = std::fs::File::open(&record.cwd)
            .and_then(|f| f.metadata().map(|m| (f, m)))
            .ok()
            .filter(|(_, m)| m.is_dir() && crate::files::identity(m) == record.cwd_id)
            .map(|(f, _)| f)
            .ok_or_else(|| {
                format!(
                    "The session's folder {} was moved or replaced since the session started; start a new session in the folder you want.",
                    record.cwd.display()
                )
            })?;
        use std::os::fd::AsRawFd;
        let fd = folder.as_raw_fd();
        // SAFETY: fchdir is async-signal-safe, and `folder` stays open until after the spawn.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fchdir(fd) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        folder
    };
    #[cfg(not(unix))]
    cmd.current_dir(&record.cwd);
    cmd.args(&argv[1..])
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
    #[cfg(unix)]
    drop(folder);
    let pid = child.id();
    {
        // Publish the pid and check, under one lock, that the turn was not stopped while the
        // process was being started; `stop` can only kill a process it can see.
        let mut state = record.state();
        if state.phase != Phase::Working {
            drop(state);
            if let Some(pid) = pid {
                kill_tree(pid);
            }
            return Err("The turn was stopped.".into());
        }
        state.child_pid = pid;
    }

    if let Some(mut stdin) = child.stdin.take() {
        let prompt = prompt.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
        });
    }
    let stderr = child.stderr.take().expect("stderr is piped");
    let mut stderr_task = tokio::spawn(read_tail(stderr));
    // Lines are read in their own task and passed over a channel: receiving is cancel-safe, so
    // no partly read line is lost when the exit status arrives first.
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let (lines_tx, mut lines) = tokio::sync::mpsc::channel::<(String, bool)>(64);
    let reader = tokio::spawn(async move {
        let mut line = Vec::new();
        while let Ok(Some(cut)) = read_line(&mut stdout, &mut line).await {
            let line = String::from_utf8_lossy(&line).into_owned();
            if lines_tx.send((line, cut)).await.is_err() {
                break;
            }
        }
    });

    let mut outcome: Option<Result<(), String>> = None;
    let mut plain = String::new();
    // Output that had to be cut: an event cut in two cannot be parsed, so its answer is lost.
    let mut lost = false;
    let mut handle = |line: &str, cut: bool| match agent.kind {
        _ if cut => lost = true,
        AgentKind::Claude | AgentKind::Codex => {
            let events = if agent.kind == AgentKind::Claude {
                agents::parse_claude_line(line)
            } else {
                agents::parse_codex_line(line)
            };
            let mut state = record.state();
            for event in events {
                match event {
                    StreamEvent::SessionId(id) => state.agent_session_id = Some(id),
                    StreamEvent::Message(text) => {
                        state.last_message = Some(truncate(text, MAX_MESSAGE));
                        state.last_message_at = Some(SystemTime::now());
                    }
                    StreamEvent::Done => outcome = Some(Ok(())),
                    StreamEvent::Failed(message) => outcome = Some(Err(message)),
                }
            }
        }
        _ => {
            if plain.len() < MAX_PLAIN {
                plain.push_str(line);
                plain.push('\n');
            } else {
                lost = true;
            }
        }
    };
    let turn = async {
        let mut open = true;
        let status = loop {
            tokio::select! {
                line = lines.recv(), if open => match line {
                    Some((line, cut)) => handle(&line, cut),
                    None => open = false,
                },
                exited = child.wait() => break exited,
            }
        };
        // The agent has exited. Whatever it left running in its process group would outlive
        // the turn unnoticed (and hold its pipes open), so it is stopped too.
        #[cfg(unix)]
        if let Some(pid) = pid {
            terminate_tree(pid).await;
        }
        // The output may still be in flight, or a descendant that escaped the process group may
        // keep stdout open: take what arrives within a short grace period.
        if open {
            let drain = async {
                while let Some((line, cut)) = lines.recv().await {
                    handle(&line, cut);
                }
            };
            let _ = tokio::time::timeout(DRAIN, drain).await;
        }
        status
    };
    let finished = tokio::time::timeout(Duration::from_secs(max_secs), turn).await;
    reader.abort();
    let status = match finished {
        Ok(Ok(status)) => status,
        Ok(Err(err)) => {
            stderr_task.abort();
            return Err(format!("Waiting for the agent failed: {err}"));
        }
        Err(_) => {
            if let Some(pid) = pid {
                kill_tree(pid);
            }
            stderr_task.abort();
            return Err(format!(
                "The turn did not finish within {max_secs} s and was stopped."
            ));
        }
    };
    let stderr = match tokio::time::timeout(DRAIN, &mut stderr_task).await {
        Ok(Ok(text)) => text,
        _ => {
            stderr_task.abort();
            String::new()
        }
    };

    let cut = |result: Result<(), String>| match result {
        Ok(()) if lost => Err(format!(
            "The agent's output had a line longer than {} MiB, which the bridge cut, so its answer may be missing or incomplete. Ask the agent for a shorter answer, or to write the full text to a file you can read with GET /v1/files/content.",
            MAX_LINE >> 20
        )),
        other => other,
    };
    if agent.kind == AgentKind::Hermes {
        let (session_id, answer) = agents::parse_hermes_output(&plain, &stderr);
        let mut state = record.state();
        if session_id.is_some() {
            state.agent_session_id = session_id;
        }
        if !answer.is_empty() {
            state.last_message = Some(truncate(answer, MAX_MESSAGE));
            state.last_message_at = Some(SystemTime::now());
        }
        return cut(if status.success() {
            Ok(())
        } else {
            Err(failure(&argv[0], &status, &stderr))
        });
    }
    match outcome {
        Some(result) => cut(result),
        // Claude and Codex end every turn with a result event; exiting without one is a failure
        // even with exit code 0.
        None if status.success() => Err(format!(
            "`{}` exited without reporting a result. {}",
            argv[0],
            if stderr.trim().is_empty() {
                "It printed no error."
            } else {
                stderr.trim()
            }
        )),
        None => Err(failure(&argv[0], &status, &stderr)),
    }
}

fn failure(program: &str, status: &std::process::ExitStatus, stderr: &str) -> String {
    format!(
        "`{program}` exited with {status}: {}",
        if stderr.trim().is_empty() {
            "no error output"
        } else {
            stderr.trim()
        }
    )
}

/// Reads one line into `buf` (without the newline), keeping at most `MAX_LINE` bytes of it.
/// Returns `None` at the end of the stream, otherwise whether the line was cut.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    buf.clear();
    let mut seen = false;
    let mut cut = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(seen.then_some(cut));
        }
        seen = true;
        let (content, take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i, i + 1, true),
            None => (chunk.len(), chunk.len(), false),
        };
        let keep = content.min(MAX_LINE.saturating_sub(buf.len()));
        cut |= keep < content;
        buf.extend_from_slice(&chunk[..keep]);
        reader.consume(take);
        if done {
            return Ok(Some(cut));
        }
    }
}

/// Reads a stream to its end and keeps the last `STDERR_TAIL` bytes: errors and Hermes's
/// `session_id:` line come last.
async fn read_tail<R: AsyncRead + Unpin>(mut reader: R) -> String {
    let mut tail = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(n @ 1..) = reader.read(&mut chunk).await {
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > 2 * STDERR_TAIL {
            tail.drain(..tail.len() - STDERR_TAIL);
        }
    }
    let start = tail.len().saturating_sub(STDERR_TAIL);
    String::from_utf8_lossy(&tail[start..]).into_owned()
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[… truncated]");
    }
    text
}

/// Stops a turn's process group (Unix) or process tree (Windows) in the background.
pub fn kill_tree(pid: u32) {
    tokio::spawn(terminate_tree(pid));
}

/// Asks a turn's processes to stop, and kills them if they are still there after 3 s.
pub async fn terminate_tree(pid: u32) {
    #[cfg(unix)]
    {
        let pgid = pid as libc::pid_t;
        // SAFETY: killpg only sends a signal; signal 0 just checks whether the group exists.
        // An outdated pid fails harmlessly with ESRCH.
        if unsafe { libc::killpg(pgid, libc::SIGTERM) } != 0 {
            return;
        }
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            // SAFETY: as above.
            if unsafe { libc::killpg(pgid, 0) } != 0 {
                return;
            }
        }
        // SAFETY: as above.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stderr_keeps_its_real_end() {
        let data = format!("{}\nsession_id: 20261030_abc\n", "noise ".repeat(400_000));
        let tail = read_tail(data.as_bytes()).await;
        assert!(tail.len() <= STDERR_TAIL);
        assert!(tail.ends_with("session_id: 20261030_abc\n"));
    }

    #[tokio::test]
    async fn long_lines_are_capped_and_lines_split() {
        let data = format!("short\n{}\nlast", "x".repeat(MAX_LINE + 10));
        let mut reader = BufReader::new(data.as_bytes());
        let mut line = Vec::new();
        let cut = read_line(&mut reader, &mut line).await.unwrap();
        assert_eq!((cut, line.as_slice()), (Some(false), b"short".as_slice()));
        let cut = read_line(&mut reader, &mut line).await.unwrap();
        assert_eq!((cut, line.len()), (Some(true), MAX_LINE));
        let cut = read_line(&mut reader, &mut line).await.unwrap();
        assert_eq!((cut, line.as_slice()), (Some(false), b"last".as_slice()));
        assert_eq!(read_line(&mut reader, &mut line).await.unwrap(), None);
    }
}
