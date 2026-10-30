//! Maps client-supplied paths to workspaces and enforces capabilities.
//!
//! Clients always use absolute paths. A path is canonicalized (following symlinks) and then
//! assigned to the most specific workspace whose root contains it, so `..` segments and symlinks
//! cannot escape a workspace.

use std::path::{Component, Path, PathBuf};

use crate::config::{Capability, Config, Workspace};
use crate::error::ApiError;

pub struct Resolved<'a> {
    pub workspace: &'a Workspace,
    /// Canonical absolute path.
    pub path: PathBuf,
    /// Path relative to the workspace root.
    pub rel: PathBuf,
}

/// Resolves an absolute, existing path inside a workspace.
pub fn resolve<'a>(config: &'a Config, raw: &str) -> Result<Resolved<'a>, ApiError> {
    if raw.trim().is_empty() {
        return Err(ApiError::bad_request(
            "A path is required. Use an absolute path inside one of the workspaces listed by GET /v1/info.",
        ));
    }
    let requested = Path::new(raw);
    if !requested.is_absolute() {
        return Err(ApiError::bad_request(format!(
            "`{raw}` is not an absolute path. Use absolute paths, e.g. a workspace path from GET /v1/info."
        )));
    }
    let outside = || {
        ApiError::forbidden(format!(
            "`{raw}` is outside every workspace this bridge exposes. GET /v1/info lists the allowed folders."
        ))
    };
    let canonical = match dunce::canonicalize(requested) {
        Ok(path) => path,
        // Only reveal that a path is missing when it would lie inside a workspace; otherwise the
        // API would tell anyone with a token which files exist elsewhere on the machine.
        Err(_) if workspace_for(config, &canonicalize_lenient(requested)).is_none() => {
            return Err(outside());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::not_found(format!("`{raw}` does not exist.")));
        }
        Err(err) => {
            return Err(ApiError::forbidden(format!(
                "Cannot access `{raw}`: {err}."
            )));
        }
    };
    let workspace = workspace_for(config, &canonical).ok_or_else(outside)?;
    let rel = canonical
        .strip_prefix(&workspace.root)
        .unwrap_or(Path::new(""))
        .to_path_buf();
    Ok(Resolved {
        workspace,
        path: canonical,
        rel,
    })
}

/// The most specific workspace containing an already canonical path.
pub fn workspace_for<'a>(config: &'a Config, canonical: &Path) -> Option<&'a Workspace> {
    config
        .workspaces
        .iter()
        .filter(|ws| canonical.starts_with(&ws.root))
        .max_by_key(|ws| ws.root.components().count())
}

pub fn require(workspace: &Workspace, capability: Capability) -> Result<(), ApiError> {
    if workspace.allows(capability) {
        return Ok(());
    }
    let allowed: Vec<String> = workspace.allow.iter().map(|c| c.to_string()).collect();
    Err(ApiError::forbidden(format!(
        "Workspace `{}` ({}) does not allow `{capability}`. Allowed here: {}.",
        workspace.name,
        workspace.root.display(),
        if allowed.is_empty() {
            "nothing".into()
        } else {
            allowed.join(", ")
        }
    )))
}

/// Git reports on a whole repository, not on a folder. So the repository's root must lie in a
/// workspace that allows `git`, and no workspace nested in the repository may forbid it. The
/// places git reads history from (`storage`: the git directory, objects and their alternates) must
/// lie in workspaces that allow `git` as well, because a `.git` file can point anywhere.
pub fn require_git_scope(
    config: &Config,
    toplevel: &Path,
    storage: &[PathBuf],
) -> Result<(), ApiError> {
    let Some(workspace) = workspace_for(config, toplevel) else {
        return Err(ApiError::forbidden(format!(
            "This folder belongs to the git repository at `{}`, which reaches outside the workspaces this bridge exposes; its history and diffs would include files you may not see.",
            toplevel.display()
        )));
    };
    require(workspace, Capability::Git)?;
    if let Some(nested) = config.workspaces.iter().find(|ws| {
        ws.root.starts_with(toplevel) && ws.root != workspace.root && !ws.allows(Capability::Git)
    }) {
        return Err(ApiError::forbidden(format!(
            "The git repository at `{}` contains workspace `{}` ({}), where `git` is not allowed, so git access to this repository is refused.",
            toplevel.display(),
            nested.name,
            nested.root.display()
        )));
    }
    if let Some(outside) = storage
        .iter()
        .find(|path| !workspace_for(config, path).is_some_and(|ws| ws.allows(Capability::Git)))
    {
        return Err(ApiError::forbidden(format!(
            "The git repository at `{}` reads its history from `{}` (through a `.git` file, a linked worktree or object alternates), which is outside the workspaces that allow `git`, so git access to it is refused.",
            toplevel.display(),
            outside.display()
        )));
    }
    Ok(())
}

