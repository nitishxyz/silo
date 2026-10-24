//! Path safety for files blocks: every path a client names is relative to
//! the block's root, and what it resolves to (symlinks followed) must stay
//! under the root's canonical form.

use std::path::{Component, Path, PathBuf};

use silo_proto::{control::RpcError, model::FilesState};

/// `rel` as stored: `/`-separated, without empty or `.` components and
/// without a trailing `/`; empty for the root.
///
/// # Errors
/// `bad_request` for an absolute path, control characters or an oversized
/// path; `forbidden` for a `..` component.
pub fn normalize(rel: &str) -> Result<String, RpcError> {
    if rel.len() > FilesState::PATH_MAX || rel.chars().any(char::is_control) {
        return Err(RpcError::bad_request(format!("paths must be at most {} printable bytes", FilesState::PATH_MAX)));
    }
    if rel.starts_with('/') || rel.starts_with('~') {
        return Err(RpcError::bad_request(format!("path {rel} must be relative to the block's root")));
    }
    let mut parts = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(escape(rel)),
            part => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

fn escape(rel: &str) -> RpcError {
    RpcError::denied(format!("path {rel} is outside the block's root"))
}

/// The canonical root.
///
/// # Errors
/// `not_found` when the root is gone.
pub fn canonical_root(root: &Path) -> Result<PathBuf, RpcError> {
    root.canonicalize().map_err(|e| RpcError::not_found(format!("root {} ({e})", root.display())))
}

/// `rel` under `root`, canonical (symlinks resolved).
///
/// # Errors
/// See [`normalize`]; `not_found` for a missing path; `forbidden` when it
/// resolves outside the root.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, RpcError> {
    let rel = normalize(rel)?;
    let root = canonical_root(root)?;
    let path = root.join(&rel).canonicalize().map_err(|e| RpcError::not_found(format!("{rel} ({e})")))?;
    if !inside(&root, &path) {
        return Err(escape(&rel));
    }
    Ok(path)
}

/// Whether canonical `path` is canonical `root` or under it.
pub fn inside(root: &Path, path: &Path) -> bool {
    path.starts_with(root) && !path.components().any(|c| c == Component::ParentDir)
}

/// `path` relative to `root`, `/`-separated (empty for the root itself);
/// `None` outside the root or for a name that is not UTF-8.
pub fn relative(root: &Path, path: &Path) -> Option<String> {
    let rest = path.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rest.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

/// A new root: `path` absolute (`~` is the home directory) or relative to
/// `current`; canonical, and a directory.
///
/// # Errors
/// `bad_request` when it is not a directory; `not_found` when missing.
pub fn new_root(current: Option<&Path>, path: &str) -> Result<PathBuf, RpcError> {
    if path.is_empty() || path.len() > FilesState::PATH_MAX || path.chars().any(char::is_control) {
        return Err(RpcError::bad_request(format!("root must be 1 to {} printable bytes", FilesState::PATH_MAX)));
    }
    let home = || silo_pty::home_dir();
    let joined = if path == "~" {
        home()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home().join(rest)
    } else if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        current.ok_or_else(|| RpcError::bad_request("root must be an absolute path"))?.join(path)
    };
    let root = joined.canonicalize().map_err(|e| RpcError::not_found(format!("{path} ({e})")))?;
    if !root.is_dir() {
        return Err(RpcError::bad_request(format!("{path} is not a directory")));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_cleans_and_refuses_escapes() {
        assert_eq!(normalize("").unwrap(), "");
        assert_eq!(normalize("./a//b/").unwrap(), "a/b");
        assert_eq!(normalize("a/../b").unwrap_err().code, RpcError::FORBIDDEN);
        assert_eq!(normalize("/etc").unwrap_err().code, "bad_request");
        assert_eq!(normalize("a\nb").unwrap_err().code, "bad_request");
        assert_eq!(relative(Path::new("/r"), Path::new("/r/a/b")).as_deref(), Some("a/b"));
        assert_eq!(relative(Path::new("/r"), Path::new("/r")).as_deref(), Some(""));
        assert_eq!(relative(Path::new("/r"), Path::new("/s/a")), None);
    }
}
