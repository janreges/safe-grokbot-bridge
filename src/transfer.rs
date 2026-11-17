//! File transfer: uploads into a workspace's incoming folder under names the bridge generates,
//! and downloads of any regular file.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Multipart, Request};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use utoipa::ToSchema;

use crate::config::{Capability, Workspace};
use crate::error::ApiError;
use crate::server::AppState;
use crate::{files, models, paths};

/// The incoming folder, directly in a workspace's root.
pub const INBOX: &str = ".sgb-inbox";
/// Bytes per upload request and per downloaded file: under Cloudflare's 100 MB ceiling whether
/// it counts in decimal or binary megabytes.
pub const MAX_TRANSFER_BYTES: u64 = 96_000_000;
/// Files per upload request.
pub const MAX_FILES: usize = 20;
/// Uploads and downloads running at once, together.
pub const MAX_TRANSFERS: usize = 4;
const BUSY_RETRY_SECS: u64 = 5;

/// The slots shared by uploads and downloads.
pub fn slots() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_TRANSFERS))
}

/// The files an upload stored.
#[derive(Serialize, ToSchema, Debug)]
pub struct Uploaded {
    /// One entry per uploaded file, in the order of the request.
    pub files: Vec<StoredFile>,
    /// The workspace's incoming folder, where every upload of this workspace goes.
    pub inbox: String,
    /// What to do with the files, and that they stay until someone deletes them.
    pub note: String,
}

#[derive(Serialize, ToSchema, Debug)]
pub struct StoredFile {
    /// The file name as you sent it, for your own bookkeeping; it is not used on the machine.
    pub name: String,
    /// Absolute path of the stored file. Give it to an agent working on this machine.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
}

/// Stores the files of a multipart form in the incoming folder of `dir`'s workspace: all of them,
/// or none.
pub async fn upload(app: &AppState, dir: Result<String, ApiError>, request: Request) -> Response {
    let too_large = || {
        ApiError::too_large(format!(
            "The upload is larger than 96 MB ({MAX_TRANSFER_BYTES} bytes), the limit of one request. Send the files in several requests, or split a large file. Nothing was stored."
        ))
    };
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    // Answered at once: reading that much only to refuse it would cost more than a reset.
    if declared.is_some_and(|length| length > MAX_TRANSFER_BYTES) {
        return too_large().into_response();
    }
    let (parts, body) = request.into_parts();
    let is_form = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .to_ascii_lowercase()
                .starts_with("multipart/form-data")
        });
    let checked = match dir {
        Ok(dir) => {
            let (app, slots) = (app.clone(), app.transfers.clone());
            tokio::task::spawn_blocking(move || {
                let resolved = paths::resolve(&app.config, &dir)?;
                if !resolved.path.is_dir() {
                    return Err(ApiError::bad_request(format!(
                        "`{dir}` is not a folder. Pass a folder in `dir`, such as a session's `cwd`."
                    )));
                }
                paths::require(resolved.workspace, Capability::Upload)?;
                if !is_form {
                    return Err(ApiError::bad_request(
                        "An upload is a multipart form (`multipart/form-data`) with one part per file, as `curl -F 'file=@report.pdf'` sends it. Nothing was stored.",
                    ));
                }
                let slot = slots.try_acquire_owned().map_err(|_| busy())?;
                let inbox = prepare_inbox(resolved.workspace)?;
                Ok((inbox, slot))
            })
            .await
            .unwrap_or_else(|err| Err(ApiError::internal(err)))
        }
        Err(err) => Err(err),
    };
    let (inbox, _slot) = match checked {
        Ok(checked) => checked,
        Err(err) => {
            // Read before answering, or the client would see a reset instead of the refusal.
            models::drain(body, MAX_TRANSFER_BYTES as usize).await;
            return err.into_response();
        }
    };
    let mut form = match Multipart::from_request(Request::from_parts(parts, body), &()).await {
        Ok(form) => form,
        Err(rejection) => {
            return ApiError::bad_request(format!(
                "The form cannot be read: {}. Nothing was stored.",
                rejection.body_text()
            ))
            .into_response();
        }
    };
    let mut created = Created(Vec::new());
    let stored = store(&mut form, &inbox, &mut created).await;
    let files = match stored {
        Ok(files) if files.is_empty() => Err(ApiError::bad_request(
            "The form has no file. Send each file as a part with a file name, as `curl -F 'file=@report.pdf'` does.",
        )),
        other => other,
    };
    match files {
        Ok(files) => {
            created.0.clear();
            let bytes: u64 = files.iter().map(|file| file.size).sum();
            tracing::info!(files = files.len(), bytes, inbox = %inbox.display(), "upload stored");
            let uploaded = Uploaded {
                files,
                inbox: inbox.display().to_string(),
                note: "Stored. Give an agent on this machine the `path` of a file. Files stay in the incoming folder until someone deletes them on the machine.".into(),
            };
            (StatusCode::CREATED, Json(uploaded)).into_response()
        }
        Err(mut err) => {
            // The rest of the form is read, so that the client gets this answer and not a reset.
            while let Ok(Some(mut field)) = form.next_field().await {
                while let Ok(Some(_)) = field.chunk().await {}
            }
            drop(created);
            if err.status == StatusCode::PAYLOAD_TOO_LARGE {
                err = too_large();
            } else if !err.message.contains("Nothing was stored") {
                err.message.push_str(" Nothing was stored.");
            }
            err.into_response()
        }
    }
}

