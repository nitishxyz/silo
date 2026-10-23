//! Files the user attached to a session: the original bytes in
//! `<state>/attachments/<session>/<id>-<name>`, the name and mime in
//! `<id>.json`. They go when the session is archived.
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
}

impl Attachment {
    /// The prompt's note for the model:
    /// `[attachment att_3f2a9c1b: shot.png, image/png, 812 KB — copy_attachment can save it into the project]`.
    #[must_use]
    pub fn note(&self) -> String {
        format!("[attachment {}: {}, {}, {}{NOTE_HINT}]", self.id, self.name, self.mime, size(self.size))
    }
}

/// Ends every attachment note: where the deferred `copy_attachment` comes in.
pub const NOTE_HINT: &str = " — copy_attachment can save it into the project";

/// `<root>/<session>`.
#[must_use]
pub fn dir(root: &Path, session: &str) -> PathBuf {
    root.join(session)
}

/// `att_` and 8 lowercase hex digits.
#[must_use]
pub fn is_id(id: &str) -> bool {
    id.strip_prefix("att_").is_some_and(|hex| hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// A file name without folders or control characters.
#[must_use]
pub fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(120).collect();
    if clean.is_empty() || clean == "." || clean == ".." { "file".into() } else { clean }
}

/// `812 B`, `812 KB`, `3.4 MB`.
#[must_use]
pub fn size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", (b / 1024.0).round())
    } else {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    }
}

fn data_path(dir: &Path, a: &Attachment) -> PathBuf {
    dir.join(format!("{}-{}", a.id, a.name))
}

fn create_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "attachment folder is a symlink"));
    }
    Ok(())
}

/// Store `bytes` as a new attachment of the session in `dir`.
///
/// # Errors
/// The folder or files cannot be written.
pub fn put(dir: &Path, name: &str, mime: &str, bytes: &[u8]) -> std::io::Result<Attachment> {
    create_dir(dir)?;
    let mime = if mime.trim().is_empty() { "application/octet-stream" } else { mime.trim() };
    loop {
        let id = format!("att_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let attachment = Attachment { id, name: clean_name(name), mime: mime.into(), size: bytes.len() as u64 };
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(data_path(dir, &attachment)) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            other => other?,
        };
        file.write_all(bytes)?;
        file.sync_all()?;
        write_meta(dir, &attachment)?;
        return Ok(attachment);
    }
}

/// Add `bytes` to the end of attachment `id` (a chunked upload); refused past `max` bytes.
///
/// # Errors
/// Unknown id, over `max`, or the files cannot be written.
pub fn append(dir: &Path, id: &str, bytes: &[u8], max: u64) -> std::io::Result<Attachment> {
    let (mut attachment, path) = get(dir, id)?;
    if attachment.size + bytes.len() as u64 > max {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("attachment exceeds {}", size(max))));
    }
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    attachment.size += bytes.len() as u64;
    write_meta(dir, &attachment)?;
    Ok(attachment)
}

fn write_meta(dir: &Path, a: &Attachment) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{}.json.tmp", a.id));
    std::fs::write(&tmp, serde_json::to_vec(a)?)?;
    std::fs::rename(tmp, dir.join(format!("{}.json", a.id)))
}

/// Attachment `id` of the session in `dir`, and where its bytes are.
///
/// # Errors
/// Not a valid id, or no such attachment.
pub fn get(dir: &Path, id: &str) -> std::io::Result<(Attachment, PathBuf)> {
    if !is_id(id) {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("unknown attachment {id}")));
    }
    let meta = std::fs::read(dir.join(format!("{id}.json")))
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, format!("unknown attachment {id}")))?;
    let attachment: Attachment = serde_json::from_slice(&meta)?;
    let path = data_path(dir, &attachment);
    Ok((attachment, path))
}

/// Attachment `id` of the first of `sessions` (the calling session, then its
/// ancestors) that has it.
///
/// # Errors
/// No session has it.
pub fn find(root: &Path, sessions: &[String], id: &str) -> std::io::Result<(Attachment, PathBuf)> {
    sessions
        .iter()
        .find_map(|s| get(&dir(root, s), id).ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("unknown attachment {id}")))
}

/// The session's attachments, oldest first.
#[must_use]
pub fn list(dir: &Path) -> Vec<Attachment> {
    let mut found: Vec<(std::time::SystemTime, Attachment)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".json").filter(|id| is_id(id))?;
            let (a, path) = get(dir, id).ok()?;
            Some((std::fs::metadata(path).and_then(|m| m.modified()).ok()?, a))
        })
        .collect();
    found.sort_by_key(|(at, _)| *at);
    found.into_iter().map(|(_, a)| a).collect()
}

/// Copy the session folder `from` to `to` (a fork keeps its attachments).
///
/// # Errors
/// A file cannot be copied.
pub fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(from) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    create_dir(to)?;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Standard base64 (`attachment.put`).
///
/// # Errors
/// Not base64.
pub fn decode(data: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(data.trim()).map_err(|e| format!("data_base64: {e}"))
}

/// Standard base64.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
