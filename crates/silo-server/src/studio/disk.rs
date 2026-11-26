//! File-system helpers for studio blocks: paths under the root (Files'
//! rules), names (slugs), atomic writes and the per-file write locks.

use std::{
    collections::HashMap,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::UNIX_EPOCH,
};

use silo_proto::control::RpcError;

use crate::files::paths;

/// The longest canvas, entity or section name.
pub const NAME_MAX: usize = 96;

pub fn io_error(what: &str, error: &std::io::Error) -> RpcError {
    match error.kind() {
        std::io::ErrorKind::NotFound => RpcError::not_found(what),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => RpcError::denied(format!("{what}: permission denied")),
        _ => RpcError::internal(format!("{what}: {error}")),
    }
}

/// `name` as one path component: not empty, no `/`, not `.`/`..`, not
/// hidden, printable, at most [`NAME_MAX`] bytes.
///
/// # Errors
/// `bad_request` otherwise.
pub fn component(what: &str, name: &str) -> Result<String, RpcError> {
    let name = name.trim();
    if name.is_empty() || name.len() > NAME_MAX || name.contains('/') || name.starts_with('.') || name.chars().any(char::is_control) {
        return Err(RpcError::bad_request(format!("{what} {name:?} must be a folder name (1 to {NAME_MAX} bytes, no /, not hidden)")));
    }
    Ok(name.to_owned())
}

/// A slug of `text`: lowercase ASCII letters, digits and `-`; `fallback`
/// when nothing is left.
pub fn slug(text: &str, fallback: &str) -> String {
    let mut out = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let mut out: String = out.trim_matches('-').chars().take(48).collect();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() { fallback.to_owned() } else { out }
}

/// `name`, else `name-2`, `name-3`… (before the extension when `ext`),
/// free in `dir`.
pub fn free_name(dir: &Path, name: &str, ext: bool) -> String {
    if dir.join(name).symlink_metadata().is_err() {
        return name.to_owned();
    }
    let (stem, suffix) = if ext { crate::trash::split_ext(name) } else { (name, "") };
    (2..).map(|n| format!("{stem}-{n}{suffix}")).find(|n| dir.join(n).symlink_metadata().is_err()).expect("a free name")
}

/// Root-relative `rel` (normalized, not empty) for a write: its nearest
/// existing folder (symlinks resolved) is under the root, and an existing
/// file there resolves under the root. The absolute path to write.
///
/// # Errors
/// See [`paths::normalize`]; `forbidden` when it leaves the root.
pub fn target(root: &Path, rel: &str) -> Result<PathBuf, RpcError> {
    let rel = paths::normalize(rel)?;
    if rel.is_empty() {
        return Err(RpcError::bad_request("a path is needed"));
    }
    let root = paths::canonical_root(root)?;
    let path = root.join(&rel);
    let existing = path.ancestors().find(|a| a.exists()).unwrap_or(&root);
    let canonical = existing.canonicalize().map_err(|e| io_error(&rel, &e))?;
    if !paths::inside(&root, &canonical) {
        return Err(RpcError::denied(format!("path {rel} is outside the block's root")));
    }
    Ok(path)
}

/// Make folder `rel` (and its parents) under the root.
///
/// # Errors
/// As [`target`]; an I/O failure.
pub fn mkdirs(root: &Path, rel: &str) -> Result<PathBuf, RpcError> {
    let path = target(root, rel)?;
    std::fs::create_dir_all(&path).map_err(|e| io_error(rel, &e))?;
    // Made through a link that leaves the root: refused after the fact.
    let canonical = path.canonicalize().map_err(|e| io_error(rel, &e))?;
    if !paths::inside(&paths::canonical_root(root)?, &canonical) {
        return Err(RpcError::denied(format!("path {rel} is outside the block's root")));
    }
    Ok(path)
}

/// Write `bytes` to `path` atomically: a temporary file in its folder,
/// renamed over it (an existing file's permissions kept).
///
/// # Errors
/// An I/O failure, named as `what`.
pub fn atomic_write(path: &Path, bytes: &[u8], what: &str) -> Result<(), RpcError> {
    let dir = path.parent().ok_or_else(|| RpcError::bad_request(format!("{what} has no folder")))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = dir.join(format!(".{name}.silo-{}-{:08x}.tmp", std::process::id(), rand::random::<u32>()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Ok(meta) = path.metadata() {
            std::fs::set_permissions(&temp, meta.permissions())?;
        }
        std::fs::rename(&temp, path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(io_error(what, &error));
    }
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Whether `name` is one of our temporary files.
pub fn is_temp(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".tmp") && name.contains(".silo-")
}

/// Modification time in unix ms; 0 when unknown.
pub fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
}

/// Modification time in nanoseconds, for change detection.
pub fn mtime_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())
}

/// One lock per file written (canonical root + root-relative path), so
/// writes to it are serialised across blocks and connections.
pub fn lock_for(root: &Path, rel: &str) -> Arc<Mutex<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(Default::default);
    let mut locks = LOCKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if locks.len() > 4096 {
        locks.retain(|_, l| Arc::strong_count(l) > 1);
    }
    locks.entry(root.join(rel)).or_default().clone()
}

/// Run `f` holding the write lock of `root`/`rel`.
pub fn locked<T>(root: &Path, rel: &str, f: impl FnOnce() -> T) -> T {
    let lock = lock_for(root, rel);
    let _guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f()
}
