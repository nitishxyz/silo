//! `files.write` and `files.create`: blocking writes under the root, run
//! off the registry lock.
//!
//! A write replaces an existing regular file atomically: the new contents go
//! to a temporary file in the same folder, which takes the old file's
//! permissions (and owner, when allowed) and is renamed over it. A path that
//! is a symlink (inside the root) writes its target, so the link stays.

use std::{
    fs::{Metadata, OpenOptions},
    io::Write as _,
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _},
    path::Path,
};

use sha2::{Digest as _, Sha256};
use silo_proto::{
    control::RpcError,
    files::{FileBase, FileWritten},
};

use super::{
    paths::{normalize, resolve},
    read::{io_error, mtime},
};

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn sha256_of(path: &Path, rel: &str) -> Result<String, RpcError> {
    let mut file = std::fs::File::open(path).map_err(|e| io_error(rel, &e))?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).map_err(|e| io_error(rel, &e))?;
    Ok(data_encoding::HEXLOWER.encode(&hash.finalize()))
}

fn too_large(rel: &str) -> RpcError {
    RpcError::new("too_large", format!("{rel}: at most {} MiB can be saved", FileWritten::MAX_BYTES >> 20))
}

fn read_only(message: String) -> RpcError {
    RpcError::new(RpcError::READ_ONLY, message)
}

/// Whether this process may write `path` (its permission bits, owner and
/// ACLs, as `access(2)` sees them).
fn writable(path: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else { return false };
    // SAFETY: `c` is a valid NUL-terminated path for the call's duration.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// Whether the file at `path` (now `meta`) is still `base`: by hash when
/// the base has one, else by modification time and size.
fn unchanged(path: &Path, rel: &str, meta: &Metadata, base: &FileBase) -> Result<bool, RpcError> {
    match &base.sha256 {
        Some(_) if meta.len() != base.size => Ok(false),
        Some(hash) => Ok(sha256_of(path, rel)?.eq_ignore_ascii_case(hash)),
        None => Ok(mtime(meta) == base.mtime && meta.len() == base.size),
    }
}

fn written(rel: String, path: &Path, text: &str) -> Result<FileWritten, RpcError> {
    let meta = path.metadata().map_err(|e| io_error(&rel, &e))?;
    Ok(FileWritten { mtime: mtime(&meta), size: meta.len(), sha256: sha256(text.as_bytes()), path: rel })
}

/// Replace file `rel` with `text`.
///
/// # Errors
/// Path errors as [`resolve`]'s; `bad_request` for a folder or special
/// file; `too_large` over [`FileWritten::MAX_BYTES`]; `read_only` when the
/// file or its folder can't be written; `conflict` (with the file's
/// `mtime` and `size`) when it is no longer `base`, unless `force`.
pub fn write(root: &Path, rel: &str, text: &str, base: Option<&FileBase>, force: bool) -> Result<FileWritten, RpcError> {
    let path = resolve(root, rel)?;
    let rel = normalize(rel)?;
    if text.len() as u64 > FileWritten::MAX_BYTES {
        return Err(too_large(&rel));
    }
    let meta = path.metadata().map_err(|e| io_error(&rel, &e))?;
    if meta.is_dir() {
        return Err(RpcError::bad_request(format!("{rel} is a directory")));
    }
    if !meta.is_file() {
        return Err(RpcError::bad_request(format!("{rel} is not a regular file")));
    }
    if meta.permissions().readonly() || !writable(&path) {
        return Err(read_only(format!("{rel} is read-only")));
    }
    if !force
        && let Some(base) = base
        && !unchanged(&path, &rel, &meta, base)?
    {
        let data = serde_json::json!({ "mtime": mtime(&meta), "size": meta.len() });
        return Err(RpcError::new(RpcError::CONFLICT, format!("{rel} changed on disk since it was read")).with_data(data));
    }
    let dir = path.parent().ok_or_else(|| RpcError::bad_request(format!("{rel} has no folder")))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = dir.join(format!(".{name}.silo-{}-{:08x}.tmp", std::process::id(), rand::random::<u32>()));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(meta.mode() & 0o7777).open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        // `mode` above went through the umask: set the old bits exactly.
        std::fs::set_permissions(&temp, meta.permissions())?;
        // Keeps the owner when allowed (root, or a group this user is in).
        let _ = std::os::unix::fs::chown(&temp, Some(meta.uid()), Some(meta.gid()));
        std::fs::rename(&temp, &path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(match error.kind() {
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => {
                read_only(format!("{rel} can't be saved: its folder is read-only"))
            }
            _ => io_error(&rel, &error),
        });
    }
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    written(rel, &path, text)
}

/// Create file `rel` holding `text`; its folder must exist.
///
/// # Errors
/// Path errors as [`resolve`]'s (for the folder); `bad_request` for the
/// root itself or a folder that is not one; `exists` when the path is
/// taken; `read_only` when the folder can't be written.
pub fn create(root: &Path, rel: &str, text: &str) -> Result<FileWritten, RpcError> {
    let rel = normalize(rel)?;
    if rel.is_empty() {
        return Err(RpcError::bad_request("a new file needs a name"));
    }
    if text.len() as u64 > FileWritten::MAX_BYTES {
        return Err(too_large(&rel));
    }
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
    let dir = resolve(root, dir)?;
    if !dir.is_dir() {
        return Err(RpcError::bad_request(format!(
            "{} is not a directory",
            super::paths::relative(&super::paths::canonical_root(root)?, &dir).unwrap_or_default()
        )));
    }
    let path = dir.join(name);
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o666).open(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => RpcError::new(RpcError::EXISTS, format!("{rel} already exists")),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => {
            read_only(format!("{rel} can't be created: its folder is read-only"))
        }
        _ => io_error(&rel, &e),
    })?;
    file.write_all(text.as_bytes()).and_then(|()| file.sync_all()).map_err(|e| io_error(&rel, &e))?;
    written(rel, &path, text)
}
