//! Read-only git commands with fixed, hardened options.

use std::path::{Component, Path};
use std::time::Duration;

use axum::http::StatusCode;
use serde::Serialize;
use tokio::process::Command;
use utoipa::ToSchema;

use crate::error::ApiError;
use crate::exec;

const MAX_OUTPUT: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_LOG: usize = 500;

pub enum GitRequest {
    Status,
    Log {
        limit: usize,
        rev: Option<String>,
        path: Option<String>,
    },
    Diff {
        rev: Option<String>,
        staged: bool,
        path: Option<String>,
    },
    Show {
        rev: Option<String>,
        path: Option<String>,
    },
}

/// Output of a git command.
#[derive(Serialize, ToSchema, Debug)]
pub struct GitOutput {
    /// Directory the command ran in.
    pub dir: String,
    /// The git command that ran, for reference.
    pub command: String,
    /// Raw git output. `log` lines are `<hash>\t<ISO date>\t<author>\t<subject>`.
    pub output: String,
    /// `true` when the output was cut at 256 KiB; `note` says how to narrow it.
    pub truncated: bool,
    /// Explains empty or truncated output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Accepts revision expressions such as `HEAD~2`, `main..feature` or `v1.0^{commit}`, and refuses
/// anything git could read as an option or a `<rev>:<path>` blob reference.
pub fn validate_rev(rev: &str) -> Result<(), ApiError> {
    let ok = !rev.is_empty()
        && rev.len() <= 200
        && !rev.starts_with('-')
        && rev
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/~^@{}+-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "`{rev}` is not an accepted revision. Use a commit hash, branch, tag or expressions like `HEAD~2` and `main..feature`."
        )))
    }
}

fn validate_path(path: &str) -> Result<(), ApiError> {
    let p = Path::new(path);
    let ok = !path.is_empty()
        && path.len() <= 1000
        && !p.is_absolute()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "`{path}` is not accepted as a git path filter. Use a path relative to `dir`, without `..`."
        )))
    }
}

pub async fn run(dir: &Path, request: GitRequest) -> Result<GitOutput, ApiError> {
    let mut args: Vec<String> = Vec::new();
    let mut tail: Vec<String> = Vec::new();
    let push_rev = |tail: &mut Vec<String>, rev: &Option<String>| -> Result<(), ApiError> {
        if let Some(rev) = rev {
            validate_rev(rev)?;
            tail.push("--end-of-options".into());
            tail.push(rev.clone());
        }
        Ok(())
    };
    let path_filter = |path: &Option<String>| -> Result<Vec<String>, ApiError> {
        match path {
            Some(path) => {
                validate_path(path)?;
                Ok(vec!["--".into(), path.clone()])
            }
            None => Ok(vec![]),
        }
    };
    let (empty_note, narrow) = match &request {
        GitRequest::Status => {
            args.extend(
                ["status", "--short", "--branch", "--ignore-submodules=all"].map(String::from),
            );
            (
                "",
                "This endpoint has no narrowing options; inspect parts of the change with /v1/git/diff and `path`.",
            )
        }
        GitRequest::Log { limit, rev, path } => {
            args.extend([
                "log".to_string(),
                format!("--max-count={}", (*limit).clamp(1, MAX_LOG)),
                "--date=iso-strict".into(),
                "--format=%H%x09%ad%x09%an%x09%s".into(),
            ]);
            push_rev(&mut tail, rev)?;
            tail.extend(path_filter(path)?);
            (
                "No commits match.",
                "Narrow it with a smaller `limit`, a `rev` range or a `path`.",
            )
        }
        GitRequest::Diff { rev, staged, path } => {
            args.extend(
                [
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--ignore-submodules=all",
                    "--stat",
                    "--patch",
                ]
                .map(String::from),
            );
            if *staged {
                args.push("--cached".into());
            }
            push_rev(&mut tail, rev)?;
            tail.extend(path_filter(path)?);
            (
                if *staged || rev.is_some() {
                    "No differences."
                } else {
                    "No unstaged changes (the working tree matches the index). Staged changes are shown with `staged=true`."
                },
                "Narrow it with `path`.",
            )
        }
        GitRequest::Show { rev, path } => {
            args.extend(
                [
                    "show",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--stat",
                    "--patch",
                    "--format=fuller",
                ]
                .map(String::from),
            );
            push_rev(
                &mut tail,
                &Some(rev.clone().unwrap_or_else(|| "HEAD".into())),
            )?;
            tail.extend(path_filter(path)?);
            ("", "Narrow it with `path`.")
        }
    };
    args.extend(tail);

    let command = format!("git {}", args.join(" "));
    let filters = filter_overrides(dir).await;
    let out = git(dir, &filters, &args, MAX_OUTPUT).await?;
    if !out.success() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "git_failed",
            format!(
                "`{command}` failed in {}: {}",
                dir.display(),
                out.stderr.trim()
            ),
        ));
    }
    let note = if out.truncated {
        Some(format!("Output was cut at 256 KiB. {narrow}"))
    } else if out.stdout.trim().is_empty() && !empty_note.is_empty() {
        Some(empty_note.to_string())
    } else {
        None
    };
    Ok(GitOutput {
        dir: dir.display().to_string(),
        command,
        output: out.stdout,
        truncated: out.truncated,
        note,
    })
}

