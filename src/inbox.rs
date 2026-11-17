//! The inbox: messages that the owner or an agent on this machine leaves for Grok Bot. A bot reads
//! them through the API and takes one by claiming it. A message is a folder in the state
//! directory, outside every workspace, so the file endpoints never reach it.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

use crate::auth::hex;
use crate::transfer::{MAX_TRANSFER_BYTES, random_part};

/// Longest body, in bytes.
pub const MAX_BODY_BYTES: usize = 256_000;
/// Longest subject, in bytes.
pub const MAX_SUBJECT_BYTES: usize = 200;
/// Longest bot name, in characters.
pub const MAX_NAME_CHARS: usize = 64;
/// Longest note on a claim or release, in bytes.
pub const MAX_NOTE_BYTES: usize = 1_000;
/// Attachments per message.
pub const MAX_ATTACHMENTS: usize = 20;
/// Messages in one listing, by default and at most.
pub const LIST_DEFAULT: usize = 50;
pub const LIST_MAX: usize = 200;

/// The inbox folder inside the state directory.
pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("inbox")
}

/// Whether `id` has the shape of a message id, so that it names one folder and nothing else.
pub fn valid_id(id: &str) -> bool {
    // `2026-11-17.16-40-12.k7q2`: each separator has a fixed place, the time and date are digits,
    // and the random part is lowercase letters or digits.
    id.len() == 24
        && id.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 | 13 | 16 => b == b'-',
            10 | 19 => b == b'.',
            0..=3 | 5..=6 | 8..=9 | 11..=12 | 14..=15 | 17..=18 => b.is_ascii_digit(),
            _ => b.is_ascii_lowercase() || b.is_ascii_digit(),
        })
}

fn not_found(id: &str) -> Error {
    Error::NotFound(format!(
        "No inbox message `{id}`. List the inbox to find an `id`."
    ))
}

fn format_time(now: SystemTime) -> String {
    humantime::format_rfc3339_seconds(now).to_string()
}

/// `<date>.<time>.<random>` in UTC, e.g. `2026-11-17.16-40-12.k7q2`.
fn new_id(now: SystemTime) -> String {
    let stamp = format_time(now);
    format!(
        "{}.{}.{}",
        &stamp[..10],
        stamp[11..19].replace(':', "-"),
        random_part()
    )
}

/// A folder only the owner may enter.
fn make_dir(path: &Path, recursive: bool) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// A new file only the owner may read. It never replaces an existing file.
fn create_new_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Takes the inbox lock. The CLI and the running bridge both change messages, so every change
/// holds this lock, and the manifest is replaced by a rename, so a reader never sees half of one.
fn lock(state_dir: &Path) -> Result<File, Error> {
    let inbox = dir(state_dir);
    make_dir(&inbox, true)?;
    let path = inbox.join(".lock");
    let file = match create_new_private(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            OpenOptions::new().write(true).open(&path)?
        }
        Err(err) => return Err(err.into()),
    };
    file.lock()?;
    Ok(file)
}

/// The folder of a message. An id of the wrong shape never reaches a path.
fn folder_of(state_dir: &Path, id: &str) -> Result<PathBuf, Error> {
    if valid_id(id) {
        Ok(dir(state_dir).join(id))
    } else {
        Err(not_found(id))
    }
}

