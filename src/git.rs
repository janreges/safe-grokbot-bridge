//! Read-only git commands with fixed, hardened options.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
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
    let filters = filter_overrides(dir).await?;
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
/// prompting: no fsmonitor, no pager, no signature verification, no submodule diffs, literal
/// pathspecs, and (for the commands that use them) no external diff drivers, textconv or filters.
/// All network and local transports are disabled, so a missing object in a partial clone fails
/// instead of fetching it through a transport the repository configures (`core.sshCommand`).
/// `GIT_*` variables of the bridge's own environment are dropped, so they cannot point git at
/// another repository. `extra` comes first so it can override configuration.
async fn git(
    dir: &Path,
    extra: &[String],
    args: &[String],
    max: usize,
) -> Result<exec::Output, ApiError> {
    let mut cmd = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            cmd.env_remove(name);
        }
    }
    cmd.args(["--no-pager", "--literal-pathspecs"])
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotepath=off"])
        .args(["-c", "color.ui=never", "-c", "log.showSignature=false"])
        .args([
            "-c",
            "diff.submodule=short",
            "-c",
            "status.submoduleSummary=false",
        ])
        .args(extra)
        .args(args)
        .current_dir(dir)
        // An empty allow list overrides every `protocol.*.allow` setting.
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_LAZY_FETCH", "1")
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
/// configuration runs nothing. Fails when the configuration cannot be read completely, because a
/// driver that was missed would run.
async fn filter_overrides(dir: &Path) -> Result<Vec<String>, ApiError> {
    let args = ["config", "--name-only", "--get-regexp", r"^filter\."].map(String::from);
    let out = git(dir, &[], &args, 256 * 1024).await?;
    // Exit code 1 means that no key matched.
    let complete = out.success() || (out.code == Some(1) && out.stdout.is_empty());
    let mut names: Vec<&str> = out
        .stdout
        .lines()
        .filter_map(|key| key.strip_prefix("filter."))
        .filter_map(|rest| rest.rsplit_once('.').map(|(name, _)| name))
        .collect();
    names.sort_unstable();
    names.dedup();
    if !complete || out.truncated || names.len() > 100 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "git_failed",
            format!(
                "Could not verify the filter drivers configured for the repository at {} ({}), so git is not run there. Check its configuration with `git config --get-regexp '^filter\\.'`.",
                dir.display(),
                if out.truncated || names.len() > 100 {
                    "too many".to_string()
                } else {
                    out.stderr.trim().to_string()
                }
            ),
        ));
    }
    Ok(names
        .into_iter()
        .flat_map(|name| {
            ["clean=", "smudge=", "process=", "required=false"]
                .into_iter()
                .flat_map(move |what| ["-c".to_string(), format!("filter.{name}.{what}")])
        })
        .collect())
}

/// A repository's working tree and the places its history is read from, all canonical.
pub struct Repository {
    pub toplevel: PathBuf,
    /// The git directory, the common directory of linked worktrees, the object store and its
    /// alternates. A `.git` file or an alternates entry can point any of them elsewhere.
    pub storage: Vec<PathBuf>,
}

/// Finds the repository that contains `dir`.
pub async fn repository(dir: &Path) -> Result<Repository, ApiError> {
    let args = [
        "rev-parse",
        "--show-toplevel",
        "--absolute-git-dir",
        "--git-common-dir",
    ]
    .map(String::from);
    let out = git(dir, &[], &args, 64 * 1024).await?;
    if !out.success() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "git_failed",
            format!(
                "`{}` is not inside a git working tree: {}",
                dir.display(),
                out.stderr.trim()
            ),
        ));
    }
    let canonical = |path: &Path| dunce::canonicalize(path).map_err(ApiError::internal);
    let mut lines = out.stdout.lines();
    let (Some(toplevel), Some(git_dir), Some(common)) = (lines.next(), lines.next(), lines.next())
    else {
        return Err(ApiError::internal(format!(
            "unexpected `git rev-parse` output: {}",
            out.stdout
        )));
    };
    let toplevel = canonical(Path::new(toplevel))?;
    // `--git-common-dir` may be relative to `dir`.
    let common = canonical(&dir.join(common))?;
    let mut storage = vec![canonical(Path::new(git_dir))?, common.clone()];
    let mut pending = vec![common.join("objects")];
    while let Some(objects) = pending.pop() {
        // Git skips alternates that do not exist.
        let Ok(objects) = dunce::canonicalize(&objects) else {
            continue;
        };
        if storage.contains(&objects) {
            continue;
        }
        if storage.len() > 16 {
            return Err(ApiError::forbidden(format!(
                "The repository at {} chains too many object alternates to verify them, so git access is refused.",
                toplevel.display()
            )));
        }
        for line in read_alternates(&objects.join("info").join("alternates")).lines() {
            let line = line.trim();
            if line.starts_with('"') {
                return Err(ApiError::forbidden(format!(
                    "The repository at {} uses a quoted object alternate, which the bridge cannot verify, so git access is refused.",
                    toplevel.display()
                )));
            }
            if !line.is_empty() && !line.starts_with('#') {
                pending.push(objects.join(line));
            }
        }
        storage.push(objects);
    }
    Ok(Repository { toplevel, storage })
}

