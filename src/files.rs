//! Read-only file access: directory listings and line ranges of text files.

use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

use axum::http::StatusCode;
use ignore::WalkBuilder;
use serde::Serialize;
use utoipa::ToSchema;

use crate::config::{Capability, Config, DenyRules, Workspace};
use crate::error::ApiError;

pub const MAX_DEPTH: usize = 5;
pub const MAX_LIST_LIMIT: usize = 2000;
pub const MAX_LINES: usize = 4000;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// A directory listing.
#[derive(Serialize, ToSchema, Debug)]
pub struct FileList {
    /// The listed directory (canonical absolute path).
    pub path: String,
    /// Entries in path order. Files ignored by `.gitignore` and paths matching the workspace's
    /// deny patterns are left out.
    pub entries: Vec<FileEntry>,
    /// `true` when `limit` cut the listing short.
    pub truncated: bool,
    /// Pass this as `offset` to get the next entries (present when `truncated`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// Entries skipped before this page.
    pub offset: usize,
    /// Levels descended (the requested `depth`, capped at 5).
    pub depth: usize,
    /// Maximum entries returned (the requested `limit`, capped at 2000).
    pub limit: usize,
    /// Entries that could not be read (e.g. permission denied); the listing is incomplete when
    /// this is above 0.
    pub unreadable: usize,
    /// Explains an empty, truncated or incomplete listing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Serialize, ToSchema, Debug)]
pub struct FileEntry {
    /// Absolute path; pass it to `/v1/files/content` or, for directories, to `/v1/files`.
    pub path: String,
    pub kind: EntryKind,
    /// Size in bytes (files only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Last modification time, RFC 3339.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

#[derive(Serialize, ToSchema, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// A range of lines from a text file.
#[derive(Serialize, ToSchema, Debug)]
pub struct FileContent {
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    pub total_lines: usize,
    /// First line returned (1-based).
    pub start_line: usize,
    /// Last line returned (1-based, inclusive); 0 for an empty file.
    pub end_line: usize,
    /// `true` when the file continues after `end_line`.
    pub truncated: bool,
    /// Where to continue reading when `truncated` is `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_start_line: Option<usize>,
    pub content: String,
}

fn denied(path: &Path) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "denied",
        format!(
            "`{}` matches a deny pattern of this workspace (secrets such as .env files and private keys are never served).",
            path.display()
        ),
    )
}

/// Lists `dir` (inside `ws`), page by page in path order. Entries that belong to a nested
/// workspace without the `files` capability, or that match the deny patterns of the workspace
/// they belong to, are left out.
pub fn list(
    config: &Config,
    ws: &Workspace,
    dir: &Path,
    depth: usize,
    limit: usize,
    offset: usize,
) -> Result<FileList, ApiError> {
    let rel = dir.strip_prefix(&ws.root).unwrap_or(Path::new(""));
    if ws.is_denied(rel) {
        return Err(denied(dir));
    }
    if !dir.is_dir() {
        return Err(ApiError::bad_request(format!(
            "`{}` is a file. Read it with GET /v1/files/content.",
            dir.display()
        )));
    }
    let depth = depth.clamp(1, MAX_DEPTH);
    let limit = limit.clamp(1, MAX_LIST_LIMIT);
    // The rules of every workspace, most specific first, so that an entry is judged by the
    // workspace it belongs to (a nested workspace may forbid what its parent allows).
    let mut rules: Vec<(std::path::PathBuf, bool, DenyRules)> = config
        .workspaces
        .iter()
        .map(|w| (w.root.clone(), w.allows(Capability::Files), w.deny.clone()))
        .collect();
    rules.sort_by_key(|(root, _, _)| std::cmp::Reverse(root.components().count()));
    let walker = WalkBuilder::new(dir)
        .max_depth(Some(depth))
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            let path = entry.path();
            rules
                .iter()
                .find(|(root, _, _)| path.starts_with(root))
                .is_some_and(|(root, files, deny)| {
                    *files && !deny.matches(path.strip_prefix(root).unwrap_or(path))
                })
        })
        .build();

    let mut entries = Vec::new();
    let mut truncated = false;
    let mut unreadable = 0;
    let mut skipped = 0;
    for entry in walker {
        let Ok(entry) = entry else {
            unreadable += 1;
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        if skipped < offset {
            skipped += 1;
            continue;
        }
        if entries.len() == limit {
            truncated = true;
            break;
        }
        let file_type = entry.file_type();
        let kind = match file_type {
            Some(t) if t.is_symlink() => EntryKind::Symlink,
            Some(t) if t.is_dir() => EntryKind::Dir,
            Some(t) if t.is_file() => EntryKind::File,
            _ => EntryKind::Other,
        };
        let metadata = entry.metadata().ok();
        entries.push(FileEntry {
            path: entry.path().display().to_string(),
            size: (kind == EntryKind::File)
                .then(|| metadata.as_ref().map(|m| m.len()))
                .flatten(),
            modified: metadata.and_then(|m| m.modified().ok()).map(rfc3339),
            kind,
        });
    }
    let next_offset = truncated.then_some(offset + limit);
    let mut notes = Vec::new();
    if let Some(next) = next_offset {
        notes.push(format!(
            "Entries {}-{} are shown; more exist. Continue with `offset={next}` (same `path` and `depth`).",
            offset + 1,
            offset + entries.len()
        ));
    }
    if unreadable > 0 {
        notes.push(format!(
            "{unreadable} entries could not be read (for example permission denied), so the listing is incomplete."
        ));
    }
    if entries.is_empty() && unreadable == 0 {
        notes.push(if offset > 0 && skipped < offset {
            format!("There are only {skipped} entries; `offset={offset}` is past the end.")
        } else {
            "The directory is empty, or everything in it is ignored by .gitignore or deny patterns."
                .to_string()
        });
    }
    let note = (!notes.is_empty()).then(|| notes.join(" "));
    Ok(FileList {
        path: dir.display().to_string(),
        entries,
        truncated,
        next_offset,
        offset,
        depth,
        limit,
        unreadable,
        note,
    })
}

