//! A thin wrapper around the tmux command line (also works with the tmux-compatible `psmux`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::process::Command;

use crate::exec::{self, shell_quote};

const FORMAT: &str = "#{pane_id}\t#{pane_pid}\t#{session_name}\t#{session_created}\t#{window_activity}\t#{pane_in_mode}\t#{pane_dead}\t#{window_id}\t#{pane_synchronized}\t#{pane_current_command}\t#{pane_current_path}";
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

/// Variables that select an agent's profile. The bridge resolves profiles with its own
/// environment (to read conversations and check `resume`), so a session takes them from the
/// bridge, never from the tmux server.
pub const PROFILE_VARS: &[&str] = &["CLAUDE_CONFIG_DIR", "CODEX_HOME", "HERMES_HOME"];

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
    /// Unix time the session was created.
    pub created: u64,
    /// Unix time of the window's last output.
    pub activity: u64,
    /// The pane shows copy mode or another mode, so typed keys would not reach the program.
    pub in_mode: bool,
    pub dead: bool,
    /// tmux window id, e.g. `@3`.
    pub window: String,
    /// `synchronize-panes` is on: keys sent to the pane also reach the window's other panes.
    pub synchronized: bool,
    pub command: String,
    pub cwd: PathBuf,
}

fn parse_pane(line: &str) -> Option<Pane> {
    let mut parts = line.splitn(11, '\t');
    Some(Pane {
        id: parts.next()?.to_string(),
        pid: parts.next()?.parse().ok()?,
        session: parts.next()?.to_string(),
        created: parts.next()?.parse().unwrap_or(0),
        activity: parts.next()?.parse().unwrap_or(0),
        in_mode: parts.next()? == "1",
        dead: parts.next()? == "1",
        window: parts.next()?.to_string(),
        synchronized: parts.next()? == "1",
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
        let session = shell_quote(session);
        // PowerShell runs a quoted program only with the call operator.
        let bin = match shell_quote(&self.bin) {
            quoted if cfg!(windows) && quoted.starts_with('\'') => format!("& {quoted}"),
            bin => bin,
        };
        match &self.socket {
            Some(socket) => format!("{bin} -L {} attach -t {session}", shell_quote(socket)),
            None => format!("{bin} attach -t {session}"),
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
        // tmux expands formats in the start directory, and `#(…)` there runs a shell command. A
        // trailing `;` would end the tmux command there.
        let cwd = escape_separator(&cwd.display().to_string().replace('#', "##"));
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
        args.extend(pane_command(env, argv));
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
        // psmux keeps buffers per session server, so the buffer must be loaded where it is used.
        let mut load = vec!["load-buffer", "-b", &buffer];
        if cfg!(windows) {
            load.extend(["-t", target]);
        }
        load.push("-");
        self.run(&load, Some(text.as_bytes().to_vec())).await?;
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
        let text = escape_separator(text);
        self.run(&["send-keys", "-t", target, "-l", "--", &text], None)
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

/// What follows the options of `new-session`: the pane's command and its environment. A running
/// tmux server passes its own environment to new sessions, and that environment may come from an
/// agent session (`CLAUDECODE` makes Claude Code refuse to start) or name another profile. So the
/// profile variables are set to the bridge's values and the rest are removed when the pane
/// starts. On Unix this goes through `env`, which also works with tmux before 3.2 (no `-e`).
fn pane_command(env: &[(String, String)], argv: &[String]) -> Vec<String> {
    let mut env = env.to_vec();
    let mut unset: Vec<&str> = Vec::new();
    for var in PROFILE_VARS {
        if env.iter().any(|(key, _)| key == var) {
            continue;
        }
        match std::env::var(var) {
            Ok(value) => env.push((var.to_string(), value)),
            Err(_) => unset.push(var),
        }
    }
    // tmux sets `TMUX` and `TMUX_PANE` for the new pane itself.
    unset.extend(
        INHERITED_SESSION_VARS
            .iter()
            .filter(|var| !var.starts_with("TMUX")),
    );
    let mut args = Vec::new();
    if cfg!(unix) {
        args.extend(["--".to_string(), "/usr/bin/env".to_string()]);
        for var in unset {
            args.extend(["-u".to_string(), var.to_string()]);
        }
        args.extend(env.iter().map(|(key, value)| format!("{key}={value}")));
    } else {
        for (key, value) in env {
            args.extend(["-e".to_string(), format!("{key}={value}")]);
        }
        args.push("--".into());
    }
    args.extend(argv.iter().map(|arg| escape_separator(arg)));
    args
}

/// tmux reads a command-line argument that ends in `;` as a command separator and drops the
/// `;`, unless it is escaped as `\;`.
fn escape_separator(arg: &str) -> String {
    match arg.strip_suffix(';') {
        Some(rest) => format!("{rest}\\;"),
        None => arg.to_string(),
    }
}

/// A missing or dead server, which has no panes. A socket that cannot be reached for another
/// reason (permissions) is an error: reading it as "no panes" would make every session look gone.
fn no_server(err: &anyhow::Error) -> bool {
    let text = err.to_string();
    text.contains("no server running")
        || text.contains("No such file or directory")
        || text.contains("Connection refused")
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
            "%3\t4242\tsgb-app-claude-1a2b3c\t1793315500\t1793315506\t0\t0\t@2\t1\tclaude\t/home/me/my app",
        )
        .unwrap();
        assert_eq!(
            (pane.id.as_str(), pane.pid, pane.created, pane.activity),
            ("%3", 4242, 1793315500, 1793315506)
        );
        assert_eq!(pane.cwd, PathBuf::from("/home/me/my app"));
        assert!(!pane.in_mode && !pane.dead);
        assert_eq!((pane.window.as_str(), pane.synchronized), ("@2", true));
        assert!(parse_pane("garbage").is_none());
    }

    #[test]
    fn pane_commands_keep_semicolons_and_drop_inherited_sessions() {
        assert_eq!(escape_separator("fix it;"), "fix it\\;");
        assert_eq!(escape_separator(";"), "\\;");
        assert_eq!(escape_separator("a;b"), "a;b");
        let env = [("CODEX_HOME".to_string(), "/p".to_string())];
        let argv = ["/bin/codex".to_string(), "go;".to_string()];
        let args = pane_command(&env, &argv);
        assert_eq!(args[args.len() - 2..], ["/bin/codex", "go\\;"]);
        if cfg!(unix) {
            assert_eq!(args[..2], ["--", "/usr/bin/env"]);
            assert!(args.windows(2).any(|w| w == ["-u", "CLAUDECODE"]));
            assert!(args.contains(&"CODEX_HOME=/p".to_string()));
            assert!(!args.iter().any(|a| a == "TMUX"));
        } else {
            assert!(args.windows(2).any(|w| w == ["-e", "CODEX_HOME=/p"]));
        }
    }
}
