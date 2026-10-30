//! `files.list`, `files.read` and `files.read_image` (images and PDFs): blocking reads, run
//! off the registry lock.

use std::{fs::Metadata, io::Read, path::Path, time::UNIX_EPOCH};

use silo_proto::{
    control::RpcError,
    files::{ContentKind, FileEntry, FileImage, FileKind, FileList, FileRead},
};

use super::paths::{canonical_root, inside, normalize, resolve};

/// Names never listed.
const HIDDEN: &[&str] = &[".git"];
/// Bytes inspected for NULs when deciding text or binary.
const SNIFF: usize = 8 * 1024;

pub(super) fn mtime(meta: &Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
}

pub(super) fn created(meta: &Metadata) -> u64 {
    meta.created().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
}

/// Hidden as Finder hides it: a dot name, or (macOS) the hidden flag.
pub(super) fn hidden(name: &str, meta: &Metadata) -> bool {
    #[cfg(target_os = "macos")]
    const UF_HIDDEN: u32 = 0x8000;
    #[cfg(target_os = "macos")]
    let flagged = std::os::macos::fs::MetadataExt::st_flags(meta) & UF_HIDDEN != 0;
    #[cfg(not(target_os = "macos"))]
    let flagged = {
        let _ = meta;
        false
    };
    name.starts_with('.') || flagged
}

/// The entry for `path` (named `name`) under canonical root `canonical`:
/// a symlink's target described when it is inside the root.
pub(super) fn entry_of(canonical: &Path, path: &Path, name: String) -> Option<FileEntry> {
    let meta = path.symlink_metadata().ok()?;
    let kind = kind_of(&meta);
    let hidden = hidden(&name, &meta);
    let (target, meta) = if kind == FileKind::Symlink {
        match path.canonicalize().ok().filter(|t| inside(canonical, t)).and_then(|t| t.metadata().ok()) {
            Some(target) => (Some(kind_of(&target)), target),
            None => (None, meta),
        }
    } else {
        (None, meta)
    };
    Some(FileEntry { name, kind, target, size: meta.len(), mtime: mtime(&meta), created: created(&meta), hidden })
}

pub(super) fn io_error(path: &str, error: &std::io::Error) -> RpcError {
    match error.kind() {
        std::io::ErrorKind::NotFound => RpcError::not_found(path),
        std::io::ErrorKind::PermissionDenied => RpcError::denied(format!("{path}: permission denied")),
        _ => RpcError::internal(format!("{path}: {error}")),
    }
}

fn kind_of(meta: &Metadata) -> FileKind {
    let t = meta.file_type();
    if t.is_symlink() {
        FileKind::Symlink
    } else if t.is_dir() {
        FileKind::Dir
    } else if t.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

/// A directory's entries: directories (and links to them) first, then by
/// name; `.git` and names that are not UTF-8 left out.
pub fn list(root: &Path, rel: &str) -> Result<FileList, RpcError> {
    let path = resolve(root, rel)?;
    let rel = normalize(rel)?;
    let canonical = canonical_root(root)?;
    if !path.is_dir() {
        return Err(RpcError::bad_request(format!("{rel} is not a directory")));
    }
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in std::fs::read_dir(&path).map_err(|e| io_error(&rel, &e))? {
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else { continue };
        if HIDDEN.contains(&name.as_str()) {
            continue;
        }
        if entries.len() == FileList::MAX_ENTRIES {
            truncated = true;
            break;
        }
        if let Some(entry) = entry_of(&canonical, &entry.path(), name) {
            entries.push(entry);
        }
    }
    let dir = |e: &FileEntry| e.kind == FileKind::Dir || e.target == Some(FileKind::Dir);
    entries.sort_by(|a, b| dir(b).cmp(&dir(a)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then_with(|| a.name.cmp(&b.name)));
    Ok(FileList { path: rel, entries, truncated })
}

/// The MIME type of `name`, by extension.
fn mime_by_name(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "csv" => "text/csv",
        "xml" => "application/xml",
        _ => return None,
    })
}

