//! Moving items to the host's Trash, never deleting them: macOS asks
//! `NSFileManager` (the Finder's Trash, with Put Back), falling back to a
//! rename into `~/.Trash`; elsewhere the XDG trash (`$XDG_DATA_HOME/Trash`,
//! or `<mount>/.Trash-<uid>` for another file system) with its
//! `.trashinfo`. A fixed folder (`Config::trash`, for tests) takes the XDG
//! layout on every platform.

use std::{
    io,
    path::{Path, PathBuf},
};

/// Move `path` (an absolute path; a symlink is moved itself) to the Trash:
/// `dir`'s XDG layout when given, else the host's.
///
/// # Errors
/// The item is missing, or no trash would take it.
pub fn trash(path: &Path, dir: Option<&Path>) -> io::Result<()> {
    path.symlink_metadata()?;
    if let Some(dir) = dir {
        return into_xdg(dir, path, None);
    }
    host(path)
}

#[cfg(target_os = "macos")]
fn host(path: &Path) -> io::Result<()> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    match NSFileManager::defaultManager().trashItemAtURL_resultingItemURL_error(&url, None) {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::debug!(path = %path.display(), "trashItemAtURL: {}; renaming into ~/.Trash", error.localizedDescription());
            let trash = silo_pty::home_dir().join(".Trash");
            std::fs::create_dir_all(&trash)?;
            let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no name"))?;
            let target = free(&trash, &name.to_string_lossy());
            std::fs::rename(path, target)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn host(path: &Path) -> io::Result<()> {
    let home =
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| silo_pty::home_dir().join(".local/share"));
    match into_xdg(&home.join("Trash"), path, None) {
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            let top = mount_point(path)?;
            // SAFETY: getuid has no preconditions.
            let uid = unsafe { libc::getuid() };
            into_xdg(&top.join(format!(".Trash-{uid}")), path, Some(&top))
        }
        other => other,
    }
}

/// The top of the file system holding `path`.
#[cfg(not(target_os = "macos"))]
fn mount_point(path: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    let dev = path.symlink_metadata()?.dev();
    let mut top = path.parent().unwrap_or(path).to_path_buf();
    while let Some(up) = top.parent() {
        if up.metadata().map(|m| m.dev()) != Ok(dev) {
            break;
        }
        top = up.to_path_buf();
    }
    Ok(top)
}

/// `name`, else `name 2`, `name 3`… (before the extension), free in `dir`.
pub fn free(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if path.symlink_metadata().is_err() {
        return path;
    }
    (2..).map(|n| dir.join(numbered(name, n))).find(|p| p.symlink_metadata().is_err()).expect("a free name")
}

/// `name` with ` n` before its extension (`a.txt` → `a 2.txt`, `.env` →
/// `.env 2`).
pub fn numbered(name: &str, n: u32) -> String {
    let (stem, ext) = split_ext(name);
    format!("{stem} {n}{ext}")
}

/// `(stem, ".ext")`; a leading dot is no extension.
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(at) if at > 0 => name.split_at(at),
        _ => (name, ""),
    }
}

/// Move `path` into XDG trash `trash` (`files/` and `info/`), recording
/// where it was: absolute, or relative to `top` for a mount's own trash.
fn into_xdg(trash: &Path, path: &Path, top: Option<&Path>) -> io::Result<()> {
    use std::{io::Write as _, os::unix::fs::DirBuilderExt as _};
    let (files, info) = (trash.join("files"), trash.join("info"));
    for dir in [&files, &info] {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no name"))?.to_string_lossy().into_owned();
    let recorded = top.and_then(|t| path.strip_prefix(t).ok()).unwrap_or(path);
    let body = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", encode(recorded), now_local());
    for n in 1..10_000u32 {
        let candidate = if n == 1 { name.clone() } else { numbered(&name, n) };
        let record = info.join(format!("{candidate}.trashinfo"));
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&record) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let target = files.join(&candidate);
        if target.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&record);
            continue;
        }
        let moved = file.write_all(body.as_bytes()).and_then(|()| std::fs::rename(path, &target));
        if let Err(error) = moved {
            let _ = std::fs::remove_file(&record);
            return Err(error);
        }
        return Ok(());
    }
    Err(io::Error::other("the trash has too many items of that name"))
}

/// A path as the trash spec records it: percent-encoded bytes, `/` kept.
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Local time as `YYYY-MM-DDThh:mm:ss`.
fn now_local() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let t = libc::time_t::try_from(now).unwrap_or_default();
    // SAFETY: `tm` is plain data written by localtime_r, which reads `t`.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&raw const t, &raw mut tm);
        tm
    };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_names_keep_the_extension() {
        assert_eq!(numbered("a.txt", 2), "a 2.txt");
        assert_eq!(numbered("archive.tar.gz", 3), "archive.tar 3.gz");
        assert_eq!(numbered(".env", 2), ".env 2");
        assert_eq!(numbered("folder", 2), "folder 2");
        assert_eq!(encode(Path::new("/a b/é%")), "/a%20b/%C3%A9%25");
    }

    #[test]
    fn xdg_trash_records_where_items_were() {
        let dir = std::env::temp_dir().join(format!("silo-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/sub")).unwrap();
        for _ in 0..2 {
            std::fs::write(dir.join("src/a.txt"), b"x").unwrap();
            trash(&dir.join("src/a.txt"), Some(&dir.join("Trash"))).unwrap();
        }
        trash(&dir.join("src/sub"), Some(&dir.join("Trash"))).unwrap();
        assert!(dir.join("Trash/files/a.txt").is_file() && dir.join("Trash/files/a 2.txt").is_file());
        assert!(dir.join("Trash/files/sub").is_dir());
        let info = std::fs::read_to_string(dir.join("Trash/info/a 2.txt.trashinfo")).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath=/") && info.contains("/src/a.txt\nDeletionDate=20"), "{info}");
        assert!(trash(&dir.join("src/gone"), Some(&dir.join("Trash"))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
