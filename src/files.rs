//! Read-only file access: directory listings and line ranges of text files.

use std::path::Path;
use std::time::SystemTime;

use axum::http::StatusCode;
use ignore::WalkBuilder;
use serde::Serialize;
use utoipa::ToSchema;

use crate::config::Workspace;
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
    /// Explains an empty or truncated listing.
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

pub fn list(ws: &Workspace, dir: &Path, depth: usize, limit: usize) -> Result<FileList, ApiError> {
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
    let root = ws.root.clone();
    let deny = ws.deny.clone();
    let walker = WalkBuilder::new(dir)
        .max_depth(Some(depth))
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            !deny.matches(entry.path().strip_prefix(&root).unwrap_or(entry.path()))
        })
        .build();

    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in walker.flatten() {
        if entry.depth() == 0 {
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
    let note = if truncated {
        Some(format!(
            "Only the first {limit} entries are shown. List a subdirectory, lower `depth`, or raise `limit` (max {MAX_LIST_LIMIT})."
        ))
    } else if entries.is_empty() {
        Some("The directory is empty, or everything in it is ignored by .gitignore or deny patterns.".into())
    } else {
        None
    };
    Ok(FileList {
        path: dir.display().to_string(),
        entries,
        truncated,
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
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ApiError::too_large(format!(
            "`{}` has {} bytes; files over {MAX_FILE_BYTES} bytes are not served. Ask an agent session to inspect it instead.",
            path.display(),
            metadata.len()
        )));
    }
    let bytes = std::fs::read(path).map_err(ApiError::internal)?;
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
        size: metadata.len(),
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

        let listing = list(ws, &root, 2, 100).unwrap();
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

        let short = list(ws, &root, 2, 1).unwrap();
        assert!(short.truncated && short.note.is_some());
        assert_eq!(
            read(ws, &root.join(".env"), 1, 10).unwrap_err().code,
            "denied"
        );
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
    }
}