/// The manifest of the message in `folder`, or `None` while the folder has none yet.
fn read_message(folder: &Path) -> Result<Option<Message>, Error> {
    match fs::read(folder.join("message.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|err| {
            Error::Storage(io::Error::other(format!("{}: {err}", folder.display())))
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn load(folder: &Path, id: &str) -> Result<Message, Error> {
    read_message(folder)?.ok_or_else(|| not_found(id))
}

/// Writes the manifest through a temporary file and a rename, so it is complete or absent.
fn write_manifest(folder: &Path, message: &Message) -> Result<(), Error> {
    let text =
        serde_json::to_vec_pretty(message).map_err(|err| Error::Storage(io::Error::other(err)))?;
    let tmp = folder.join("message.json.tmp");
    let _ = fs::remove_file(&tmp);
    let mut file = create_new_private(&tmp)?;
    file.write_all(&text)?;
    file.sync_all()?;
    fs::rename(&tmp, folder.join("message.json"))?;
    Ok(())
}

/// A bot name as a client chooses it: free text, but short and printable.
fn check_bot(name: &str) -> Result<(), Error> {
    if name.trim().is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(Error::Invalid(format!(
            "The bot name {name:?} must be 1 to {MAX_NAME_CHARS} characters without control characters."
        )));
    }
    Ok(())
}

fn check_note(note: Option<&str>) -> Result<(), Error> {
    match note {
        Some(note) if note.len() > MAX_NOTE_BYTES => Err(Error::Invalid(format!(
            "The note is {} bytes; the limit is {MAX_NOTE_BYTES}.",
            note.len()
        ))),
        _ => Ok(()),
    }
}

/// An attachment name is a plain file name. It is shown and downloaded by name and never used as
/// a path.
fn check_attachment_name(name: &str) -> Result<(), Error> {
    let plain = !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control);
    if plain {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "The attachment name {name:?} is not a plain file name. Use at most 255 bytes without slashes or control characters."
        )))
    }
}

/// Refuses a message the inbox cannot hold, before anything is written.
fn check_message(new: &NewMessage) -> Result<(), Error> {
    if new.subject.trim().is_empty() {
        return Err(Error::Invalid(
            "The subject is empty. Write one line that names the job.".into(),
        ));
    }
    if new.subject.len() > MAX_SUBJECT_BYTES || new.subject.chars().any(char::is_control) {
        return Err(Error::Invalid(format!(
            "The subject must be one line of at most {MAX_SUBJECT_BYTES} bytes without line breaks."
        )));
    }
    if new.body.len() > MAX_BODY_BYTES {
        return Err(Error::Invalid(format!(
            "The body is {} bytes; the limit is {MAX_BODY_BYTES} (256 KB). Put long material into an attachment.",
            new.body.len()
        )));
    }
    if let Some(project) = &new.project
        && !Path::new(project).is_absolute()
    {
        return Err(Error::Invalid(format!(
            "`project` must be an absolute folder path, not {project:?}."
        )));
    }
    if let Some(to) = &new.to {
        check_bot(to)?;
    }
    if new.attachments.len() > MAX_ATTACHMENTS {
        return Err(Error::Invalid(format!(
            "A message takes at most {MAX_ATTACHMENTS} attachments; this one has {}. Split it into several messages.",
            new.attachments.len()
        )));
    }
    let mut names = HashSet::new();
    let mut total = 0u64;
    for source in &new.attachments {
        check_attachment_name(&source.name)?;
        if !names.insert(source.name.as_str()) {
            return Err(Error::Invalid(format!(
                "Two attachments are named {:?}. Give each attachment its own name.",
                source.name
            )));
        }
        let metadata = source
            .path
            .metadata()
            .map_err(|err| unreadable(&source.path, err))?;
        if !metadata.is_file() {
            return Err(Error::Invalid(format!(
                "`{}` is not a regular file and cannot be attached.",
                source.path.display()
            )));
        }
        total += metadata.len();
    }
    if total > MAX_TRANSFER_BYTES {
        return Err(Error::Invalid(format!(
            "The attachments are {total} bytes together; one message may carry at most {MAX_TRANSFER_BYTES} bytes (96 MB)."
        )));
    }
    Ok(())
}

fn unreadable(path: &Path, err: io::Error) -> Error {
    Error::Invalid(format!(
        "`{}` cannot be read: {err}. Check the path; the message was not stored.",
        path.display()
    ))
}

