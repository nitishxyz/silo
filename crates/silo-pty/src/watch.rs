//! Exit detection for processes that are not our children: a pidfd on Linux,
//! kqueue `EVFILT_PROC`/`NOTE_EXIT` on macOS. Both work for non-children and
//! fire when the process exits, even while it is an unreaped zombie.

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// How often a waiter re-checks its stop flag.
const TICK: Duration = Duration::from_millis(100);

/// A registered exit watch on one process.
pub(crate) struct Watch {
    /// `None` once the process was already gone at registration.
    fd: Option<OwnedFd>,
}

impl Watch {
    /// Start watching `pid`. A process that no longer exists yields a watch
    /// that reports exit immediately.
    pub(crate) fn new(pid: i32) -> io::Result<Self> {
        match register(pid) {
            Ok(fd) => Ok(Self { fd: Some(fd) }),
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(Self { fd: None }),
            Err(e) => Err(e),
        }
    }

    /// Blocks until the process exits (true) or `stop` is set (false).
    /// A wait error other than EINTR is treated as exit.
    pub(crate) fn wait(&self, stop: &Arc<AtomicBool>) -> bool {
        let Some(fd) = &self.fd else { return true };
        while !stop.load(Ordering::Acquire) {
            match ready(fd) {
                Ok(true) => return true,
                Ok(false) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return true,
            }
        }
        false
    }
}

#[cfg(target_os = "linux")]
fn register(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open returns a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is new and owned by us (pidfds are CLOEXEC).
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// A pidfd becomes readable once the process exits.
#[cfg(target_os = "linux")]
fn ready(fd: &OwnedFd) -> io::Result<bool> {
    let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: poll reads/writes one pollfd.
    let n = unsafe { libc::poll(&raw mut pfd, 1, TICK.as_millis() as libc::c_int) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n > 0)
}

#[cfg(target_os = "macos")]
fn register(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: kqueue returns a new descriptor or -1; kevent reads one change.
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let kq = OwnedFd::from_raw_fd(kq);
        rustix::io::fcntl_setfd(&kq, rustix::io::FdFlags::CLOEXEC)?;
        let mut change: libc::kevent = std::mem::zeroed();
        change.ident = pid as usize;
        change.filter = libc::EVFILT_PROC;
        change.flags = libc::EV_ADD | libc::EV_ONESHOT;
        change.fflags = libc::NOTE_EXIT;
        if libc::kevent(kq.as_raw_fd(), &raw const change, 1, std::ptr::null_mut(), 0, std::ptr::null()) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(kq)
    }
}

#[cfg(target_os = "macos")]
fn ready(kq: &OwnedFd) -> io::Result<bool> {
    let timeout = libc::timespec { tv_sec: 0, tv_nsec: TICK.as_nanos() as libc::c_long };
    // SAFETY: kevent writes at most one event into the zeroed struct.
    unsafe {
        let mut event: libc::kevent = std::mem::zeroed();
        let n = libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &raw mut event, 1, &raw const timeout);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n > 0)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Command;

    fn stop() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn pidfd_reports_exit_of_unreaped_child() {
        let mut child = Command::new("/bin/sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let watch = Watch::new(pid).unwrap();
        // The child stays a zombie until `wait` below; exit must still fire.
        assert!(watch.wait(&stop()));
        child.wait().unwrap();
    }

    #[test]
    fn pidfd_wait_returns_false_when_stopped() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let watch = Watch::new(i32::try_from(child.id()).unwrap()).unwrap();
        let flag = stop();
        flag.store(true, Ordering::Release);
        assert!(!watch.wait(&flag));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn pidfd_reports_exit_of_non_child() {
        // The shell's background sleep is our grandchild, not our child.
        let out = Command::new("/bin/sh").args(["-c", "sleep 0.3 >/dev/null 2>&1 & echo $!"]).output().unwrap();
        let pid: i32 = String::from_utf8(out.stdout).unwrap().trim().parse().unwrap();
        let watch = Watch::new(pid).unwrap();
        assert!(watch.wait(&stop()));
    }

    #[test]
    fn missing_process_counts_as_exited() {
        let mut child = Command::new("/bin/sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        child.wait().unwrap();
        assert!(Watch::new(pid).unwrap().wait(&stop()));
    }
}
