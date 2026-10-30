//! Process inspection: which agent runs inside a tmux pane, and where agent processes run.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::config::Agent;

pub struct ProcInfo {
    pub pid: u32,
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

/// Whether a process runs `command`: its name, or its first or second argument (for
/// interpreters such as `node /usr/bin/codex`), names the same program.
pub fn runs_command(process: &ProcInfo, command: &str) -> bool {
    let wanted = program_name(command);
    !wanted.is_empty()
        && (program_name(&process.name) == wanted
            || process
                .cmd
                .iter()
                .take(2)
                .any(|arg| program_name(arg) == wanted))
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
    }

    #[test]
    fn snapshot_contains_this_process() {
        let snapshot = Snapshot::take(false);
        assert!(snapshot.get(std::process::id()).is_some());
        assert!(Snapshot::alive(std::process::id()));
    }
}