pub fn read(
    ws: &Workspace,
    path: &Path,
    start_line: usize,
    max_lines: usize,
) -> Result<FileContent, ApiError> {
    let rel = path.strip_prefix(&ws.root).unwrap_or(Path::new(""));
    if ws.is_denied(rel) {
        return Err(denied(path));
    }
    let metadata = std::fs::metadata(path).map_err(ApiError::internal)?;
    if metadata.is_dir() {
        return Err(ApiError::bad_request(format!(
            "`{}` is a directory. List it with GET /v1/files.",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(ApiError::bad_request(format!(
            "`{}` is not a regular file (a device, socket or pipe) and cannot be read.",
            path.display()
        )));
    }
    let too_large = |size: u64| {
        ApiError::too_large(format!(
            "`{}` has {size} bytes or more; this endpoint serves files up to {MAX_FILE_BYTES} bytes and no parameter changes that. If this workspace allows sessions, an agent session in that folder can inspect the file.",
            path.display()
        ))
    };
    if metadata.len() > MAX_FILE_BYTES {
        return Err(too_large(metadata.len()));
    }
    // The size may change between the check and the read, so the read itself is capped too.
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(ApiError::internal)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "binary_file",
            format!(
                "`{}` is a binary file; only text files can be read.",
                path.display()
            ),
        ));
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = start_line.max(1);
    if start > total.max(1) {
        return Err(ApiError::bad_request(format!(
            "start_line {start} is past the end of the file ({total} lines)."
        )));
    }
    let end = total.min(start - 1 + max_lines.clamp(1, MAX_LINES));
    let content = if total == 0 {
        String::new()
    } else {
        lines[start - 1..end].join("\n")
    };
    Ok(FileContent {
        path: path.display().to_string(),
        size: bytes.len() as u64,
        total_lines: total,
        start_line: start,
        end_line: end,
        truncated: end < total,
        next_start_line: (end < total).then_some(end + 1),
        content,
    })
}

pub fn rfc3339(time: SystemTime) -> String {
    humantime::format_rfc3339_seconds(time).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn workspace(dir: &Path) -> Config {
        let text = format!(
            "[[tokens]]\nname = \"t\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\n",
            "0".repeat(64),
            toml::Value::String(dir.display().to_string())
        );
        Config::parse(&text, Path::new("t.toml")).unwrap()
    }

    #[test]
    fn listing_respects_gitignore_deny_patterns_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        let cfg = workspace(&root);
        let ws = &cfg.workspaces[0];

        let listing = list(&cfg, ws, &root, 2, 100, 0).unwrap();
        let names: Vec<_> = listing
            .entries
            .iter()
            .map(|e| {
                e.path
                    .strip_prefix(&root.display().to_string())
                    .unwrap()
                    .to_string()
            })
            .collect();
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            names,
            [
                format!("{sep}.gitignore"),
                format!("{sep}src"),
                format!("{sep}src{sep}main.rs")
            ]
        );
        assert!(!listing.truncated);

        let short = list(&cfg, ws, &root, 2, 1, 0).unwrap();
        assert!(short.truncated && short.note.is_some());
        assert_eq!(short.next_offset, Some(1));
        let page2 = list(&cfg, ws, &root, 2, 1, 1).unwrap();
        assert_eq!(page2.entries[0].path, listing.entries[1].path);
        assert_eq!(
            read(ws, &root.join(".env"), 1, 10).unwrap_err().code,
            "denied"
        );
    }

    #[test]
    fn listing_honours_nested_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("private")).unwrap();
        std::fs::write(root.join("private/plan.txt"), "x").unwrap();
        std::fs::write(root.join("open.txt"), "x").unwrap();
        let text = format!(
            "[[tokens]]\nname = \"t\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\n[[workspaces]]\npath = {}\nallow = [\"git\"]\n",
            "0".repeat(64),
            toml::Value::String(root.display().to_string()),
            toml::Value::String(root.join("private").display().to_string())
        );
        let cfg = Config::parse(&text, Path::new("t.toml")).unwrap();
        let listing = list(&cfg, &cfg.workspaces[0], &root, 3, 100, 0).unwrap();
        let paths: Vec<_> = listing.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, [root.join("open.txt").display().to_string()]);
    }

    #[test]
    fn reading_returns_a_line_window_and_rejects_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("a.txt"), "one\r\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(root.join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        let cfg = workspace(&root);
        let ws = &cfg.workspaces[0];

        let window = read(ws, &root.join("a.txt"), 2, 2).unwrap();
        assert_eq!(window.content, "two\nthree");
        assert_eq!(
            (window.total_lines, window.end_line, window.next_start_line),
            (4, 3, Some(4))
        );
        let rest = read(ws, &root.join("a.txt"), 4, 10).unwrap();
        assert!(!rest.truncated && rest.content == "four");
        assert!(read(ws, &root.join("a.txt"), 9, 10).is_err());
        assert_eq!(
            read(ws, &root.join("b.bin"), 1, 10).unwrap_err().code,
            "binary_file"
        );
        #[cfg(unix)]
        {
            // Devices are refused instead of being read without end.
            let null = Path::new("/dev/null");
            assert_eq!(read(ws, null, 1, 10).unwrap_err().code, "invalid_request");
        }
    }
}