/// Copies a source file into a new private file and returns its size and SHA-256. Stops with an
/// error when the copy would exceed `budget`, the bytes the attachments of one message may still
/// take.
fn copy_hashed(source: &Path, target: &Path, budget: u64) -> Result<(u64, String), Error> {
    let mut input = File::open(source).map_err(|err| unreadable(source, err))?;
    let mut output = create_new_private(target)?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > budget {
            return Err(Error::Invalid(format!(
                "The attachments of one message may take at most {MAX_TRANSFER_BYTES} bytes (96 MB) together, and `{}` does not fit. Send it in another message or compress it.",
                source.display()
            )));
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    Ok((total, hex(&hasher.finalize())))
}

/// A new, unused message folder. A taken id draws another random part.
fn create_folder(state_dir: &Path, now: SystemTime) -> Result<(String, PathBuf), Error> {
    for _ in 0..10 {
        let id = new_id(now);
        let folder = dir(state_dir).join(&id);
        match make_dir(&folder, false) {
            Ok(()) => return Ok((id, folder)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(Error::Storage(io::Error::other(
        "no free message id after 10 tries",
    )))
}

/// Writes the files and the manifest of a message into its folder. The manifest comes last.
fn store(folder: &Path, id: &str, new: NewMessage, now: SystemTime) -> Result<Message, Error> {
    let mut attachments = Vec::with_capacity(new.attachments.len());
    if !new.attachments.is_empty() {
        make_dir(&folder.join("attachments"), false)?;
    }
    let mut budget = MAX_TRANSFER_BYTES;
    for (index, source) in new.attachments.iter().enumerate() {
        // Stored under their index, so that no client-chosen name ever becomes a path.
        let target = folder.join("attachments").join(index.to_string());
        let (size, sha256) = copy_hashed(&source.path, &target, budget)?;
        budget -= size;
        attachments.push(Attachment {
            name: source.name.clone(),
            size,
            sha256,
        });
    }
    let mut body = create_new_private(&folder.join("body.md"))?;
    body.write_all(new.body.as_bytes())?;
    let message = Message {
        id: id.to_owned(),
        created: format_time(now),
        kind: new.kind,
        subject: new.subject,
        project: new.project,
        to: new.to,
        state: State::Unread,
        attachments,
        claimed_by: None,
        claimed_at: None,
        claim_note: None,
        released_by: None,
        released_at: None,
        release_note: None,
        body: None,
    };
    write_manifest(folder, &message)?;
    Ok(message)
}

/// What a message asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A new job: the bot first picks the most suitable bot for it.
    Task,
    /// Input for an agenda the bot already works on: it finds the earlier discussion first.
    Followup,
}

/// Where a message stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Nobody has taken it yet.
    Unread,
    /// A bot has taken it; `claimed_by` says which.
    Claimed,
    /// It was taken and given back; nobody holds it now.
    Released,
}

/// A file attached to a message.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Attachment {
    /// The name the sender gave the file. Download it by this name.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the content, in lowercase hex.
    pub sha256: String,
}

/// A message as stored and as the API shows it. The body is in the API's single-message answer
/// only; listings leave it out.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Message {
    /// Stable id, for example `2026-11-17.16-40-12.k7q2`.
    pub id: String,
    /// When the bridge stored the message, in UTC, RFC 3339.
    pub created: String,
    pub kind: Kind,
    pub subject: String,
    /// The folder the message concerns, if the sender named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The bot the sender asked for. Without it, any bot may take the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub state: State,
    pub attachments: Vec<Attachment>,
    /// The bot that claimed the message last; it holds the message only while `state` is
    /// `claimed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_note: Option<String>,
    /// The bot that gave the message back last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_note: Option<String>,
    /// Markdown body; only in `GET /v1/inbox/{id}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// A file to attach when a message is sent: where it is and the name it keeps.
#[derive(Clone, Debug)]
pub struct Source {
    pub name: String,
    pub path: PathBuf,
}

/// A message to store.
#[derive(Clone, Debug)]
pub struct NewMessage {
    pub kind: Kind,
    pub subject: String,
    pub project: Option<String>,
    pub to: Option<String>,
    pub body: String,
    pub attachments: Vec<Source>,
}

/// A page of messages, newest first, and how many matched in all.
#[derive(Debug)]
pub struct Listing {
    pub messages: Vec<Message>,
    pub total: usize,
}

/// Why an inbox operation failed.
#[derive(Debug)]
pub enum Error {
    /// The request is not allowed; the text says what to change.
    Invalid(String),
    /// No message with that id, or no attachment with that name.
    NotFound(String),
    /// The state of the message does not allow the operation.
    Conflict { code: &'static str, message: String },
    /// The state directory could not be read or written.
    Storage(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(message) | Error::NotFound(message) => f.write_str(message),
            Error::Conflict { message, .. } => f.write_str(message),
            Error::Storage(err) => write!(f, "the inbox cannot be written: {err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Storage(err)
    }
}

/// Stores a new message and its attachments. The message is complete when its manifest appears,
/// so a listing never shows a half-written one.
pub fn send(state_dir: &Path, new: NewMessage, now: SystemTime) -> Result<Message, Error> {
    check_message(&new)?;
    let _lock = lock(state_dir)?;
    let (id, folder) = create_folder(state_dir, now)?;
    match store(&folder, &id, new, now) {
        Ok(message) => Ok(message),
        Err(err) => {
            // Without its manifest a folder is invisible, but its files would stay on disk.
            let _ = fs::remove_dir_all(&folder);
            Err(err)
        }
    }
}

/// Messages in `state` (all states when `None`) and, if `to` is given, addressed to that bot or to
/// nobody. Newest first; one page of at most `limit` of them from `offset`, without bodies.
pub fn list(
    state_dir: &Path,
    state: Option<State>,
    to: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<Listing, Error> {
    let entries = match fs::read_dir(dir(state_dir)) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Listing {
                messages: Vec::new(),
                total: 0,
            });
        }
        Err(err) => return Err(err.into()),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_id(&id) || !entry.path().is_dir() {
            continue;
        }
        match read_message(&entry.path()) {
            Ok(Some(message)) => {
                if state.is_some_and(|wanted| wanted != message.state) {
                    continue;
                }
                if let Some(bot) = to
                    && message
                        .to
                        .as_deref()
                        .is_some_and(|addressee| addressee != bot)
                {
                    continue;
                }
                found.push(message);
            }
            Ok(None) => {}
            // One damaged message must not hide the others; the log says which one it was.
            Err(err) => {
                tracing::warn!(message = %id, error = %err, "skipping an unreadable inbox message")
            }
        }
    }
    found.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
    let total = found.len();
    let messages = found.into_iter().skip(offset).take(limit).collect();
    Ok(Listing { messages, total })
}

/// One message with its body.
pub fn get(state_dir: &Path, id: &str) -> Result<Message, Error> {
    let folder = folder_of(state_dir, id)?;
    let mut message = load(&folder, id)?;
    message.body = Some(fs::read_to_string(folder.join("body.md"))?);
    Ok(message)
}

/// The file of an attachment, by its name, and its details.
pub fn attachment_file(
    state_dir: &Path,
    id: &str,
    name: &str,
) -> Result<(PathBuf, Attachment), Error> {
    let folder = folder_of(state_dir, id)?;
    let message = load(&folder, id)?;
    let Some(index) = message.attachments.iter().position(|a| a.name == name) else {
        let names: Vec<&str> = message
            .attachments
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        return Err(Error::NotFound(format!(
            "Message `{id}` has no attachment named {name:?}. Its attachments: {names:?}."
        )));
    };
    let attachment = message.attachments[index].clone();
    Ok((
        folder.join("attachments").join(index.to_string()),
        attachment,
    ))
}

/// Takes a message for `bot`. Taking it again as the same bot changes nothing.
pub fn claim(
    state_dir: &Path,
    id: &str,
    bot: &str,
    note: Option<&str>,
    now: SystemTime,
) -> Result<Message, Error> {
    check_bot(bot)?;
    check_note(note)?;
    let _lock = lock(state_dir)?;
    let folder = folder_of(state_dir, id)?;
    let mut message = load(&folder, id)?;
    if let Some(addressee) = &message.to
        && addressee != bot
    {
        return Err(Error::Conflict {
            code: "addressed_to_other",
            message: format!(
                "Message `{id}` is addressed to {addressee:?}, not to {bot:?}. It is not yours to take; leave it for {addressee:?}."
            ),
        });
    }
    match message.state {
        // Taking it again as the same bot changes nothing.
        State::Claimed if message.claimed_by.as_deref() == Some(bot) => return Ok(message),
        State::Claimed => {
            return Err(Error::Conflict {
                code: "already_claimed",
                message: format!(
                    "Message `{id}` was claimed by {:?} at {}. Leave it and pick another message.",
                    message.claimed_by.clone().unwrap_or_default(),
                    message.claimed_at.clone().unwrap_or_default()
                ),
            });
        }
        State::Unread | State::Released => {}
    }
    message.state = State::Claimed;
    message.claimed_by = Some(bot.to_owned());
    message.claimed_at = Some(format_time(now));
    message.claim_note = note.map(str::to_owned);
    write_manifest(&folder, &message)?;
    Ok(message)
}

/// Gives a message back. Only the bot that holds it may do so.
pub fn release(
    state_dir: &Path,
    id: &str,
    bot: &str,
    note: Option<&str>,
    now: SystemTime,
) -> Result<Message, Error> {
    check_bot(bot)?;
    check_note(note)?;
    let _lock = lock(state_dir)?;
    let folder = folder_of(state_dir, id)?;
    let mut message = load(&folder, id)?;
    if message.state != State::Claimed {
        return Err(Error::Conflict {
            code: "not_claimed",
            message: format!("Message `{id}` is not claimed, so there is nothing to give back."),
        });
    }
    if message.claimed_by.as_deref() != Some(bot) {
        return Err(Error::Conflict {
            code: "not_yours",
            message: format!(
                "Message `{id}` is held by {:?}; only that bot can give it back.",
                message.claimed_by.clone().unwrap_or_default()
            ),
        });
    }
    message.state = State::Released;
    message.released_by = Some(bot.to_owned());
    message.released_at = Some(format_time(now));
    message.release_note = note.map(str::to_owned);
    message.claimed_by = None;
    message.claimed_at = None;
    message.claim_note = None;
    write_manifest(&folder, &message)?;
    Ok(message)
}

/// Removes a message and its files. The owner does this on the machine.
pub fn delete(state_dir: &Path, id: &str) -> Result<(), Error> {
    let _lock = lock(state_dir)?;
    let folder = folder_of(state_dir, id)?;
    load(&folder, id)?;
    fs::remove_dir_all(&folder)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sha2::{Digest, Sha256};

    use super::*;

    fn state() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        (dir, state)
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_000_000 + secs)
    }

    fn message(subject: &str) -> NewMessage {
        NewMessage {
            kind: Kind::Task,
            subject: subject.into(),
            project: None,
            to: None,
            body: "Body in *Markdown*.\n".into(),
            attachments: vec![],
        }
    }

    #[test]
    fn ids_are_utc_stamps_and_only_such_ids_name_a_folder() {
        let (_dir, state) = state();
        let stored = send(&state, message("Hello"), at(0)).unwrap();
        let parts: Vec<&str> = stored.id.split('.').collect();
        assert_eq!(parts.len(), 3, "{}", stored.id);
        assert_eq!((parts[0].len(), parts[1].len(), parts[2].len()), (10, 8, 4));
        assert!(stored.created.starts_with(parts[0]) && stored.created.ends_with('Z'));
        assert!(valid_id(&stored.id));
        for bad in ["", "..", "../escape", "a/b", "a\\b", "x y"] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn a_sent_message_is_a_folder_with_private_files() {
        let (_dir, state) = state();
        let stored = send(&state, message("Hello"), at(0)).unwrap();
        let folder = dir(&state).join(&stored.id);
        assert_eq!(
            std::fs::read_to_string(folder.join("body.md")).unwrap(),
            "Body in *Markdown*.\n"
        );
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(folder.join("message.json")).unwrap())
                .unwrap();
        assert!(
            manifest.get("body").is_none(),
            "the body is kept in body.md only"
        );
        assert_eq!(manifest["state"], "unread");
        assert_eq!(manifest["subject"], "Hello");
        assert!(!folder.join("message.json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(dir(&state)), 0o700);
            assert_eq!(mode(folder.clone()), 0o700);
            assert_eq!(mode(folder.join("body.md")), 0o600);
            assert_eq!(mode(folder.join("message.json")), 0o600);
        }
    }

    #[test]
    fn attachments_keep_their_names_and_are_hashed() {
        let (dir, state) = state();
        let source = dir.path().join("report.pdf");
        std::fs::write(&source, b"%PDF-1.4 test").unwrap();
        let mut new = message("With a file");
        new.attachments = vec![Source {
            name: "report.pdf".into(),
            path: source,
        }];
        let stored = send(&state, new, at(0)).unwrap();
        let attachment = &stored.attachments[0];
        assert_eq!(attachment.name, "report.pdf");
        assert_eq!(attachment.size, 13);
        assert_eq!(
            attachment.sha256,
            crate::auth::hex(&Sha256::digest(b"%PDF-1.4 test"))
        );
        let (path, found) = attachment_file(&state, &stored.id, "report.pdf").unwrap();
        assert_eq!(found.sha256, attachment.sha256);
        assert_eq!(std::fs::read(path).unwrap(), b"%PDF-1.4 test");
        assert!(matches!(
            attachment_file(&state, &stored.id, "other.pdf"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn invalid_messages_are_refused_and_nothing_is_stored() {
        let (dir, state) = state();
        let refused = |new: NewMessage| matches!(send(&state, new, at(0)), Err(Error::Invalid(_)));
        assert!(refused(message("   ")));
        assert!(refused(message(&"x".repeat(MAX_SUBJECT_BYTES + 1))));
        assert!(refused(message("line\nbreak")));
        let mut long = message("long");
        long.body = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(refused(long));
        let mut bad_bot = message("bot");
        bad_bot.to = Some("bad\u{7}name".into());
        assert!(refused(bad_bot));
        let mut relative = message("project");
        relative.project = Some("relative/path".into());
        assert!(refused(relative));

        let source = dir.path().join("a.txt");
        std::fs::write(&source, b"a").unwrap();
        let file = |name: &str| Source {
            name: name.into(),
            path: source.clone(),
        };
        let mut many = message("many");
        many.attachments = (0..=MAX_ATTACHMENTS)
            .map(|i| file(&format!("f{i}.txt")))
            .collect();
        assert!(refused(many));
        let mut twice = message("twice");
        twice.attachments = vec![file("same.txt"), file("same.txt")];
        assert!(refused(twice));

        let huge_path = dir.path().join("huge.bin");
        std::fs::File::create(&huge_path)
            .unwrap()
            .set_len(96_000_001)
            .unwrap();
        let mut huge = message("huge");
        huge.attachments = vec![Source {
            name: "huge.bin".into(),
            path: huge_path,
        }];
        assert!(refused(huge));

        assert!(
            list(&state, None, None, 0, LIST_MAX)
                .unwrap()
                .messages
                .is_empty()
        );
    }

    #[test]
    fn attachment_names_are_plain_file_names() {
        let (dir, state) = state();
        let source = dir.path().join("a.txt");
        std::fs::write(&source, b"a").unwrap();
        for bad in [
            "",
            ".",
            "..",
            "../up.txt",
            "sub/x.txt",
            "back\\slash.txt",
            "tab\tname",
        ] {
            let mut new = message("names");
            new.attachments = vec![Source {
                name: bad.into(),
                path: source.clone(),
            }];
            assert!(
                matches!(send(&state, new, at(0)), Err(Error::Invalid(_))),
                "{bad:?} must be refused"
            );
        }
        let mut fine = message("names");
        fine.attachments = vec![Source {
            name: "Zpráva č. 1 (final).pdf".into(),
            path: source,
        }];
        assert!(send(&state, fine, at(0)).is_ok());
    }

    #[test]
    fn a_missing_attachment_leaves_no_half_message_behind() {
        let (dir, state) = state();
        let mut new = message("missing file");
        new.attachments = vec![Source {
            name: "gone.pdf".into(),
            path: dir.path().join("gone.pdf"),
        }];
        assert!(matches!(send(&state, new, at(0)), Err(Error::Invalid(_))));
        assert!(
            list(&state, None, None, 0, LIST_MAX)
                .unwrap()
                .messages
                .is_empty()
        );
        let leftovers: Vec<_> = std::fs::read_dir(super::dir(&state))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name() != ".lock")
            .collect();
        assert!(
            leftovers.is_empty(),
            "nothing may be left behind: {leftovers:?}"
        );
    }

    #[test]
    fn a_folder_without_a_manifest_is_not_a_message() {
        let (_dir, state) = state();
        send(&state, message("real"), at(0)).unwrap();
        std::fs::create_dir_all(dir(&state).join("2026-11-17.16-40-12.zzzz")).unwrap();
        let listing = list(&state, None, None, 0, LIST_MAX).unwrap();
        assert_eq!(listing.total, 1);
        assert_eq!(listing.messages[0].subject, "real");
    }

    #[test]
    fn a_claim_is_exclusive_and_repeating_it_changes_nothing() {
        let (_dir, state) = state();
        let stored = send(&state, message("Task"), at(0)).unwrap();
        let claimed = claim(&state, &stored.id, "Homelab", Some("taking it"), at(1)).unwrap();
        assert_eq!(claimed.state, State::Claimed);
        assert_eq!(claimed.claimed_by.as_deref(), Some("Homelab"));
        assert_eq!(claimed.claim_note.as_deref(), Some("taking it"));
        assert_eq!(
            claim(&state, &stored.id, "Homelab", None, at(2))
                .unwrap()
                .state,
            State::Claimed
        );
        match claim(&state, &stored.id, "Chief of Staff", None, at(3)) {
            Err(Error::Conflict { code, message }) => {
                assert_eq!(code, "already_claimed");
                assert!(message.contains("Homelab"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            claim(&state, &stored.id, "  ", None, at(4)),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn only_the_holder_gives_a_message_back_and_then_others_may_take_it() {
        let (_dir, state) = state();
        let stored = send(&state, message("Task"), at(0)).unwrap();
        assert!(matches!(
            release(&state, &stored.id, "Homelab", None, at(1)),
            Err(Error::Conflict {
                code: "not_claimed",
                ..
            })
        ));
        claim(&state, &stored.id, "Homelab", None, at(2)).unwrap();
        assert!(matches!(
            release(&state, &stored.id, "Chief of Staff", None, at(3)),
            Err(Error::Conflict {
                code: "not_yours",
                ..
            })
        ));
        let given_back = release(&state, &stored.id, "Homelab", Some("for Adam"), at(4)).unwrap();
        assert_eq!(given_back.state, State::Released);
        assert_eq!(given_back.released_by.as_deref(), Some("Homelab"));
        assert_eq!(given_back.release_note.as_deref(), Some("for Adam"));
        assert!(
            given_back.claimed_by.is_none(),
            "a released message is held by nobody"
        );
        assert_eq!(
            claim(&state, &stored.id, "Chief of Staff", None, at(5))
                .unwrap()
                .claimed_by
                .as_deref(),
            Some("Chief of Staff")
        );
    }

    #[test]
    fn racing_claims_have_exactly_one_winner() {
        let (_dir, state) = state();
        let stored = send(&state, message("Task"), at(0)).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let (state, id) = (state.clone(), stored.id.clone());
                std::thread::spawn(move || {
                    claim(&state, &id, &format!("bot {i}"), None, at(1)).is_ok()
                })
            })
            .collect();
        let winners = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    }

    #[test]
    fn a_message_addressed_to_one_bot_is_taken_only_by_that_bot() {
        let (_dir, state) = state();
        let mut new = message("For Homelab");
        new.to = Some("Homelab".into());
        let stored = send(&state, new, at(0)).unwrap();
        assert!(matches!(
            claim(&state, &stored.id, "Chief of Staff", None, at(1)),
            Err(Error::Conflict {
                code: "addressed_to_other",
                ..
            })
        ));
        assert_eq!(
            claim(&state, &stored.id, "Homelab", None, at(2))
                .unwrap()
                .state,
            State::Claimed
        );
    }

    #[test]
    fn listings_filter_by_state_and_bot_and_leave_bodies_out() {
        let (_dir, state) = state();
        send(&state, message("open"), at(1)).unwrap();
        let taken = send(&state, message("taken"), at(2)).unwrap();
        claim(&state, &taken.id, "Homelab", None, at(3)).unwrap();
        let given_back = send(&state, message("given back"), at(4)).unwrap();
        claim(&state, &given_back.id, "Homelab", None, at(5)).unwrap();
        release(&state, &given_back.id, "Homelab", None, at(6)).unwrap();
        let mut addressed = message("for Adam");
        addressed.to = Some("Adam".into());
        send(&state, addressed, at(7)).unwrap();

        let subjects = |which: Option<State>, to: Option<&str>| -> Vec<String> {
            list(&state, which, to, 0, LIST_MAX)
                .unwrap()
                .messages
                .into_iter()
                .map(|m| m.subject)
                .collect()
        };
        assert_eq!(subjects(Some(State::Unread), None), ["for Adam", "open"]);
        assert_eq!(subjects(Some(State::Released), None), ["given back"]);
        assert_eq!(subjects(Some(State::Claimed), None), ["taken"]);
        assert_eq!(subjects(None, None).len(), 4);
        assert_eq!(subjects(Some(State::Unread), Some("Homelab")), ["open"]);
        assert_eq!(
            subjects(Some(State::Released), Some("Homelab")),
            ["given back"]
        );
        assert!(!subjects(None, Some("Homelab")).contains(&"for Adam".to_string()));
        assert!(subjects(None, Some("Adam")).contains(&"for Adam".to_string()));
        let page = list(&state, None, None, 0, 2).unwrap();
        assert_eq!((page.total, page.messages.len()), (4, 2));
        assert!(page.messages.iter().all(|m| m.body.is_none()));
        assert_eq!(
            get(&state, &taken.id).unwrap().body.as_deref(),
            Some("Body in *Markdown*.\n")
        );
    }

    #[test]
    fn a_listing_pages_through_every_message() {
        let (_dir, state) = state();
        for index in 1..=5u64 {
            send(&state, message(&format!("m{index}")), at(index)).unwrap();
        }
        // Newest first: m5, m4, m3, m2, m1.
        let second = list(&state, None, None, 2, 2).unwrap();
        let subjects: Vec<_> = second.messages.iter().map(|m| m.subject.as_str()).collect();
        assert_eq!((second.total, subjects), (5, vec!["m3", "m2"]));
        let last = list(&state, None, None, 4, 2).unwrap();
        assert_eq!(last.messages.len(), 1);
        assert!(list(&state, None, None, 9, 2).unwrap().messages.is_empty());
    }

    #[test]
    fn deleting_removes_the_message() {
        let (_dir, state) = state();
        let stored = send(&state, message("Gone soon"), at(0)).unwrap();
        delete(&state, &stored.id).unwrap();
        assert!(matches!(get(&state, &stored.id), Err(Error::NotFound(_))));
        assert!(matches!(
            delete(&state, &stored.id),
            Err(Error::NotFound(_))
        ));
    }
}