fn read_alternates(path: &Path) -> String {
    let mut text = String::new();
    if path.is_file()
        && let Ok(file) = std::fs::File::open(path)
    {
        let _ = file.take(64 * 1024).read_to_string(&mut text);
    }
    text
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

    /// Settings a repository's own configuration controls must neither run programs nor point
    /// git at data outside the repository unnoticed.
    #[cfg(unix)]
    #[tokio::test]
    async fn repository_configuration_cannot_run_programs() {
        use std::io::Write;
        if exec::which("git").is_none() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let marker = root.join("PWNED");
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
                .args(["-c", "protocol.file.allow=always"])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let src = root.join("src");
        std::fs::create_dir(&src).unwrap();
        git(&src, &["init", "-q"]);
        std::fs::write(src.join("a.txt"), "a\n").unwrap();
        git(&src, &["add", "a.txt"]);
        git(&src, &["commit", "-qm", "one"]);
        git(&src, &["config", "uploadpack.allowFilter", "true"]);

        // A missing object in a partial clone must not be fetched through the remote's
        // configured ssh command.
        let url = format!("file://{}", src.display());
        git(
            &root,
            &[
                "clone",
                "-q",
                "--no-checkout",
                "--filter=blob:none",
                &url,
                "partial",
            ],
        );
        let partial = root.join("partial");
        git(
            &partial,
            &["config", "remote.origin.url", "ssh://example.invalid/r"],
        );
        let touch = format!("touch {}", marker.display());
        git(&partial, &["config", "core.sshCommand", &touch]);
        let blob = git(&src, &["rev-parse", "HEAD:a.txt"]);
        let show = |rev: Option<String>| GitRequest::Show { rev, path: None };
        let err = run(&partial, show(Some(blob))).await.unwrap_err();
        assert_eq!(err.code, "git_failed");
        assert!(
            !marker.exists(),
            "a lazy fetch ran the configured ssh command"
        );

        // `diff.submodule=diff` must not run a submodule's own diff driver.
        git(&root, &["init", "-q", "parent"]);
        let parent = root.join("parent");
        git(&parent, &["submodule", "add", "-q", &url, "sub"]);
        git(&parent, &["commit", "-qm", "add sub"]);
        let sub = parent.join("sub");
        std::fs::write(sub.join(".gitattributes"), "*.txt diff=evil\n").unwrap();
        std::fs::write(sub.join("a.txt"), "b\n").unwrap();
        git(&sub, &["add", "-A"]);
        git(&sub, &["commit", "-qm", "two"]);
        let textconv = format!("sh -c 'touch {}; cat \"$1\"' -", marker.display());
        git(&sub, &["config", "diff.evil.textconv", &textconv]);
        git(&parent, &["add", "sub"]);
        git(&parent, &["commit", "-qm", "bump sub"]);
        git(&parent, &["config", "diff.submodule", "diff"]);
        run(&parent, show(None)).await.unwrap();
        assert!(!marker.exists(), "a submodule diff driver ran");

        // A filter driver listed after a large configuration value must still be disabled.
        std::fs::write(parent.join("b.txt"), "b\n").unwrap();
        git(&parent, &["add", "b.txt"]);
        git(&parent, &["commit", "-qm", "b"]);
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(parent.join(".git").join("config"))
            .unwrap();
        let padding = "x".repeat(300 * 1024);
        writeln!(config, "[filter \"padding\"]\n\tclean = {padding}").unwrap();
        writeln!(config, "[filter \"evil\"]\n\tclean = sh -c '{touch}; cat'").unwrap();
        std::fs::write(parent.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        std::fs::write(parent.join("b.txt"), "changed\n").unwrap();
        run(&parent, GitRequest::Status).await.unwrap();
        let diff = GitRequest::Diff {
            rev: None,
            staged: false,
            path: None,
        };
        run(&parent, diff).await.unwrap();
        assert!(!marker.exists(), "a filter driver ran");

        // A `.git` file and object alternates are reported as storage, so they can be checked.
        let pointer = root.join("pointer");
        std::fs::create_dir(&pointer).unwrap();
        let gitdir = src.join(".git");
        std::fs::write(
            pointer.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        let repo = repository(&pointer).await.unwrap();
        assert_eq!(repo.toplevel, pointer);
        assert!(repo.storage.contains(&gitdir), "{:?}", repo.storage);
        git(
            &root,
            &["clone", "-q", "--shared", src.to_str().unwrap(), "borrowed"],
        );
        let repo = repository(&root.join("borrowed")).await.unwrap();
        assert!(
            repo.storage.contains(&gitdir.join("objects")),
            "{:?}",
            repo.storage
        );
    }
}
