//! `files.mkdir`, `files.rename`, `files.copy`, `files.move` and
//! `files.trash`: blocking file operations under the root, run off the
//! registry lock.
//!
//! An item is named by its folder, resolved and checked to be inside the
//! root, and its own name: a symlink is the link itself (renamed, moved,
//! copied as a link or trashed), never what it points to. Nothing here
//! deletes: a replaced item goes to the Trash first, and the source of a
//! move across file systems is removed only once its copy is complete.

use std::{
    io,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};

use silo_proto::{
    control::RpcError,
    files::{FileFailed, FileMoved, FilesDone, FilesTransferParams},
};

use super::paths::{canonical_root, normalize, relative, resolve};
use crate::trash::{free, split_ext};

/// An operation's failure on `rel`.
fn op_error(rel: &str, error: &io::Error) -> RpcError {
    match error.kind() {
        io::ErrorKind::NotFound => RpcError::not_found(format!("{rel} no longer exists")),
        io::ErrorKind::AlreadyExists => RpcError::new(RpcError::EXISTS, format!("{rel} already exists")),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
            RpcError::new(RpcError::READ_ONLY, format!("{rel}: permission denied"))
        }
        _ => RpcError::internal(format!("{rel}: {error}")),
    }
}

/// Item `rel` (not the root): its normalized path and where it is, its
/// folder canonical and inside the root. The item itself may not exist.
///
/// # Errors
/// Path errors as [`resolve`]'s for the folder; `bad_request` for the root
/// itself or a folder that is not one.
pub fn item(root: &Path, rel: &str) -> Result<(String, PathBuf), RpcError> {
    let rel = normalize(rel)?;
    if rel.is_empty() {
        return Err(RpcError::bad_request("the block's root itself can't be changed"));
    }
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
    let folder = resolve(root, dir)?;
    if !folder.is_dir() {
        return Err(RpcError::bad_request(format!("{dir} is not a folder")));
    }
    let path = folder.join(name);
    Ok((rel, path))
}

/// A folder under the root, canonical.
fn folder(root: &Path, rel: &str) -> Result<PathBuf, RpcError> {
    let dir = resolve(root, rel)?;
    if !dir.is_dir() {
        return Err(RpcError::bad_request(format!("{} is not a folder", normalize(rel)?)));
    }
    Ok(dir)
}

/// The same file (device and inode): a case-only rename on a
/// case-insensitive volume.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.symlink_metadata(), b.symlink_metadata()) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// A real folder (not a link to one).
fn is_real_dir(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.is_dir())
}

fn trash_item(path: &Path, rel: &str, trash: Option<&Path>) -> Result<(), RpcError> {
    crate::trash::trash(path, trash).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => RpcError::not_found(format!("{rel} no longer exists")),
        _ => RpcError::new(RpcError::READ_ONLY, format!("{rel} can't be moved to the Trash: {e}")),
    })
}

/// `files.mkdir`.
///
/// # Errors
/// Path errors as [`item`]'s; `exists` when the name is taken (not
/// `unique`); `read_only` when the folder can't be written.
pub fn mkdir(root: &Path, rel: &str, unique: bool) -> Result<FilesDone, RpcError> {
    let canonical = canonical_root(root)?;
    let (rel, path) = item(root, rel)?;
    let path = match (unique, path.parent(), path.file_name()) {
        (true, Some(dir), Some(name)) => free(dir, &name.to_string_lossy()),
        _ => path,
    };
    std::fs::create_dir(&path).map_err(|e| op_error(&rel, &e))?;
    let to = relative(&canonical, &path).unwrap_or(rel);
    Ok(FilesDone { done: vec![FileMoved { from: String::new(), to }], failed: Vec::new() })
}

/// `files.rename`.
///
/// # Errors
/// Path errors as [`item`]'s; `not_found` for a missing `from`; `exists`
/// for a taken `to` (unless `replace`); `bad_request` for a folder moved
/// into itself.
pub fn rename(root: &Path, from: &str, to: &str, replace: bool, trash: Option<&Path>) -> Result<FilesDone, RpcError> {
    let canonical = canonical_root(root)?;
    let (from_rel, src) = item(root, from)?;
    src.symlink_metadata().map_err(|e| op_error(&from_rel, &e))?;
    let (to_rel, dst) = item(root, to)?;
    if src == dst {
        return Ok(FilesDone { done: vec![FileMoved { from: from_rel, to: to_rel }], failed: Vec::new() });
    }
    if is_real_dir(&src) && dst.starts_with(&src) {
        return Err(RpcError::bad_request(format!("{from_rel} can't go inside itself")));
    }
    if dst.symlink_metadata().is_ok() && !same_file(&src, &dst) {
        if !replace {
            return Err(RpcError::new(RpcError::EXISTS, format!("{to_rel} already exists")));
        }
        trash_item(&dst, &to_rel, trash)?;
    }
    move_item(&src, &dst).map_err(|e| op_error(&from_rel, &e))?;
    let to = relative(&canonical, &dst).unwrap_or(to_rel);
    Ok(FilesDone { done: vec![FileMoved { from: from_rel, to }], failed: Vec::new() })
}

