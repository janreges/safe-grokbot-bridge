//! Process inspection: which agent runs inside a tmux pane, and where agent processes run.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::config::Agent;

pub struct ProcInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    /// Unix time the process started.
    pub start_time: u64,
    pub name: String,
    pub cmd: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub environ: Vec<String>,
}

/// A point-in-time view of the process table.
#[derive(Default)]
pub struct Snapshot {
    procs: HashMap<u32, ProcInfo>,
    children: HashMap<u32, Vec<u32>>,
}

impl Snapshot {
    /// Reads the process table. `with_environ` also reads environment variables (slower).
    pub fn take(with_environ: bool) -> Snapshot {
        let Ok(mut system) = System::new() else {
            return Snapshot::default();
        };
        let environ = if with_environ {
            UpdateKind::Always
        } else {
            UpdateKind::Never
        };
        let kind = ProcessRefreshKind::nothing()
            .without_tasks()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_environ(environ);
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let mut snapshot = Snapshot::default();
        for (pid, process) in system.processes() {
            let pid = pid.as_u32();
            if let Some(parent) = process.parent() {
                snapshot
                    .children
                    .entry(parent.as_u32())
                    .or_default()
                    .push(pid);
            }
            snapshot.procs.insert(
                pid,
                ProcInfo {
                    pid,
                    parent: process.parent().map(|p| p.as_u32()),
                    start_time: process.start_time(),
                    name: process.name().to_string_lossy().into_owned(),
                    cmd: process
                        .cmd()
                        .iter()
                        .map(|a| a.to_string_lossy().into_owned())
                        .collect(),
                    cwd: process.cwd().map(Path::to_path_buf),
                    environ: process
                        .environ()
                        .iter()
                        .map(|v| v.to_string_lossy().into_owned())
                        .collect(),
                },
            );
        }
        snapshot
    }

    pub fn get(&self, pid: u32) -> Option<&ProcInfo> {
        self.procs.get(&pid)
    }

    pub fn alive(pid: u32) -> bool {
        let Ok(mut system) = System::new() else {
            return false;
        };
        let pids = [Pid::from_u32(pid)];
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().without_tasks(),
        ) > 0
    }

    pub fn processes(&self) -> impl Iterator<Item = &ProcInfo> {
        self.procs.values()
    }

    /// `pid` and its descendants, breadth first, up to `max_depth` levels down.
    pub fn tree(&self, pid: u32, max_depth: usize) -> Vec<&ProcInfo> {
        let mut out: Vec<&ProcInfo> = self.procs.get(&pid).into_iter().collect();
        let mut level = vec![pid];
        for _ in 0..max_depth {
            let next: Vec<u32> = level
                .iter()
                .flat_map(|p| self.children.get(p).into_iter().flatten().copied())
                .collect();
            out.extend(next.iter().filter_map(|p| self.procs.get(p)));
            if next.is_empty() {
                break;
            }
            level = next;
        }
        out
    }

    /// Whether text typed into a terminal whose program is `top` could reach a shell rather than
    /// the process `pid` below it: a process from `pid` up to `top` may read the terminal after
    /// its child exits (see [`may_read_terminal`]), or the chain between them is unknown.
    pub fn shell_between(&self, top: u32, pid: u32) -> bool {
        let mut current = pid;
        for _ in 0..16 {
            let Some(process) = self.procs.get(&current) else {
                return true;
            };
            if may_read_terminal(process) {
                return true;
            }
            if current == top {
                return false;
            }
            match process.parent {
                Some(parent) => current = parent,
                None => return true,
            }
        }
        true
    }

    /// The first configured agent found in the process tree under `pid` (e.g. a tmux pane's
    /// shell), with the pid of the matching process.
    pub fn find_agent<'a>(&self, pid: u32, agents: &'a [Agent]) -> Option<(&'a Agent, u32)> {
        for process in self.tree(pid, 4) {
            if let Some(agent) = agents.iter().find(|a| runs_command(process, &a.command)) {
                return Some((agent, process.pid));
            }
        }
        None
    }
}

/// Programs that run a script named by their first argument (`node /usr/bin/codex`).
const INTERPRETERS: &[&str] = &[
    "node", "nodejs", "bun", "deno", "python", "python3", "sh", "bash", "zsh", "dash",
];

/// Whether a process runs `command`: its executable is that program, or an interpreter runs it as
/// a script (`node /usr/bin/codex`, `bash /usr/bin/codex.mnt`). An argument of any other program
/// (`vim codex`) does not count.
pub fn runs_command(process: &ProcInfo, command: &str) -> bool {
    let wanted = program_name(command);
    if wanted.is_empty() {
        return false;
    }
    let executable = match process.cmd.first() {
        Some(first) => program_name(first),
        None => program_name(&process.name),
    };
    if executable == wanted || program_name(&process.name) == wanted {
        return true;
    }
    INTERPRETERS.contains(&executable.as_str())
        && process
            .cmd
            .iter()
            .skip(1)
            .find(|arg| !arg.starts_with('-'))
            .is_some_and(|script| program_name(script) == wanted)
}

