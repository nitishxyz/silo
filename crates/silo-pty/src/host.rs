//! The PTY host: a tiny parent process for each program, so whichever
//! server owns the PTY learns the program's wait status through a pipe,
//! even after a live upgrade made another process the PTY's owner.
//!
//! The server execs its own binary as
//! `<exe> pty-host <status fd> <slave fd> <program> <arg0> [args...]`.
//! Any binary linking this crate turns into the host before `main` runs
//! (an init-array/`__mod_init_func` constructor), so `silo` and in-process
//! test servers behave alike. The host ignores SIGHUP/SIGINT/SIGTERM, forks
//! the program as the session leader of the PTY, and writes [`Record`]s to
//! the status fd: [`Record::Started`] or [`Record::Failed`], then
//! [`Record::Exited`] once the program was reaped.

use std::{
    ffi::{CStr, c_char, c_int},
    io,
    os::fd::{AsRawFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) const ARG: &str = "pty-host";
pub(crate) const RECORD_LEN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    /// The program runs with this pid.
    Started(i32),
    /// It could not be started (errno).
    Failed(i32),
    /// It was reaped with this raw wait status.
    Exited(i32),
}

impl Record {
    pub(crate) fn encode(self) -> [u8; RECORD_LEN] {
        let (tag, value) = match self {
            Self::Started(v) => (1u32, v),
            Self::Failed(v) => (2, v),
            Self::Exited(v) => (3, v),
        };
        let mut out = [0u8; RECORD_LEN];
        out[..4].copy_from_slice(&tag.to_le_bytes());
        out[4..].copy_from_slice(&value.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: [u8; RECORD_LEN]) -> Option<Self> {
        let (tag, value) = bytes.split_at(4);
        let value = i32::from_le_bytes(value.try_into().ok()?);
        match u32::from_le_bytes(tag.try_into().ok()?) {
            1 => Some(Self::Started(value)),
            2 => Some(Self::Failed(value)),
            3 => Some(Self::Exited(value)),
            _ => None,
        }
    }
}

/// The executable to run as host: this very binary.
pub(crate) fn exe() -> io::Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        // Still valid when the binary was replaced on disk.
        Ok("/proc/self/exe".into())
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe()
    }
}

/// Read one record, waiting at most `timeout` for it to start; `None` at
/// EOF or on a malformed record.
pub(crate) fn read_within(fd: &OwnedFd, timeout: std::time::Duration) -> io::Result<Option<Record>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "pty host did not report"));
        }
        let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: poll reads/writes one pollfd.
        let n = unsafe { libc::poll(&raw mut pfd, 1, left.as_millis().clamp(1, i32::MAX as u128) as c_int) };
        if n > 0 {
            return read(fd);
        }
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
}

/// Read one record; `None` at EOF or on a malformed record.
pub(crate) fn read(fd: &OwnedFd) -> io::Result<Option<Record>> {
    let mut buf = [0u8; RECORD_LEN];
    let mut filled = 0;
    while filled < RECORD_LEN {
        // SAFETY: read writes at most the remaining bytes of `buf`.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf[filled..].as_mut_ptr().cast(), RECORD_LEN - filled) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if n == 0 {
            return Ok(None);
        }
        filled += n as usize;
    }
    Ok(Record::decode(buf))
}

/// Block until the program's exit is reported (`Some` with its raw wait
/// status, or `None` if the host died without one) or `stop` is set
/// (`None` outer).
pub(crate) fn wait(fd: &OwnedFd, stop: &Arc<AtomicBool>) -> Option<Option<i32>> {
    while !stop.load(Ordering::Acquire) {
        let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: poll reads/writes one pollfd.
        let n = unsafe { libc::poll(&raw mut pfd, 1, 100) };
        if n == 0 || (n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted) {
            continue;
        }
        return Some(match read(fd) {
            Ok(Some(Record::Exited(status))) => Some(status),
            _ => None,
        });
    }
    None
}

#[used]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
pub(crate) static INIT: unsafe extern "C" fn(c_int, *const *const c_char, *const *const c_char) = init;

unsafe extern "C" fn init(argc: c_int, argv: *const *const c_char, _envp: *const *const c_char) {
    // SAFETY: the loader passes the process's argc/argv.
    unsafe {
        if argc >= 6 && !argv.is_null() && CStr::from_ptr(*argv.add(1)).to_bytes() == ARG.as_bytes() {
            libc::_exit(run(argv));
        }
    }
}

fn fd_arg(arg: *const c_char) -> Option<c_int> {
    // SAFETY: argv entries are NUL-terminated strings.
    unsafe { CStr::from_ptr(arg) }.to_str().ok()?.parse().ok()
}

