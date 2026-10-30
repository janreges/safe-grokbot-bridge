//! Running external programs: always with an argument vector (never a shell), a timeout and
//! bounded output.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// `stdout` was cut at `max_bytes`.
    pub truncated: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Quotes a value for a shell command line shown to people (POSIX shells and PowerShell read a
/// single-quoted value literally).
pub fn shell_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-=:@%+,".contains(c));
    if plain {
        value.to_string()
    } else if cfg!(windows) {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Runs `cmd` to completion. The process is killed when the timeout expires.
pub async fn run(
    mut cmd: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    max_bytes: usize,
) -> Result<Output> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot start `{program}`"))?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = pipe.write_all(&data).await;
        });
    }
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let work = async {
        let (out, err) = tokio::join!(
            read_capped(stdout, max_bytes),
            read_capped(stderr, 64 * 1024)
        );
        let status = child.wait().await?;
        anyhow::Ok((status, out?, err?))
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(result) => {
            let (status, (stdout, truncated), (stderr, _)) = result?;
            Ok(Output {
                code: status.code(),
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                truncated,
            })
        }
        // Dropping `child` on return kills it (`kill_on_drop`).
        Err(_) => bail!("`{program}` did not finish within {} s", timeout.as_secs()),
    }
}

/// Reads a stream to the end, keeping at most `max` bytes.
pub async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    max: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok((buf, truncated));
        }
        let take = n.min(max.saturating_sub(buf.len()));
        buf.extend_from_slice(&chunk[..take]);
        truncated |= take < n;
    }
}

/// Finds a program the way a shell would: a path with a separator is used as is, a bare name is
/// looked up on `PATH` (with `PATHEXT` extensions on Windows).
pub fn which(program: &str) -> Option<PathBuf> {
    find_in(program, std::env::var_os("PATH").as_deref())
}

/// Like [`which`], but searches the `PATH` that `env` (an agent's configured environment) sets.
pub fn which_with(program: &str, env: &[(String, String)]) -> Option<PathBuf> {
    // Windows treats environment names case-insensitively (`Path`).
    let is_path = |key: &str| {
        if cfg!(windows) {
            key.eq_ignore_ascii_case("PATH")
        } else {
            key == "PATH"
        }
    };
    match env.iter().rev().find(|(key, _)| is_path(key)) {
        Some((_, path)) => find_in(program, Some(std::ffi::OsStr::new(path))),
        None => which(program),
    }
}

fn find_in(program: &str, search: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return is_executable(path).then(|| path.to_path_buf());
    }
    let search = search?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        Vec::new()
    };
    std::env::split_paths(search).find_map(|dir| {
        let candidate = dir.join(program);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        extensions
            .iter()
            .map(|ext| dir.join(format!("{program}{ext}")))
            .find(|candidate| candidate.is_file())
    })
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn explicit_paths_need_no_search_path() {
        assert_eq!(find_in("/bin/sh", None), Some(PathBuf::from("/bin/sh")));
        assert_eq!(find_in("sh", None), None);
        let dirs = std::ffi::OsString::from("/nonexistent:/bin");
        assert_eq!(find_in("sh", Some(&dirs)), Some(PathBuf::from("/bin/sh")));
    }
}