/// `files.copy` (`copy`) and `files.move`.
///
/// # Errors
/// `bad_request` for too many items or a destination that is no folder;
/// path errors for the destination. Each item's own failure is in
/// [`FilesDone::failed`].
pub fn transfer(root: &Path, params: &FilesTransferParams, copy: bool, trash: Option<&Path>) -> Result<FilesDone, RpcError> {
    if params.from.len() > FilesTransferParams::MAX_ITEMS {
        return Err(RpcError::bad_request(format!("at most {} items at once", FilesTransferParams::MAX_ITEMS)));
    }
    let canonical = canonical_root(root)?;
    let dir = folder(root, &params.to_dir)?;
    let mut out = FilesDone::default();
    for from in &params.from {
        match transfer_one(root, &dir, from, copy, params.replace, trash) {
            Ok(dst) => out.done.push(FileMoved { from: from.clone(), to: relative(&canonical, &dst).unwrap_or_default() }),
            Err(error) => out.failed.push(FileFailed { path: from.clone(), code: error.code, message: error.message }),
        }
    }
    Ok(out)
}

fn transfer_one(root: &Path, dir: &Path, from: &str, copy: bool, replace: bool, trash: Option<&Path>) -> Result<PathBuf, RpcError> {
    let (rel, src) = item(root, from)?;
    src.symlink_metadata().map_err(|e| op_error(&rel, &e))?;
    let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let same_dir = src.parent() == Some(dir);
    if same_dir && !copy {
        return Ok(src);
    }
    if is_real_dir(&src) && dir.starts_with(&src) {
        return Err(RpcError::bad_request(format!("{rel} can't go inside itself")));
    }
    let dst = if same_dir {
        copy_name(dir, &name)
    } else {
        let dst = dir.join(&name);
        match dst.symlink_metadata() {
            Ok(_) if replace => {
                trash_item(&dst, &name, trash)?;
                dst
            }
            Ok(_) => free(dir, &name),
            Err(_) => dst,
        }
    };
    if copy {
        if let Err(error) = copy_tree(&src, &dst) {
            // Only what this copy made goes.
            if dst.symlink_metadata().is_ok() {
                let _ = if is_real_dir(&dst) { std::fs::remove_dir_all(&dst) } else { std::fs::remove_file(&dst) };
            }
            return Err(op_error(&rel, &error));
        }
    } else {
        move_item(&src, &dst).map_err(|e| op_error(&rel, &e))?;
    }
    Ok(dst)
}

/// Finder's Duplicate name: `name copy.ext`, then `name copy 2.ext`…
pub fn copy_name(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = split_ext(name);
    let first = dir.join(format!("{stem} copy{ext}"));
    if first.symlink_metadata().is_err() {
        return first;
    }
    (2..).map(|n| dir.join(format!("{stem} copy {n}{ext}"))).find(|p| p.symlink_metadata().is_err()).expect("a free name")
}

/// Copy `src` to the new path `dst`: folders recursively, symlinks as
/// links, files with their permissions; other kinds are skipped.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = src.symlink_metadata()?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
    } else if kind.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        std::fs::set_permissions(dst, meta.permissions())
    } else if kind.is_file() {
        if dst.symlink_metadata().is_ok() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        std::fs::copy(src, dst).map(drop)
    } else {
        Ok(())
    }
}

/// Rename `src` to `dst`; across file systems, copy then remove `src`.
fn move_item(src: &Path, dst: &Path) -> io::Result<()> {
    match std::fs::rename(src, dst) {
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            copy_tree(src, dst)?;
            if is_real_dir(src) { std::fs::remove_dir_all(src) } else { std::fs::remove_file(src) }
        }
        other => other,
    }
}

/// `files.trash`.
///
/// # Errors
/// `bad_request` for too many items; each item's own failure is in
/// [`FilesDone::failed`].
pub fn trash(root: &Path, paths: &[String], trash: Option<&Path>) -> Result<FilesDone, RpcError> {
    if paths.len() > FilesTransferParams::MAX_ITEMS {
        return Err(RpcError::bad_request(format!("at most {} items at once", FilesTransferParams::MAX_ITEMS)));
    }
    let mut out = FilesDone::default();
    for path in paths {
        let result = item(root, path).and_then(|(rel, at)| {
            at.symlink_metadata().map_err(|e| op_error(&rel, &e))?;
            trash_item(&at, &rel, trash)
        });
        match result {
            Ok(()) => out.done.push(FileMoved { from: path.clone(), to: String::new() }),
            Err(error) => out.failed.push(FileFailed { path: path.clone(), code: error.code, message: error.message }),
        }
    }
    Ok(out)
}

/// The folders (relative to the root) an operation's result changed: each
/// item's old and new folder, and the items themselves.
pub fn touched(done: &FilesDone) -> Vec<String> {
    let parent = |p: &str| p.rsplit_once('/').map_or(String::new(), |(d, _)| d.to_owned());
    let mut out: Vec<String> = Vec::new();
    for item in &done.done {
        for path in [&item.from, &item.to] {
            if path.is_empty() {
                continue;
            }
            out.push(path.clone());
            out.push(parent(path));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicates_are_named_like_finder() {
        let dir = std::env::temp_dir().join(format!("silo-copy-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(copy_name(&dir, "a.txt"), dir.join("a copy.txt"));
        std::fs::write(dir.join("a copy.txt"), b"").unwrap();
        assert_eq!(copy_name(&dir, "a.txt"), dir.join("a copy 2.txt"));
        assert_eq!(copy_name(&dir, "src"), dir.join("src copy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn touched_lists_items_and_their_folders() {
        let done = FilesDone {
            done: vec![FileMoved { from: "a/b.txt".into(), to: "c/b.txt".into() }, FileMoved { from: String::new(), to: "d".into() }],
            failed: Vec::new(),
        };
        assert_eq!(touched(&done), ["", "a", "a/b.txt", "c", "c/b.txt", "d"]);
    }
}