/// Runs git with options that keep a repository's own configuration from running programs or
/// prompting: no fsmonitor, no pager, no signature verification, literal pathspecs, and (for the
/// commands that use them) no external diff drivers, textconv or filters. `extra` comes first so
/// it can override configuration.
async fn git(
    dir: &Path,
    extra: &[String],
    args: &[String],
    max: usize,
) -> Result<exec::Output, ApiError> {
    let mut cmd = Command::new("git");
    cmd.args(["--no-pager", "--literal-pathspecs"])
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotepath=off"])
        .args(["-c", "color.ui=never", "-c", "log.showSignature=false"])
        .args(extra)
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    exec::run(cmd, None, TIMEOUT, max).await.map_err(|err| {
        ApiError::unavailable(format!(
            "Could not run git: {err:#}. This affects every git endpoint on this machine; retrying will not help until git works there."
        ))
    })
}

/// `-c filter.<name>.{clean,smudge,process}=` for every filter driver the configuration defines.
/// Git runs clean filters while comparing working-tree files (`status`, `diff`), so a driver in
/// a repository's config would otherwise execute on a read-only request. Reading the
/// configuration runs nothing.
async fn filter_overrides(dir: &Path) -> Vec<String> {
    let args = ["config", "--get-regexp", r"^filter\."].map(String::from);
    let Ok(out) = git(dir, &[], &args, 256 * 1024).await else {
        return Vec::new();
    };
    let mut names: Vec<&str> = out
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|key| key.strip_prefix("filter."))
        .filter_map(|rest| rest.rsplit_once('.').map(|(name, _)| name))
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .flat_map(|name| {
            ["clean", "smudge", "process"]
                .into_iter()
                .flat_map(move |what| ["-c".to_string(), format!("filter.{name}.{what}=")])
        })
        .collect()
}

/// The root of the repository that contains `dir`, canonicalized.
pub async fn toplevel(dir: &Path) -> Result<std::path::PathBuf, ApiError> {
    let args = ["rev-parse", "--show-toplevel"].map(String::from);
    let out = git(dir, &[], &args, 64 * 1024).await?;
    if !out.success() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "git_failed",
            format!(
                "`{}` is not inside a git repository: {}",
                dir.display(),
                out.stderr.trim()
            ),
        ));
    }
    dunce::canonicalize(out.stdout.trim()).map_err(ApiError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_are_validated() {
        for ok in [
            "HEAD",
            "HEAD~2",
            "main..feature",
            "v1.0^{commit}",
            "a1b2c3d",
            "origin/main",
            "@{u}",
        ] {
            assert!(validate_rev(ok).is_ok(), "{ok}");
        }
        for bad in [
            "--output=/tmp/x",
            "-p",
            "HEAD:secret.txt",
            "a b",
            "",
            "$(id)",
        ] {
            assert!(validate_rev(bad).is_err(), "{bad}");
        }
        assert!(validate_path("src/main.rs").is_ok());
        assert!(validate_path("../x").is_err() && validate_path("/etc/passwd").is_err());
    }

    #[tokio::test]
    async fn log_and_status_work_on_a_repository() {
        if exec::which("git").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(status.status.success(), "{status:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "Add greeting"]);
        std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();

        let log = run(
            dir.path(),
            GitRequest::Log {
                limit: 5,
                rev: None,
                path: None,
            },
        )
        .await
        .unwrap();
        assert!(log.output.contains("\tT\tAdd greeting"), "{}", log.output);
        let status = run(dir.path(), GitRequest::Status).await.unwrap();
        assert!(status.output.contains(" M a.txt"), "{}", status.output);
        let diff = run(
            dir.path(),
            GitRequest::Diff {
                rev: None,
                staged: false,
                path: Some("a.txt".into()),
            },
        )
        .await
        .unwrap();
        assert!(diff.output.contains("+hello world"));
        // A clean filter from the repository's own config must not run on a read request.
        let marker = dir.path().join("PWNED");
        std::fs::write(dir.path().join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        git(&[
            "config",
            "filter.evil.clean",
            &format!("sh -c 'touch {}; cat'", marker.display()),
        ]);
        std::fs::write(dir.path().join("a.txt"), "changed again\n").unwrap();
        run(dir.path(), GitRequest::Status).await.unwrap();
        run(
            dir.path(),
            GitRequest::Diff {
                rev: None,
                staged: false,
                path: None,
            },
        )
        .await
        .unwrap();
        assert!(!marker.exists(), "a repository filter ran");

        let err = run(
            dir.path(),
            GitRequest::Show {
                rev: Some("nope".into()),
                path: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "git_failed");
    }
}