/// Canonicalizes the longest existing prefix of a path that may not exist (yet), so that it can
/// be compared with canonical workspace roots even on systems with symlinked prefixes (macOS
/// `/var` → `/private/var`).
fn canonicalize_lenient(path: &Path) -> PathBuf {
    let normalized = normalize(path);
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    loop {
        if let Ok(mut canonical) = dunce::canonicalize(existing) {
            canonical.extend(missing.iter().rev());
            return canonical;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_owned());
                existing = parent;
            }
            _ => return normalized,
        }
    }
}

/// Lexical normalization (no file system access): drops `.` and resolves `..`.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(workspaces: &[(&Path, &str)]) -> Config {
        let mut text = String::from(
            "[[tokens]]\nname = \"t\"\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n",
        );
        for (path, allow) in workspaces {
            text.push_str(&format!(
                "[[workspaces]]\npath = {}\nallow = {allow}\n",
                toml::Value::String(path.display().to_string())
            ));
        }
        Config::parse(&text, Path::new("test.toml")).unwrap()
    }

    #[test]
    fn most_specific_workspace_wins() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(inner.join("a.txt"), "x").unwrap();
        let cfg = config(&[
            (dir.path(), "[\"files\"]"),
            (&inner, "[\"files\", \"sessions\", \"start\"]"),
        ]);
        let resolved = resolve(&cfg, inner.join("a.txt").to_str().unwrap()).unwrap();
        assert!(resolved.workspace.allows(Capability::Start));
        assert_eq!(resolved.rel, Path::new("a.txt"));
        assert!(require(resolved.workspace, Capability::Git).is_err());
    }

    #[test]
    fn git_scope_covers_the_whole_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(dir.path()).unwrap();
        let public = repo.join("public");
        let private = repo.join("private");
        std::fs::create_dir_all(&public).unwrap();
        std::fs::create_dir_all(&private).unwrap();
        let storage = [repo.join(".git")];
        // Only a subfolder is exposed: the repository root is outside every workspace.
        let cfg = config(&[(&public, "[\"git\"]")]);
        assert_eq!(
            require_git_scope(&cfg, &repo, &storage).unwrap_err().code,
            "forbidden"
        );
        // The root is exposed, but a nested workspace forbids git.
        let cfg = config(&[(&repo, "[\"git\"]"), (&private, "[\"files\"]")]);
        assert!(
            require_git_scope(&cfg, &repo, &storage)
                .unwrap_err()
                .message
                .contains("contains workspace")
        );
        let cfg = config(&[(&repo, "[\"git\"]")]);
        assert!(require_git_scope(&cfg, &repo, &storage).is_ok());
        // The working tree is exposed, but its `.git` file points at another repository.
        let cfg = config(&[(&public, "[\"git\"]")]);
        assert!(
            require_git_scope(&cfg, &public, &storage)
                .unwrap_err()
                .message
                .contains("reads its history from")
        );
    }

    #[test]
    fn traversal_and_outside_paths_are_forbidden() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        std::fs::write(root.path().join("secret.txt"), "x").unwrap();
        let cfg = config(&[(&ws, "[\"files\"]")]);
        let escape = format!("{}/../secret.txt", ws.display());
        assert_eq!(resolve(&cfg, &escape).err().unwrap().code, "forbidden");
        // A missing file outside the workspaces is "forbidden", not "not found".
        let missing = root.path().join("nope.txt");
        assert_eq!(
            resolve(&cfg, missing.to_str().unwrap()).err().unwrap().code,
            "forbidden"
        );
        let missing_inside = ws.join("nope.txt");
        assert_eq!(
            resolve(&cfg, missing_inside.to_str().unwrap())
                .err()
                .unwrap()
                .code,
            "not_found"
        );
        assert_eq!(
            resolve(&cfg, "relative/path").err().unwrap().code,
            "invalid_request"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_a_workspace() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        std::fs::write(root.path().join("secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(root.path().join("secret.txt"), ws.join("link")).unwrap();
        let cfg = config(&[(&ws, "[\"files\"]")]);
        assert_eq!(
            resolve(&cfg, ws.join("link").to_str().unwrap())
                .err()
                .unwrap()
                .code,
            "forbidden"
        );
    }
}
