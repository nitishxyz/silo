//! The server's log, `<SILO_HOME>/server.log`, kept to a size: launchd,
//! systemd or the daemon start point the server's stdout and stderr at it,
//! and nothing else ever trims it. Past [`CAP`] the server moves it to
//! `server.log.old` (replacing the previous one) and points both at a new
//! file, as the app does with `app.log`.

use std::{
    os::fd::{AsRawFd, RawFd},
    path::Path,
    time::Duration,
};

const CAP: u64 = 20 * 1024 * 1024;
const EVERY: Duration = Duration::from_secs(60);

/// Watch the log while the server runs, when its stderr is the log.
pub fn spawn(home: &Path) {
    let path = silo_proto::paths::log(home);
    tokio::spawn(async move {
        loop {
            match rotate(&path, CAP, &[1, 2]) {
                Ok(true) => tracing::info!(log = %path.display(), "log rotated: the previous one is server.log.old"),
                Ok(false) => {}
                Err(error) => tracing::debug!("log rotation: {error}"),
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// When `fds[0]` writes to `path` and it has grown past `cap`: rename it to
/// `<path>.old`, open a new one, and point every fd in `fds` at it.
/// Whether it did. Another process's log, or one already moved, is left alone.
fn rotate(path: &Path, cap: u64, fds: &[RawFd]) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let Some(&first) = fds.first() else { return Ok(false) };
    let Ok(meta) = std::fs::metadata(path) else { return Ok(false) };
    // SAFETY: fstat on an fd number only reads its status into `st`.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(first, &raw mut st) } != 0 {
        return Ok(false);
    }
    #[allow(clippy::cast_sign_loss, clippy::useless_conversion)]
    let same = u64::from(st.st_ino) == meta.ino() && st.st_dev as u64 == meta.dev();
    if !same || meta.len() <= cap {
        return Ok(false);
    }
    std::fs::rename(path, path.with_extension("log.old"))?;
    let fresh = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    for &fd in fds {
        // SAFETY: dup2 replaces `fd` with a copy of the open log; `fresh` stays valid until dropped after.
        if unsafe { libc::dup2(fresh.as_raw_fd(), fd) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::{io::Write, os::fd::IntoRawFd};

    use super::*;

    #[test]
    fn a_log_past_its_cap_starts_over_and_keeps_the_last_one() {
        let dir = std::env::temp_dir().join(format!("silo-log-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.log");
        // As launchd does: the process's output opened on the log.
        let fd = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap().into_raw_fd();
        let write = |text: &str| {
            // SAFETY: `fd` is open for the test's length; ManuallyDrop keeps it so.
            let mut f = std::mem::ManuallyDrop::new(unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) });
            f.write_all(text.as_bytes()).unwrap();
        };
        write("0123456789");
        assert!(!rotate(&path, 10, &[fd]).unwrap(), "at the cap: kept");
        write("x");
        assert!(rotate(&path, 10, &[fd]).unwrap());
        write("after\n");
        assert_eq!(std::fs::read_to_string(dir.join("server.log.old")).unwrap(), "0123456789x");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after\n", "the fd now writes to the new log");
        // An fd writing elsewhere (another process's log, one moved away) is left alone.
        std::fs::write(&path, "y".repeat(50)).unwrap();
        std::fs::rename(&path, dir.join("moved.log")).unwrap();
        std::fs::write(&path, "z".repeat(50)).unwrap();
        let other = std::fs::File::open(dir.join("moved.log")).unwrap();
        assert!(!rotate(&path, 10, &[other.as_raw_fd()]).unwrap());
        // SAFETY: closes the test's own fd once.
        unsafe { libc::close(fd) };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
