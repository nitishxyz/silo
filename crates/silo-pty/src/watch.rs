//! Exit detection for processes that are not our children.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const TICK: Duration = Duration::from_millis(100);

/// True while `pid` exists (zombies count as alive on Linux).
pub(crate) fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks existence and permission.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Blocks until `pid` exits (returns true) or `stop` is set (false).
pub(crate) fn wait_exit(pid: i32, stop: &Arc<AtomicBool>) -> bool {
    #[cfg(target_os = "macos")]
    if let Some(exited) = kqueue_wait(pid, stop) {
        return exited;
    }
    while !stop.load(Ordering::Acquire) {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(TICK);
    }
    false
}

/// `EVFILT_PROC`/`NOTE_EXIT` works for non-children and fires for zombies.
/// `None` means kqueue was unavailable and the caller should poll.
#[cfg(target_os = "macos")]
fn kqueue_wait(pid: i32, stop: &Arc<AtomicBool>) -> Option<bool> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // SAFETY: kqueue returns a new descriptor or -1; kevent reads/writes the
    // given arrays only.
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 {
            return None;
        }
        let kq = OwnedFd::from_raw_fd(kq);
        let mut change: libc::kevent = std::mem::zeroed();
        change.ident = pid as usize;
        change.filter = libc::EVFILT_PROC;
        change.flags = libc::EV_ADD | libc::EV_ONESHOT;
        change.fflags = libc::NOTE_EXIT;
        if libc::kevent(kq.as_raw_fd(), &raw const change, 1, std::ptr::null_mut(), 0, std::ptr::null()) < 0 {
            return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                Some(true)
            } else {
                None
            };
        }
        let timeout = libc::timespec { tv_sec: 0, tv_nsec: TICK.as_nanos() as libc::c_long };
        while !stop.load(Ordering::Acquire) {
            let mut event: libc::kevent = std::mem::zeroed();
            let n = libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &raw mut event, 1, &raw const timeout);
            if n > 0 {
                return Some(true);
            }
            if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return None;
            }
        }
        Some(false)
    }
}