/// Files an upload created; removed when it fails or the client hangs up.
struct Created(Vec<PathBuf>);

impl Drop for Created {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Writes every file part of the form into the incoming folder.
async fn store(
    form: &mut Multipart,
    inbox: &Path,
    created: &mut Created,
) -> Result<Vec<StoredFile>, ApiError> {
    let broken = |err: axum::extract::multipart::MultipartError| {
        // A body that broke off is the client's side, whatever status the parser picks.
        let status = match err.status() {
            StatusCode::PAYLOAD_TOO_LARGE => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        };
        ApiError::new(
            status,
            "invalid_request",
            format!(
                "The upload broke off or is not a valid form: {}.",
                err.body_text()
            ),
        )
    };
    let mut files = Vec::new();
    while let Some(mut field) = form.next_field().await.map_err(broken)? {
        let Some(name) = field.file_name().map(str::to_string) else {
            return Err(ApiError::bad_request(format!(
                "The part `{}` has no file name. Every part of an upload is a file, as `curl -F 'file=@report.pdf'` sends it.",
                field.name().unwrap_or_default()
            )));
        };
        if files.len() == MAX_FILES {
            return Err(ApiError::bad_request(format!(
                "An upload may carry at most {MAX_FILES} files. Send the rest in another request."
            )));
        }
        let (dir, extension) = (inbox.to_path_buf(), extension(&name));
        let (path, file) = tokio::task::spawn_blocking(move || {
            create_unique(&dir, &extension, SystemTime::now(), random_part)
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(write_error)?;
        created.0.push(path.clone());
        let mut file = tokio::fs::File::from_std(file);
        let mut size = 0u64;
        while let Some(chunk) = field.chunk().await.map_err(broken)? {
            file.write_all(&chunk).await.map_err(write_error)?;
            size += chunk.len() as u64;
        }
        file.flush().await.map_err(write_error)?;
        files.push(StoredFile {
            name,
            path: path.display().to_string(),
            size,
        });
    }
    Ok(files)
}

/// Sends a regular file of a workspace that allows `download`, streamed from the checked handle.
pub async fn download(app: &AppState, path: Result<String, ApiError>) -> Response {
    let path = match path {
        Ok(path) => path,
        Err(err) => return err.into_response(),
    };
    let (app, slots) = (app.clone(), app.transfers.clone());
    let opened = tokio::task::spawn_blocking(move || {
        let resolved = paths::resolve(&app.config, &path)?;
        paths::require(resolved.workspace, Capability::Download)?;
        if resolved.workspace.is_denied(&resolved.rel) {
            return Err(files::denied(&resolved.path));
        }
        let metadata = std::fs::metadata(&resolved.path).map_err(|err| {
            ApiError::not_found(format!("`{path}` cannot be read: {err}."))
        })?;
        if metadata.is_dir() {
            return Err(ApiError::bad_request(format!(
                "`{path}` is a folder. List it with GET /v1/files, or ask an agent to pack it into an archive and download that."
            )));
        }
        if !metadata.is_file() {
            return Err(ApiError::bad_request(format!(
                "`{path}` is not a regular file (a device, socket or pipe) and cannot be downloaded."
            )));
        }
        let too_large = |size: u64| {
            ApiError::too_large(format!(
                "`{path}` has {size} bytes; a download may have at most {MAX_TRANSFER_BYTES} bytes (96 MB). If this workspace allows sessions, ask an agent to split or compress it."
            ))
        };
        if metadata.len() > MAX_TRANSFER_BYTES {
            return Err(too_large(metadata.len()));
        }
        let slot = slots.try_acquire_owned().map_err(|_| busy())?;
        let file = files::open_checked(&resolved.path)?;
        // The length of the open file, so that the header matches what is read.
        let length = file.metadata().map_err(ApiError::internal)?.len();
        if length > MAX_TRANSFER_BYTES {
            return Err(too_large(length));
        }
        let name = resolved
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok((file, length, name, slot))
    })
    .await
    .unwrap_or_else(|err| Err(ApiError::internal(err)));
    let (file, length, name, slot) = match opened {
        Ok(opened) => opened,
        Err(err) => return err.into_response(),
    };
    let sending = Sending {
        file: tokio::fs::File::from_std(file).take(length),
        length,
        sent: 0,
        done: false,
        _slot: slot,
    };
    let body = futures_util::stream::unfold(sending, |mut sending| async move {
        let item = sending.next().await?;
        Some((item, sending))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, length)
        .header(header::CONTENT_DISPOSITION, disposition(&name))
        .header("x-content-type-options", "nosniff")
        .body(Body::from_stream(body))
        .unwrap_or_else(|err| ApiError::internal(err).into_response())
}

/// A download in progress; logs its end with the bytes sent.
struct Sending {
    file: tokio::io::Take<tokio::fs::File>,
    length: u64,
    sent: u64,
    done: bool,
    _slot: OwnedSemaphorePermit,
}

impl Sending {
    async fn next(&mut self) -> Option<std::io::Result<Bytes>> {
        if self.done {
            return None;
        }
        let mut buffer = vec![0u8; 64 * 1024];
        match self.file.read(&mut buffer).await {
            Ok(0) if self.sent == self.length => None,
            // An error aborts the connection, so a cut file is never taken for a whole one.
            Ok(0) => self.fail(std::io::Error::other(
                "the file became shorter while it was sent",
            )),
            Ok(read) => {
                buffer.truncate(read);
                self.sent += read as u64;
                Some(Ok(Bytes::from(buffer)))
            }
            Err(err) => self.fail(err),
        }
    }

    fn fail(&mut self, err: std::io::Error) -> Option<std::io::Result<Bytes>> {
        self.done = true;
        Some(Err(err))
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        tracing::info!(
            bytes = self.sent,
            complete = self.sent == self.length,
            "download ended"
        );
    }
}

/// `attachment` with the file's name: a plain ASCII fallback and the exact UTF-8 name.
fn disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| match c {
            ' '..='~' if c != '"' && c != '\\' => c,
            _ => '_',
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect();
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

/// What `check` should say about a workspace's incoming folder, if anything.
pub fn inbox_warning(ws: &Workspace) -> Option<String> {
    if !ws.allows(Capability::Upload) {
        return None;
    }
    if ws.root.parent().is_none() {
        return Some(format!(
            "Workspace `{}` allows `upload`, but its folder is the root of a file system, which gets no incoming folder; uploads are refused there.",
            ws.name
        ));
    }
    let inbox = ws.root.join(INBOX);
    // A missing folder is created by the first upload.
    let metadata = std::fs::symlink_metadata(&inbox).ok()?;
    let problem = if metadata.file_type().is_symlink() {
        "is a symbolic link, so uploads are refused".to_string()
    } else if !metadata.is_dir() {
        "is not a folder, so uploads are refused".to_string()
    } else if !writable(&inbox) {
        "is not writable for the bridge's user, so uploads fail".to_string()
    } else if group_or_others_may_write(&metadata) {
        format!(
            "can be written by group or others; run `chmod 700 {}`",
            inbox.display()
        )
    } else {
        return None;
    };
    Some(format!(
        "The incoming folder {} of workspace `{}` {problem}.",
        inbox.display(),
        ws.name
    ))
}

fn writable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `path` is a valid NUL-terminated string for the duration of the call.
        unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path).is_ok_and(|m| !m.permissions().readonly())
    }
}

fn group_or_others_may_write(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o022 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

fn write_error(err: std::io::Error) -> ApiError {
    match err.kind() {
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "disk_full",
            "The machine has no space left for the upload. Tell the machine's owner.",
        ),
        _ => ApiError::internal(format!("cannot store the upload: {err}")),
    }
}

fn busy() -> ApiError {
    ApiError::too_many(
        "transfers_busy",
        format!(
            "{MAX_TRANSFERS} uploads and downloads are running on this machine. Wait `Retry-After` seconds and try again."
        ),
        BUSY_RETRY_SECS,
    )
}

/// The extension a stored file gets: the uploaded name's last dot-separated part, lowercased,
/// when it is 1 to 10 ASCII letters or digits (with `tar` before it kept as well); otherwise
/// `bin`. Nothing else of the client's file name reaches the file system.
pub fn extension(file_name: &str) -> String {
    let plain = |part: &str| {
        (1..=10).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric())
    };
    let mut parts = file_name.rsplit('.');
    let (Some(last), Some(before)) = (parts.next(), parts.next()) else {
        return "bin".into();
    };
    if !plain(last) {
        return "bin".into();
    }
    let last = last.to_ascii_lowercase();
    if before.eq_ignore_ascii_case("tar") && last != "tar" {
        format!("tar.{last}")
    } else {
        last
    }
}

/// `<date>.<time>.<random>.<extension>` in UTC, e.g. `2026-11-19.14-32-53.k7q2.zip`.
fn stored_name(now: SystemTime, random: &str, extension: &str) -> String {
    // `2026-11-19T14:32:53Z`
    let stamp = humantime::format_rfc3339_seconds(now).to_string();
    format!(
        "{}.{}.{random}.{extension}",
        &stamp[..10],
        stamp[11..19].replace(':', "-")
    )
}

/// Four characters from `a-z0-9`, against collisions within one second.
fn random_part() -> String {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("the operating system's random number generator failed");
    bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

/// Creates a new file in `dir` under a generated name, never replacing one: a taken name draws
/// another random part. Exclusive creation does not follow a link at the final name either.
fn create_unique(
    dir: &Path,
    extension: &str,
    now: SystemTime,
    mut random: impl FnMut() -> String,
) -> std::io::Result<(PathBuf, File)> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut last = None;
    for _ in 0..10 {
        let path = dir.join(stored_name(now, &random(), extension));
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => last = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(last.expect("the loop ran"))
}

/// The workspace's incoming folder, created when missing, with its `.gitignore`.
pub fn prepare_inbox(ws: &Workspace) -> Result<PathBuf, ApiError> {
    let unusable = |why: String| {
        ApiError::conflict(
            "inbox_unusable",
            format!(
                "Workspace `{}` cannot take uploads: {why} Tell the machine's owner; nothing was stored.",
                ws.name
            ),
        )
    };
    if ws.root.parent().is_none() {
        return Err(unusable(format!(
            "its folder `{}` is the root of a file system, which gets no incoming folder. Ask the machine's owner for a workspace on a normal folder.",
            ws.root.display()
        )));
    }
    let inbox = ws.root.join(INBOX);
    match std::fs::symlink_metadata(&inbox) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(unusable(format!(
                "its incoming folder `{}` is a symbolic link, and uploads never follow a link.",
                inbox.display()
            )));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(unusable(format!(
                "`{}` exists and is not a folder.",
                inbox.display()
            )));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&inbox) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    return prepare_inbox(ws);
                }
                Err(err) => {
                    return Err(unusable(format!(
                        "its incoming folder `{}` cannot be created ({err}).",
                        inbox.display()
                    )));
                }
            }
        }
        Err(err) => {
            return Err(unusable(format!(
                "its incoming folder `{}` cannot be inspected ({err}).",
                inbox.display()
            )));
        }
    }
    // A folder above it could have been swapped for a link meanwhile.
    if !dunce::canonicalize(&inbox).is_ok_and(|real| real == inbox) {
        return Err(unusable(format!(
            "its incoming folder `{}` resolves outside the workspace.",
            inbox.display()
        )));
    }
    let gitignore = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(inbox.join(".gitignore"));
    match gitignore {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(b"*\n").map_err(|err| {
                unusable(format!(
                    "the `.gitignore` of `{}` cannot be written ({err}).",
                    inbox.display()
                ))
            })?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(unusable(format!(
                "the `.gitignore` of `{}` cannot be created ({err}).",
                inbox.display()
            )));
        }
    }
    Ok(inbox)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn workspace(path: &Path) -> Config {
        let text = format!(
            "[[tokens]]\nname = \"t\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\nallow = [\"upload\"]\n",
            "0".repeat(64),
            toml::Value::String(path.display().to_string())
        );
        Config::parse(&text, Path::new("test.toml")).unwrap()
    }

    #[test]
    fn extensions_are_sanitized() {
        let cases = [
            ("data.zip", "zip"),
            ("backup.tar.gz", "tar.gz"),
            ("Archive.TAR.GZ", "tar.gz"),
            ("PHOTO.JPG", "jpg"),
            ("README", "bin"),
            ("trailing.", "bin"),
            ("odd.p@d", "bin"),
            ("long.abcdefghijk", "bin"),
            ("ten.abcdefghij", "abcdefghij"),
            ("../../etc/passwd", "bin"),
            ("dir.d/file", "bin"),
            ("..\\..\\boot.ini", "ini"),
            ("notes.tar", "tar"),
            ("", "bin"),
        ];
        for (name, expected) in cases {
            assert_eq!(extension(name), expected, "{name:?}");
        }
    }

    #[test]
    fn stored_names_have_the_specified_shape() {
        let now = humantime::parse_rfc3339("2026-11-19T14:32:53Z").unwrap();
        assert_eq!(
            stored_name(now, "k7q2", "zip"),
            "2026-11-19.14-32-53.k7q2.zip"
        );
        for _ in 0..50 {
            let random = random_part();
            assert_eq!(random.len(), 4);
            assert!(
                random
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "{random}"
            );
        }
    }

    #[test]
    fn existing_files_are_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let now = humantime::parse_rfc3339("2026-11-19T14:32:53Z").unwrap();
        let taken = dir.path().join("2026-11-19.14-32-53.aaaa.zip");
        std::fs::write(&taken, "first").unwrap();

        let mut names = ["aaaa", "bbbb"].into_iter().map(String::from);
        let (path, _) = create_unique(dir.path(), "zip", now, || names.next().unwrap()).unwrap();
        assert_eq!(path, dir.path().join("2026-11-19.14-32-53.bbbb.zip"));
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "first");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let err = create_unique(dir.path(), "zip", now, || "aaaa".into()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "first");
    }

    #[test]
    fn the_inbox_is_created_with_its_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let config = workspace(&root);
        let inbox = prepare_inbox(&config.workspaces[0]).unwrap();
        assert_eq!(inbox, root.join(INBOX));
        assert_eq!(
            std::fs::read_to_string(inbox.join(".gitignore")).unwrap(),
            "*\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&inbox).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }

        // An existing `.gitignore` is left alone; a missing one is added again.
        std::fs::write(inbox.join(".gitignore"), "custom\n").unwrap();
        prepare_inbox(&config.workspaces[0]).unwrap();
        assert_eq!(
            std::fs::read_to_string(inbox.join(".gitignore")).unwrap(),
            "custom\n"
        );
        std::fs::remove_file(inbox.join(".gitignore")).unwrap();
        prepare_inbox(&config.workspaces[0]).unwrap();
        assert_eq!(
            std::fs::read_to_string(inbox.join(".gitignore")).unwrap(),
            "*\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_warns_about_a_doubtful_inbox() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let config = workspace(&root);
        let ws = &config.workspaces[0];
        assert_eq!(inbox_warning(ws), None, "a missing folder is created later");
        let inbox = prepare_inbox(ws).unwrap();
        assert_eq!(inbox_warning(ws), None);

        std::fs::set_permissions(&inbox, std::fs::Permissions::from_mode(0o777)).unwrap();
        let warning = inbox_warning(ws).unwrap();
        assert!(warning.contains("group or others"), "{warning}");

        std::fs::set_permissions(&inbox, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Root may write anyway, so only check where the test does not run as root.
        if unsafe { libc::geteuid() } != 0 {
            let warning = inbox_warning(ws).unwrap();
            assert!(warning.contains("not writable"), "{warning}");
        }
        std::fs::set_permissions(&inbox, std::fs::Permissions::from_mode(0o700)).unwrap();

        std::fs::remove_dir_all(&inbox).unwrap();
        std::os::unix::fs::symlink(dir.path(), &inbox).unwrap();
        let warning = inbox_warning(ws).unwrap();
        assert!(warning.contains("link"), "{warning}");

        let read_only = Config::parse(
            &format!(
                "[[tokens]]\nname = \"t\"\nsha256 = \"{}\"\n[[workspaces]]\npath = {}\n",
                "0".repeat(64),
                toml::Value::String(root.display().to_string())
            ),
            Path::new("test.toml"),
        )
        .unwrap();
        assert_eq!(inbox_warning(&read_only.workspaces[0]), None);
    }

    #[cfg(unix)]
    #[test]
    fn an_unusable_inbox_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap().join("ws");
        let elsewhere = root.with_file_name("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(INBOX)).unwrap();
        let config = workspace(&root);
        let err = prepare_inbox(&config.workspaces[0]).unwrap_err();
        assert_eq!(err.code, "inbox_unusable");
        assert!(err.message.contains("link"), "{}", err.message);
        assert!(!elsewhere.join(".gitignore").exists());

        std::fs::remove_file(root.join(INBOX)).unwrap();
        std::fs::write(root.join(INBOX), "a file").unwrap();
        let err = prepare_inbox(&config.workspaces[0]).unwrap_err();
        assert_eq!(err.code, "inbox_unusable");

        let config = workspace(Path::new("/"));
        let err = prepare_inbox(&config.workspaces[0]).unwrap_err();
        assert_eq!(err.code, "inbox_unusable");
        assert!(err.message.contains("normal folder"), "{}", err.message);
    }
}