fn errno() -> c_int {
    io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

/// Close every descriptor from 3 up except the two in `keep` (which may be
/// the same).
unsafe fn close_inherited(keep: [c_int; 2]) {
    // SAFETY: only closes descriptors of this single-threaded process.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let close_range =
                |first: c_int, last: c_int| first > last || libc::syscall(libc::SYS_close_range, first as libc::c_uint, last as libc::c_uint, 0) == 0;
            let (low, high) = (keep[0].min(keep[1]).max(2), keep[0].max(keep[1]).max(2));
            if close_range(3, low - 1) && close_range(low + 1, high - 1) && close_range(high + 1, c_int::MAX) {
                return;
            }
            for fd in 3..libc::getdtablesize() {
                if !keep.contains(&fd) {
                    libc::close(fd);
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            let mut fds: [libc::proc_fdinfo; 1024] = std::mem::zeroed();
            let size = size_of_val(&fds) as c_int;
            loop {
                let n = libc::proc_pidinfo(libc::getpid(), libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size);
                let count = usize::try_from(n).unwrap_or(0) / size_of::<libc::proc_fdinfo>();
                let mut closed = false;
                for info in &fds[..count] {
                    if info.proc_fd >= 3 && !keep.contains(&info.proc_fd) {
                        libc::close(info.proc_fd);
                        closed = true;
                    }
                }
                if !closed || count < fds.len() {
                    break;
                }
            }
        }
    }
}
fn report(fd: c_int, record: Record) {
    let bytes = record.encode();
    // SAFETY: writes from a stack buffer; RECORD_LEN < PIPE_BUF is atomic.
    unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
}

const RESET: [c_int; 6] = [libc::SIGPIPE, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP, libc::SIGCHLD];

/// The host proper; returns its exit code.
unsafe fn run(argv: *const *const c_char) -> c_int {
    // SAFETY: argc >= 6 was checked; the program's argv starts at index 5
    // and is NULL-terminated like ours.
    unsafe {
        let (Some(status), Some(slave)) = (fd_arg(*argv.add(2)), fd_arg(*argv.add(3))) else { return 2 };
        // Anything else inherited (a descriptor another thread of the
        // spawning process had not marked close-on-exec yet) must not live
        // as long as the program: a socket held here would outlive its
        // owner's close.
        close_inherited([status, slave]);
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
            libc::signal(signal, libc::SIG_IGN);
        }
        let mut exec_err = [0 as c_int; 2];
        if libc::pipe(exec_err.as_mut_ptr()) < 0 || libc::fcntl(exec_err[1], libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            report(status, Record::Failed(errno()));
            return 1;
        }
        let pid = libc::fork();
        if pid < 0 {
            report(status, Record::Failed(errno()));
            return 1;
        }
        if pid == 0 {
            libc::close(exec_err[0]);
            libc::close(status);
            for signal in RESET {
                libc::signal(signal, libc::SIG_DFL);
            }
            if libc::setsid() >= 0
                && libc::ioctl(slave, libc::TIOCSCTTY as _, 0) >= 0
                && libc::dup2(slave, 0) >= 0
                && libc::dup2(slave, 1) >= 0
                && libc::dup2(slave, 2) >= 0
            {
                if slave > 2 {
                    libc::close(slave);
                }
                close_inherited([exec_err[1]; 2]);
                libc::execvp(*argv.add(4), argv.add(5));
            }
            let e = errno();
            libc::write(exec_err[1], (&raw const e).cast(), size_of::<c_int>());
            libc::_exit(127);
        }
        libc::close(exec_err[1]);
        libc::close(slave);
        let mut e: c_int = 0;
        let n = loop {
            let n = libc::read(exec_err[0], (&raw mut e).cast(), size_of::<c_int>());
            if n >= 0 || errno() != libc::EINTR {
                break n;
            }
        };
        libc::close(exec_err[0]);
        let started = n == 0;
        report(status, if started { Record::Started(pid) } else { Record::Failed(e) });
        let mut wstatus = 0;
        while libc::waitpid(pid, &raw mut wstatus, 0) < 0 {
            if errno() != libc::EINTR {
                return 1;
            }
        }
        if started {
            report(status, Record::Exited(wstatus));
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        for record in [Record::Started(4242), Record::Failed(libc::ENOENT), Record::Exited(3 << 8), Record::Exited(libc::SIGKILL)] {
            assert_eq!(Record::decode(record.encode()), Some(record));
        }
        assert_eq!(Record::Exited(0x0300).encode(), [3, 0, 0, 0, 0, 3, 0, 0]);
        assert_eq!(Record::decode([9, 0, 0, 0, 0, 0, 0, 0]), None);
    }
}