/// The MIME type of contents starting with `head`, by magic number.
fn mime_by_magic(head: &[u8]) -> Option<&'static str> {
    const MAGIC: &[(&[u8], &str)] = &[
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"%PDF-", "application/pdf"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b", "application/gzip"),
        (b"\0asm", "application/wasm"),
    ];
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    MAGIC.iter().find(|(magic, _)| head.starts_with(magic)).map(|(_, mime)| *mime)
}

/// Open a regular file under the root.
fn open(root: &Path, rel: &str) -> Result<(String, std::fs::File, Metadata), RpcError> {
    let path = resolve(root, rel)?;
    let rel = normalize(rel)?;
    let meta = path.metadata().map_err(|e| io_error(&rel, &e))?;
    if meta.is_dir() {
        return Err(RpcError::bad_request(format!("{rel} is a directory")));
    }
    if !meta.is_file() {
        return Err(RpcError::bad_request(format!("{rel} is not a regular file")));
    }
    let file = std::fs::File::open(&path).map_err(|e| io_error(&rel, &e))?;
    Ok((rel, file, meta))
}

/// Up to `max_bytes` of a file: text when it is UTF-8 without NULs.
pub fn read(root: &Path, rel: &str, max_bytes: Option<u64>) -> Result<FileRead, RpcError> {
    let (rel, file, meta) = open(root, rel)?;
    let max = max_bytes.unwrap_or(FileRead::DEFAULT_BYTES).min(FileRead::MAX_BYTES);
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes).map_err(|e| io_error(&rel, &e))?;
    let truncated = bytes.len() as u64 > max;
    bytes.truncate(max as usize);
    let named = mime_by_name(&rel);
    let magic = mime_by_magic(&bytes);
    let nul = bytes[..bytes.len().min(SNIFF)].contains(&0);
    let utf8 = std::str::from_utf8(&bytes).map(|_| ()).map_err(|e| (e.valid_up_to(), e.error_len()));
    let text = match utf8 {
        _ if nul || magic.is_some() => None,
        Ok(()) => Some(bytes),
        // A character cut at `max_bytes`.
        Err((valid, None)) if truncated => {
            bytes.truncate(valid);
            Some(bytes)
        }
        Err(_) => None,
    };
    let (kind, text, mime) = match text {
        Some(text) => (ContentKind::Text, String::from_utf8(text).ok(), named.unwrap_or("text/plain")),
        None => (ContentKind::Binary, None, magic.or(named).unwrap_or("application/octet-stream")),
    };
    let sha256 = text.as_deref().filter(|_| !truncated).map(|t| super::write::sha256(t.as_bytes()));
    Ok(FileRead { path: rel, kind, text, mime: mime.to_owned(), size: meta.len(), mtime: mtime(&meta), truncated, sha256 })
}

/// An image's (or a PDF's) bytes, base64.
pub fn read_image(root: &Path, rel: &str) -> Result<FileImage, RpcError> {
    const IMAGES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp", "image/svg+xml", "application/pdf"];
    let (rel, file, meta) = open(root, rel)?;
    if meta.len() > FileImage::MAX_BYTES {
        return Err(RpcError::new("too_large", format!("{rel} is over {} MiB", FileImage::MAX_BYTES >> 20)));
    }
    let mut bytes = Vec::new();
    file.take(FileImage::MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| io_error(&rel, &e))?;
    if bytes.len() as u64 > FileImage::MAX_BYTES {
        return Err(RpcError::new("too_large", format!("{rel} is over {} MiB", FileImage::MAX_BYTES >> 20)));
    }
    let mime = mime_by_magic(&bytes).or_else(|| mime_by_name(&rel)).filter(|m| IMAGES.contains(m));
    let mime = mime.ok_or_else(|| RpcError::bad_request(format!("{rel} is not a png, jpeg, gif, webp or svg image, or a pdf")))?;
    let data_base64 = data_encoding::BASE64.encode(&bytes);
    Ok(FileImage { path: rel, mime: mime.to_owned(), size: bytes.len() as u64, mtime: mtime(&meta), data_base64 })
}
