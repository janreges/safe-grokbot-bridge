//! A thin wrapper around the tmux command line (also works with the tmux-compatible `psmux`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::process::Command;

use crate::exec;

const FORMAT: &str = "#{pane_id}\t#{pane_pid}\t#{session_name}\t#{window_activity}\t#{pane_in_mode}\t#{pane_dead}\t#{pane_current_command}\t#{pane_current_path}";
const TIMEOUT: Duration = Duration::from_secs(10);

/// Environment variables that identify the process the bridge itself may run in (a Claude Code
/// session, a tmux pane). Agents started by the bridge must not inherit them: Claude Code, for
/// one, refuses to start inside another Claude Code session.
pub const INHERITED_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SSE_PORT",
    "CODEX_THREAD_ID",
    "TMUX",
    "TMUX_PANE",
];

pub fn scrub_env(cmd: &mut Command) {
    for var in INHERITED_SESSION_VARS {
        cmd.env_remove(var);
    }
}

#[derive(Debug, Clone)]
pub struct Pane {
    /// tmux pane id, e.g. `%12`.
    pub id: String,
    pub pid: u32,
    pub session: String,
    /// Unix time of the window's last output.
    pub activity: u64,
    /// The pane shows copy mode or another mode, so typed keys would not reach the program.
    pub in_mode: bool,
    pub dead: bool,
    pub command: String,
    pub cwd: PathBuf,
}

fn parse_pane(line: &str) -> Option<Pane> {
    let mut parts = line.splitn(8, '\t');
    Some(Pane {
        id: parts.next()?.to_string(),
        pid: parts.next()?.parse().ok()?,
        session: parts.next()?.to_string(),
        activity: parts.next()?.parse().unwrap_or(0),
        in_mode: parts.next()? == "1",
        dead: parts.next()? == "1",
        command: parts.next()?.to_string(),
        cwd: PathBuf::from(parts.next()?),
    })
}

#[derive(Clone)]
pub struct Tmux {
    bin: String,
    socket: Option<String>,
}

impl Tmux {
    pub fn new(bin: &str, socket: Option<&str>) -> Self {
        Self {
            bin: bin.to_string(),
            socket: socket.map(str::to_string),
        }
    }

    /// Arguments that select the server, for commands shown to people (`tmux -L x attach …`).
    pub fn attach_command(&self, session: &str) -> String {
        match &self.socket {
            Some(socket) => format!("{} -L {socket} attach -t {session}", self.bin),
            None => format!("{} attach -t {session}", self.bin),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        scrub_env(&mut cmd);
        if let Some(socket) = &self.socket {
            cmd.args(["-L", socket]);
        }
        cmd
    }

    async fn run(&self, args: &[&str], stdin: Option<Vec<u8>>) -> Result<String> {
        let mut cmd = self.command();
        cmd.args(args);
        let out = exec::run(cmd, stdin, TIMEOUT, 8 * 1024 * 1024).await?;
        if !out.success() {
            bail!(
                "tmux {} failed: {}",
                args.first().unwrap_or(&""),
                out.stderr.trim()
            );
        }
        Ok(out.stdout)
    }

    /// The version string, or `None` when tmux cannot be run.
    pub async fn version(&self) -> Option<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-V");
        let out = exec::run(cmd, None, TIMEOUT, 4096).await.ok()?;
        out.success().then(|| out.stdout.trim().to_string())
    }

    /// All panes of the server. No running server means no panes.
    pub async fn list_panes(&self) -> Result<Vec<Pane>> {
        match self.run(&["list-panes", "-a", "-F", FORMAT], None).await {
            Ok(out) => Ok(out.lines().filter_map(parse_pane).collect()),
            Err(err) if no_server(&err) => Ok(Vec::new()),
            Err(err) => Err(err),
        }
    }

    /// Starts a detached session running `argv` and returns its pane. Session names must be
    /// unique; the caller generates them.
    pub async fn new_session(
        &self,
        name: &str,
        cwd: &Path,
        env: &[(String, String)],
        argv: &[String],
    ) -> Result<Pane> {
        let cwd = cwd.display().to_string();
        let mut args: Vec<String> = [
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            &cwd,
            "-x",
            "200",
            "-y",
            "50",
            "-P",
            "-F",
            FORMAT,
        ]
        .map(String::from)
        .to_vec();
        for (key, value) in env {
            args.extend(["-e".to_string(), format!("{key}={value}")]);
        }
        args.push("--".into());
        args.extend(argv.iter().cloned());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run(&args, None).await?;
        out.lines()
            .find_map(parse_pane)
            .ok_or_else(|| anyhow::anyhow!("tmux did not report the new pane"))
    }

    /// Pastes text as a bracketed paste, so multi-line text arrives as one block instead of
    /// one Enter per line.
    pub async fn paste(&self, target: &str, text: &str) -> Result<()> {
        let buffer = format!("sgb-{}", uuid::Uuid::new_v4().simple());
        self.run(
            &["load-buffer", "-b", &buffer, "-"],
            Some(text.as_bytes().to_vec()),
        )
        .await?;
        self.run(
            &["paste-buffer", "-p", "-d", "-b", &buffer, "-t", target],
            None,
        )
        .await?;
        Ok(())
    }

    /// Sends tmux key names (`Enter`, `Escape`, `C-c`, …).
    pub async fn send_keys(&self, target: &str, keys: &[&str]) -> Result<()> {
        let mut args = vec!["send-keys", "-t", target];
        args.extend(keys);
        self.run(&args, None).await.map(drop)
    }

    /// Types characters literally (no key-name interpretation).
    pub async fn send_literal(&self, target: &str, text: &str) -> Result<()> {
        self.run(&["send-keys", "-t", target, "-l", "--", text], None)
            .await
            .map(drop)
    }

    /// Leaves copy mode (or any other pane mode), so input reaches the program again.
    pub async fn cancel_mode(&self, target: &str) -> Result<()> {
        self.run(&["send-keys", "-t", target, "-X", "cancel"], None)
            .await
            .map(drop)
    }

    /// The last `lines` lines of the pane, including scrollback.
    pub async fn capture(&self, target: &str, lines: usize) -> Result<String> {
        let start = format!("-{lines}");
        let out = self
            .run(
                &["capture-pane", "-p", "-J", "-t", target, "-S", &start],
                None,
            )
            .await?;
        Ok(out.trim_end().to_string())
    }

    pub async fn kill_session(&self, name: &str) -> Result<()> {
        self.run(&["kill-session", "-t", &format!("={name}")], None)
            .await
            .map(drop)
    }
}

fn no_server(err: &anyhow::Error) -> bool {
    let text = err.to_string();
    text.contains("no server running")
        || text.contains("error connecting to")
        || text.contains("No such file")
}

/// tmux target for a bridge-started session: its (only) pane, matched by exact session name.
pub fn session_target(name: &str) -> String {
    format!("={name}:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_lines_are_parsed() {
        let pane = parse_pane(
            "%3\t4242\tsgb-app-claude-1a2b3c\t1793315506\t0\t0\tclaude\t/home/me/my app",
        )
        .unwrap();
        assert_eq!(
            (pane.id.as_str(), pane.pid, pane.activity),
            ("%3", 4242, 1793315506)
        );
        assert_eq!(pane.cwd, PathBuf::from("/home/me/my app"));
        assert!(!pane.in_mode && !pane.dead);
        assert!(parse_pane("garbage").is_none());
    }
}
