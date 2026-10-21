use crate::{Error, Result};
use std::path::Path;
/// Archived output left by older versions is retained for seven days.
pub(crate) const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub(crate) fn expire(state: &Path, id: &str, archived: bool, updated_at: i64, now: i64) -> Result<()> {
    if archived && now.saturating_sub(updated_at) > RETENTION_MS {
        remove(state, id)?;
    }
    Ok(())
}
pub(crate) fn remove(state: &Path, id: &str) -> Result<()> {
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return Err(Error::Invalid("invalid session output directory".into()));
    }
    let outputs = state.join("outputs");
    match std::fs::symlink_metadata(&outputs) {
        Ok(m) if m.file_type().is_symlink() => return Err(Error::Invalid("output root is a symlink".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        _ => {}
    }
    let path = outputs.join(id);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_symlink() || m.is_file() => std::fs::remove_file(path)?,
        Ok(_) => std::fs::remove_dir_all(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn retention_only_removes_old_archived_outputs() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["old", "recent", "active"] {
            std::fs::create_dir_all(dir.path().join("outputs").join(id)).unwrap();
        }
        super::expire(dir.path(), "old", true, 0, super::RETENTION_MS + 1).unwrap();
        super::expire(dir.path(), "recent", true, 1, super::RETENTION_MS + 1).unwrap();
        super::expire(dir.path(), "active", false, 0, super::RETENTION_MS + 1).unwrap();
        assert!(!dir.path().join("outputs/old").exists());
        assert!(dir.path().join("outputs/recent").exists());
        assert!(dir.path().join("outputs/active").exists());
    }
}