const SHELLS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "mksh",
    "tcsh",
    "csh",
    "nu",
    "xonsh",
    "elvish",
    "pwsh",
    "powershell",
    "cmd",
];

/// Whether a process may read what is typed into its terminal after the program it started
/// exits: a shell that is interactive, runs a script, or runs more than one simple command
/// (`sh -c 'claude; exec bash'`). Only a shell running one simple command (`sh -c "claude"`,
/// `cmd /c codex`) ends with that program. Options that are not understood count as interactive.
pub fn may_read_terminal(process: &ProcInfo) -> bool {
    let first = process
        .cmd
        .first()
        .map(String::as_str)
        .unwrap_or(&process.name);
    let name = program_name(first);
    let name = name.trim_start_matches('-');
    if !SHELLS.contains(&name) {
        return false;
    }
    let args = process.cmd.get(1..).unwrap_or_default();
    let position = |options: &[&str]| {
        args.iter()
            .position(|arg| options.iter().any(|o| arg.eq_ignore_ascii_case(o)))
    };
    let command = match name {
        // cmd.exe takes the rest of its command line as the command.
        "cmd" => position(&["/c"]).map(|i| args[i + 1..].join(" ")),
        "pwsh" | "powershell" if position(&["-noexit"]).is_some() => None,
        "pwsh" | "powershell" => {
            position(&["-c", "-command"]).and_then(|i| args.get(i + 1).cloned())
        }
        // POSIX shells: the argument after `-c`, alone or in a cluster of short options (`-lc`).
        _ => args
            .iter()
            .position(|arg| {
                arg.len() > 1
                    && arg.starts_with('-')
                    && arg[1..].chars().all(|c| c.is_ascii_alphabetic())
                    && arg.contains('c')
            })
            .and_then(|i| args.get(i + 1).cloned()),
    };
    !command.is_some_and(|command| simple_command(&command))
}

/// One command without control operators, substitutions or redirections.
fn simple_command(command: &str) -> bool {
    let command = command.trim();
    !command.is_empty()
        && !command.starts_with('-')
        && !command.chars().any(|c| ";&|\n\r`$(){}<>^".contains(c))
}

/// File name without directory and without the extensions launchers add.
fn program_name(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .unwrap_or(OsStr::new(""))
        .to_string_lossy()
        .to_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".ps1", ".js", ".mjs", ".py"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem.to_string();
        }
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(name: &str, cmd: &[&str]) -> ProcInfo {
        ProcInfo {
            pid: 1,
            parent: None,
            start_time: 0,
            name: name.into(),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            environ: vec![],
        }
    }

    #[test]
    fn agent_processes_are_recognised_through_launchers() {
        assert!(runs_command(
            &proc("claude", &["claude", "--model", "opus"]),
            "claude"
        ));
        assert!(runs_command(
            &proc("MainThread", &["node", "/usr/bin/codex", "-m", "x"]),
            "codex"
        ));
        assert!(runs_command(
            &proc("codex", &["/opt/codex/bin/codex"]),
            "/usr/local/bin/codex"
        ));
        assert!(runs_command(
            &proc("claude.exe", &["C:\\bin\\claude.exe"]),
            "claude"
        ));
        assert!(!runs_command(
            &proc("codex.mnt", &["/bin/bash", "/usr/bin/codex.mnt"]),
            "codex"
        ));
        assert!(!runs_command(&proc("bash", &["bash"]), "claude"));
        // An editor opening a file named like an agent is not that agent.
        assert!(!runs_command(&proc("vim", &["vim", "codex"]), "codex"));
    }

    #[test]
    fn shells_that_end_with_their_program_are_told_apart() {
        let script = std::env::current_exe().unwrap().display().to_string();
        let reads = |cmd: &[&str]| may_read_terminal(&proc(cmd[0], cmd));
        // Interactive shells.
        assert!(reads(&["bash"]) && reads(&["-zsh"]) && reads(&["/bin/bash", "-l"]));
        assert!(reads(&["bash", "--rcfile", "x"]) && reads(&["pwsh", "-Interactive"]));
        assert!(reads(&["pwsh", "-NoExit", "-c", "x"]) && reads(&["cmd.exe", "/K", "codex"]));
        // Scripts and compound commands may go on after the agent exits.
        assert!(reads(&["/bin/bash", &script]) && reads(&["pwsh", "-File", "x.ps1"]));
        assert!(
            reads(&["sh", "-c", "claude; exec bash"]) && reads(&["sh", "-c", "claude && bash"])
        );
        // One simple command ends with it.
        assert!(!reads(&["sh", "-c", "claude --model opus"]) && !reads(&["bash", "-lc", "claude"]));
        assert!(!reads(&["pwsh", "-Command", "codex"]) && !reads(&["cmd.exe", "/C", "codex"]));
        assert!(!reads(&["claude", "--model", "opus"]));
    }

    #[test]
    fn snapshot_contains_this_process() {
        let snapshot = Snapshot::take(false);
        assert!(snapshot.get(std::process::id()).is_some());
        assert!(Snapshot::alive(std::process::id()));
    }
}
