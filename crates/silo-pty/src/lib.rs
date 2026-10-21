//! PTY allocation and child supervision (macOS and Linux).

mod cloexec;
pub mod handoff;
mod host;
mod threads;
mod watch;

pub use threads::{PtyEvent, PtyInput};

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::PathBuf,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use threads::IoThreads;

/// Longest wait for the PTY host to report that the program started.
const HOST_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

use rustix::{
    fs::{Mode, OFlags, open},
    pty::{grantpt, ptsname, unlockpt},
    termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtySize {
    pub columns: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

#[derive(Clone, Debug, Default)]
pub struct SpawnConfig {
    /// Program and arguments; empty means the user's login shell.
    pub command: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
}

/// The user's shell: `$SHELL`, else the passwd entry, else `/bin/zsh`.
#[must_use]
pub fn user_shell() -> PathBuf {
    if let Some(shell) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
        return shell.into();
    }
    // SAFETY: getpwuid returns a pointer into static storage or NULL; the
    // shell string is copied before any other passwd call.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            let shell = std::ffi::CStr::from_ptr((*pw).pw_shell);
            if let Ok(shell) = shell.to_str()
                && !shell.is_empty()
            {
                return shell.into();
            }
        }
    }
    PathBuf::from("/bin/zsh")
}

/// The user's home directory.
#[must_use]
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

struct ChildState {
    pid: i32,
    exited: AtomicBool,
    status: Mutex<Option<ExitStatus>>,
    /// Cleared once exit was reported or the PTY was detached.
    notify: AtomicBool,
    /// Stops the exit watcher of an adopted process.
    stop: Arc<AtomicBool>,
}

impl ChildState {
    fn new(pid: i32) -> Arc<Self> {
        Arc::new(Self {
            pid,
            exited: AtomicBool::new(false),
            status: Mutex::new(None),
            notify: AtomicBool::new(true),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    fn finish(&self, status: Option<ExitStatus>, on_exit: impl FnOnce(Option<ExitStatus>)) {
        if let Ok(mut slot) = self.status.lock() {
            *slot = status;
        }
        self.exited.store(true, Ordering::Release);
        if self.notify.swap(false, Ordering::AcqRel) {
            on_exit(status);
        }
    }
}

/// Report the program's exit from its PTY host's status pipe.
fn watch_status(
    state: &Arc<ChildState>,
    status: &Arc<OwnedFd>,
    exit: Option<i32>,
    on_exit: impl FnOnce(Option<ExitStatus>) + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    let (worker, status) = (state.clone(), status.clone());
    std::thread::Builder::new().name("silo-pty-status".into()).spawn(move || {
        if let Some(raw) = exit.map(Some).or_else(|| host::wait(&status, &worker.stop)) {
            worker.finish(raw.map(ExitStatus::from_raw), on_exit);
        }
    })
}

/// A PTY and its process. Dropping it hangs up the process group; use
/// [`Pty::detach`] to hand the process to another owner instead.
pub struct Pty {
    master: Arc<OwnedFd>,
    /// Read end of the PTY host's status pipe; `None` for programs started
    /// without a host, whose exit status is unknown.
    status: Option<Arc<OwnedFd>>,
    child: Arc<ChildState>,
    size: Mutex<PtySize>,
    io: Option<IoThreads>,
    watcher: Option<JoinHandle<()>>,
    detached: bool,
}

/// A PTY master released by [`Pty::detach`]; the process keeps running.
#[derive(Debug)]
pub struct Detached {
    pub master: OwnedFd,
    /// The PTY host's status pipe (see [`Pty::spawn`]).
    pub status: Option<OwnedFd>,
    /// Raw wait status already read from `status` before the detach.
    pub exit: Option<i32>,
    pub pid: i32,
    pub size: PtySize,
}

/// A failed [`Pty::adopt`]: the master is handed back so the caller can
/// retry or pass it on instead of hanging up the program.
#[derive(Debug)]
pub struct AdoptError {
    pub error: io::Error,
    pub detached: Detached,
}

impl std::fmt::Display for AdoptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "adopt pid {}: {}", self.detached.pid, self.error)
    }
}

impl std::error::Error for AdoptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl Pty {
    /// Spawn `config` on a new PTY under a PTY host (this executable run
    /// as `pty-host`), which stays the program's parent and reports its
    /// wait status through a pipe that travels with [`Pty::detach`].
    /// `on_exit` runs on a supervisor thread once the program was reaped,
    /// with `None` if the host died without reporting.
    ///
    /// # Errors
    /// PTY allocation or spawn failures.
    pub fn spawn(config: &SpawnConfig, size: PtySize, on_exit: impl FnOnce(Option<ExitStatus>) + Send + 'static) -> io::Result<Self> {
        // Keeps the host constructor linked into every binary that spawns.
        std::hint::black_box(&host::INIT);
        let master = open_master()?;
        grantpt(&master).map_err(step("grantpt"))?;
        unlockpt(&master).map_err(step("unlockpt"))?;
        let name = ptsname(&master, Vec::new()).map_err(step("ptsname"))?;
        let slave = open(name.as_c_str(), OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC, Mode::empty());
        let slave = File::from(slave.map_err(step("open the PTY slave"))?);
        set_size(&master, size).map_err(step("set the PTY size"))?;
        let (status, status_write) = cloexec::pipe().map_err(step("status pipe"))?;

        let (program, args, login) = if config.command.is_empty() {
            (user_shell().into_os_string(), Vec::new(), true)
        } else {
            (config.command[0].clone(), config.command[1..].to_vec(), false)
        };
        let arg0 = if login {
            // A leading dash makes the shell a login shell, so a daemon
            // started from a GUI app still gets the user's PATH.
            let base = std::path::Path::new(&program).file_name().unwrap_or(OsStr::new("sh"));
            let mut arg0 = OsString::from("-");
            arg0.push(base);
            arg0
        } else {
            program.clone()
        };
        let inherit = [status_write.as_raw_fd(), slave.as_raw_fd()];
        let mut command = Command::new(host::exe()?);
        command.arg(host::ARG).arg(inherit[0].to_string()).arg(inherit[1].to_string()).arg(&program).arg(arg0).args(&args);
        command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "silo")
            .env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"))
            // ssh forwards LC_* (TERM_PROGRAM is not): lets `silo` on the far
            // side know it runs inside Silo and offer its host.
            .env("LC_TERMINAL", "silo")
            .env_remove("COLUMNS")
            .env_remove("LINES")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if std::env::var_os("LANG").is_none() {
            command.env("LANG", "en_US.UTF-8");
        }
        for (key, value) in &config.env {
            command.env(key, value);
        }
        command.current_dir(config.cwd.clone().unwrap_or_else(home_dir));
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                for fd in inherit {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let spawned = {
            let _forking = cloexec::forking();
            command.spawn()
        };
        drop((slave, status_write));
        let mut host = spawned.map_err(step("start the PTY host"))?;
        let pid = match host::read_within(&status, HOST_START_TIMEOUT) {
            Ok(Some(host::Record::Started(pid))) => pid,
            other => {
                if other.is_err() {
                    let _ = host.kill();
                }
                let _ = host.wait();
                return Err(match other {
                    Ok(Some(host::Record::Failed(errno))) => step("start the program")(io::Error::from_raw_os_error(errno)),
                    Err(error) => error,
                    _ => io::Error::other("pty host failed to start the program"),
                });
            }
        };
        // The host is reaped here even after `detach`; it exits right after
        // reporting the program's status to whoever holds the pipe then.
        std::thread::Builder::new().name("silo-pty-host".into()).spawn(move || {
            let _ = host.wait();
        })?;
        let state = ChildState::new(pid);
        let status = Arc::new(status);
        let watcher = watch_status(&state, &status, None, on_exit)?;
        Ok(Self::new(master, Some(status), state, size, Some(watcher)))
    }

    fn new(master: OwnedFd, status: Option<Arc<OwnedFd>>, child: Arc<ChildState>, size: PtySize, watcher: Option<JoinHandle<()>>) -> Self {
        Self { master: Arc::new(master), status, child, size: Mutex::new(size), io: None, watcher, detached: false }
    }

    /// Wrap an inherited PTY master whose session leader `pid` is not our
    /// child (e.g. received over [`handoff`]). With a status pipe the exit
    /// status comes from its PTY host. Without one (a program started by
    /// an older build) exit is observed through a pidfd on Linux and kqueue
    /// `EVFILT_PROC` on macOS and the status is unknown, so `on_exit` gets
    /// `None`. Master EOF/EIO is reported separately as [`PtyEvent::Closed`].
    ///
    /// # Errors
    /// Fails if `pid` is invalid, the descriptor is not a usable terminal or
    /// a thread cannot be spawned. On failure the master is handed back
    /// unharmed in [`AdoptError`], so the program is not hung up.
    pub fn adopt(detached: Detached, on_exit: impl FnOnce(Option<ExitStatus>) + Send + 'static) -> Result<Self, AdoptError> {
        let Detached { master, status, exit, pid, size } = detached;
        let fail = |error, master, status| Err(AdoptError { error, detached: Detached { master, status, exit, pid, size } });
        if pid <= 0 {
            return fail(io::Error::new(io::ErrorKind::InvalidInput, "invalid pid"), master, status);
        }
        for fd in std::iter::once(&master).chain(&status) {
            if let Err(e) = rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC) {
                return fail(e.into(), master, status);
            }
        }
        if let Err(e) = set_size(&master, size) {
            return fail(e, master, status);
        }
        let state = ChildState::new(pid);
        if let Some(status) = status {
            let status = Arc::new(status);
            return match watch_status(&state, &status, exit, on_exit) {
                Ok(watcher) => Ok(Self::new(master, Some(status), state, size, Some(watcher))),
                Err(e) => fail(e, master, Arc::into_inner(status)),
            };
        }
        let watch = match watch::Watch::new(pid) {
            Ok(watch) => watch,
            Err(e) => return fail(e, master, None),
        };
        let worker = state.clone();
        let watcher = std::thread::Builder::new().name("silo-pty-watch".into()).spawn(move || {
            if watch.wait(&worker.stop) {
                worker.finish(None, on_exit);
            }
        });
        match watcher {
            Ok(watcher) => Ok(Self::new(master, None, state, size, Some(watcher))),
            Err(e) => fail(e, master, None),
        }
    }

    /// [`Pty::adopt`] followed by [`Pty::start_io`]; if either fails the
    /// master is handed back in [`AdoptError`].
    ///
    /// # Errors
    /// As [`Pty::adopt`] and [`Pty::start_io`].
    pub fn adopt_with_io(
        detached: Detached,
        name: &str,
        on_exit: impl FnOnce(Option<ExitStatus>) + Send + 'static,
        on_event: impl FnMut(PtyEvent) + Send + 'static,
    ) -> Result<(Self, PtyInput), AdoptError> {
        let mut pty = Self::adopt(detached, on_exit)?;
        match pty.start_io(name, on_event) {
            Ok(input) => Ok((pty, input)),
            Err(error) => Err(AdoptError { error, detached: pty.detach() }),
        }
    }

    /// Run owned reader/writer threads: output goes to `on_event`, input is
    /// queued through the returned [`PtyInput`]. These threads, unlike
    /// [`Pty::reader`]/[`Pty::writer`] handles, are stopped by
    /// [`Pty::detach`]. The master becomes non-blocking while they run.
    ///
    /// # Errors
    /// Fails if already started or a thread cannot be spawned.
    pub fn start_io(&mut self, name: &str, on_event: impl FnMut(PtyEvent) + Send + 'static) -> io::Result<PtyInput> {
        if self.io.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "pty io already started"));
        }
        let (threads, input) = IoThreads::start(self.master.clone(), name, on_event)?;
        self.io = Some(threads);
        Ok(input)
    }

    /// Stop and join the owned I/O threads and exit reporting, and release
    /// the master without hanging up the process. Handles from
    /// [`Pty::reader`]/[`Pty::writer`] are unaffected and should be closed
    /// by their owners.
    ///
    /// Once the threads are joined this Pty holds the only reference to the
    /// master and status pipe, so they are moved out, not duplicated.
    #[must_use]
    pub fn detach(mut self) -> Detached {
        self.detached = true;
        self.child.notify.store(false, Ordering::Release);
        self.stop_threads();
        let (master, status, pid, size) = (self.master.clone(), self.status.take(), self.child.pid, self.size());
        let exit = if self.child.exited.load(Ordering::Acquire) { self.exit_status().map(ExitStatusExt::into_raw) } else { None };
        drop(self);
        // Nothing else clones the Arc once the I/O threads are joined.
        let master = Arc::try_unwrap(master).expect("pty master still shared after detach");
        let status = status.map(|s| Arc::try_unwrap(s).expect("pty status still shared after detach"));
        Detached { master, status, exit, pid, size }
    }

    fn stop_threads(&mut self) {
        if let Some(io) = self.io.take() {
            io.stop();
        }
        self.child.stop.store(true, Ordering::Release);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }

    /// A handle for blocking reads of child output.
    ///
    /// # Errors
    /// Fails if the descriptor cannot be duplicated.
    pub fn reader(&self) -> io::Result<File> {
        Ok(File::from(self.master.try_clone()?))
    }

    /// A handle for blocking writes of input to the child.
    ///
    /// # Errors
    /// Fails if the descriptor cannot be duplicated.
    pub fn writer(&self) -> io::Result<File> {
        Ok(File::from(self.master.try_clone()?))
    }

    /// # Errors
    /// Fails if the kernel rejects the window size.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        set_size(&*self.master, size)?;
        if let Ok(mut slot) = self.size.lock() {
            *slot = size;
        }
        Ok(())
    }

    #[must_use]
    pub fn size(&self) -> PtySize {
        self.size.lock().map_or_else(|e| *e.into_inner(), |s| *s)
    }

    /// Whether the process was seen to exit.
    #[must_use]
    pub fn exited(&self) -> bool {
        self.child.exited.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn pid(&self) -> i32 {
        self.child.pid
    }

    #[must_use]
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.child.status.lock().ok().and_then(|s| *s)
    }

    /// Process group currently in the PTY foreground; `None` if there is
    /// none (e.g. the session leader died) or it cannot be queried.
    #[must_use]
    pub fn foreground_pid(&self) -> Option<i32> {
        // rustix's tcgetpgrp panics when the kernel reports pgrp 0, so call
        // libc directly. SAFETY: tcgetpgrp only reads the descriptor.
        let pgrp = unsafe { libc::tcgetpgrp(std::os::fd::AsRawFd::as_raw_fd(&*self.master)) };
        (pgrp > 0).then_some(pgrp)
    }

    /// The line discipline's `(ECHO, ICANON)` flags, read from the master
    /// (which reflects the slave's termios on macOS and Linux).
    ///
    /// # Errors
    /// `tcgetattr` failures.
    pub fn line_mode(&self) -> io::Result<(bool, bool)> {
        let local = tcgetattr(&*self.master)?.local_modes;
        Ok((local.contains(LocalModes::ECHO), local.contains(LocalModes::ICANON)))
    }

    /// Send `signal` to the child's process group unless it was reaped.
    pub fn signal(&self, signal: i32) {
        if !self.child.exited.load(Ordering::Acquire) {
            // SAFETY: kill takes no pointers. The PTY host reaps the program
            // before its status reaches `exited`, so between that reap and
            // the status being read the pid may already be reused and the
            // signal hit an unrelated process group.
            unsafe {
                libc::kill(-self.child.pid, signal);
            }
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if !self.detached && self.status.is_some() {
            // The status watcher outlives us to report the hangup's exit.
            if let Some(io) = self.io.take() {
                io.stop();
            }
            self.signal(libc::SIGHUP);
            return;
        }
        self.stop_threads();
        if !self.detached {
            self.signal(libc::SIGHUP);
        }
    }
}

/// Name the step of [`Pty::spawn`] that failed.
fn step<E: Into<io::Error>>(what: &'static str) -> impl FnOnce(E) -> io::Error {
    move |error| {
        let error = error.into();
        io::Error::new(error.kind(), format!("{what}: {error}"))
    }
}

/// `EREDRIVEOPEN`: xnu's "redrive the open" (`bsd/sys/errno.h`), which
/// escapes to userspace when two opens of `/dev/ptmx` pick the same free
/// minor number at once.
#[cfg(target_os = "macos")]
const EREDRIVEOPEN: i32 = -6;

/// A new PTY master, close-on-exec from the start. `posix_openpt` is
/// `open("/dev/ptmx")` on macOS and Linux, but macOS's takes no `O_CLOEXEC`.
///
/// Opens are serialized: on macOS concurrent opens race for the next minor
/// number and the loser fails with [`EREDRIVEOPEN`]. Another process can
/// still race us; that is reported by name, not as an unknown error.
fn open_master() -> io::Result<OwnedFd> {
    static OPENING: Mutex<()> = Mutex::new(());
    let _opening = OPENING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match open(c"/dev/ptmx", OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC, Mode::empty()) {
        Ok(master) => Ok(master),
        #[cfg(target_os = "macos")]
        Err(e) if e.raw_os_error() == EREDRIVEOPEN => {
            Err(io::Error::new(io::ErrorKind::ResourceBusy, "open /dev/ptmx: raced another process for a new PTY (EREDRIVEOPEN)"))
        }
        Err(e) => Err(e.into()),
    }
}

fn set_size(fd: impl std::os::fd::AsFd, size: PtySize) -> io::Result<()> {
    tcsetwinsize(
        fd,
        Winsize {
            ws_row: size.rows,
            ws_col: size.columns,
            ws_xpixel: size.columns.saturating_mul(size.cell_width),
            ws_ypixel: size.rows.saturating_mul(size.cell_height),
        },
    )?;
    Ok(())
}

/// Working directory of `pid`, if it can be determined.
#[must_use]
pub fn process_cwd(pid: i32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: proc_pidinfo fills at most `size` bytes of the zeroed struct.
        unsafe {
            let mut info: libc::proc_vnodepathinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
            let n = libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut info).cast(), size);
            if n != size {
                return None;
            }
            let path = std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast());
            path.to_str().ok().map(PathBuf::from)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
}

/// Short name of process `pid` (for titles).
#[must_use]
pub fn process_name(pid: i32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 256];
        // SAFETY: proc_name writes at most buf.len() bytes.
        let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if n <= 0 {
            return None;
        }
        std::str::from_utf8(&buf[..n as usize]).ok().map(str::to_owned)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_owned())
    }
}

/// The processes in process group `pgid` (its leader included).
#[must_use]
pub fn process_group(pgid: i32) -> Vec<i32> {
    #[cfg(target_os = "macos")]
    {
        let mut pids = vec![0 as libc::pid_t; 64];
        loop {
            let size = i32::try_from(pids.len() * std::mem::size_of::<libc::pid_t>()).unwrap_or(i32::MAX);
            // SAFETY: proc_listpgrppids writes at most `size` bytes of pids
            // and returns how many it wrote.
            let n = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), size) };
            let Ok(count) = usize::try_from(n) else {
                return Vec::new();
            };
            if count < pids.len() {
                pids.truncate(count);
                pids.retain(|&p| p > 0);
                return pids;
            }
            pids.resize(pids.len() * 2, 0);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
            .filter(|pid| {
                // `pid (comm) state ppid pgrp ...`; comm may hold anything.
                std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|stat| stat.rsplit_once(')')?.1.split_whitespace().nth(2)?.parse::<i32>().ok())
                    == Some(pgid)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::FromRawFd,
    };

    #[test]
    fn spawns_echoes_resizes_and_reports_exit() {
        let (tx, rx) = std::sync::mpsc::channel();
        let config =
            SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), "stty size; read x; echo got:$x; exit 3".into()], ..Default::default() };
        let size = PtySize { columns: 91, rows: 33, cell_width: 8, cell_height: 16 };
        let pty = Pty::spawn(&config, size, move |status| {
            let _ = tx.send(status);
        })
        .unwrap();
        let mut reader = pty.reader().unwrap();
        let mut writer = pty.writer().unwrap();
        writer.write_all(b"hello\n").unwrap();
        let mut output = Vec::new();
        let mut buf = [0u8; 1024];
        while !String::from_utf8_lossy(&output).contains("got:hello") {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
            }
        }
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("33 91"), "{text}");
        assert!(text.contains("got:hello"), "{text}");
        let status = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(status.and_then(|s| s.code()), Some(3));
        assert!(pty.exited());
    }

    #[test]
    fn program_inherits_only_stdio() {
        // SAFETY: dup returns a new descriptor without CLOEXEC, owned here.
        let leak = unsafe { OwnedFd::from_raw_fd(libc::dup(2)) };
        let script = format!("[ -e /dev/fd/{} ] && echo fd:leaked || echo fd:clean", leak.as_raw_fd());
        let config = SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), script.into()], ..Default::default() };
        let pty = Pty::spawn(&config, PtySize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 }, |_| {}).unwrap();
        let mut reader = pty.reader().unwrap();
        let mut output = Vec::new();
        let mut buf = [0u8; 256];
        while !String::from_utf8_lossy(&output).contains("fd:") || !output.ends_with(b"\n") {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("fd:clean"), "{}", String::from_utf8_lossy(&output));
    }

    /// The PTY host keeps only its status pipe: a descriptor it inherited
    /// (one another thread had not marked close-on-exec yet) would outlive
    /// its owner's close for as long as the program runs.
    #[test]
    fn host_keeps_no_inherited_descriptor() {
        // SAFETY: dup returns a new descriptor without CLOEXEC, owned here.
        let leak = unsafe { OwnedFd::from_raw_fd(libc::dup(2)) };
        let config = SpawnConfig { command: vec!["/bin/sleep".into(), "5".into()], ..Default::default() };
        let pty = Pty::spawn(&config, PtySize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 }, |_| {}).unwrap();
        let out = Command::new("/bin/ps").args(["-o", "ppid=", "-p", &pty.pid().to_string()]).output().unwrap();
        let host = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        assert!(host.parse::<i32>().is_ok_and(|h| h > 1), "host pid {host:?}");
        #[cfg(target_os = "linux")]
        let fds: Vec<i32> =
            std::fs::read_dir(format!("/proc/{host}/fd")).unwrap().filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect();
        #[cfg(target_os = "macos")]
        let fds: Vec<i32> = {
            let out = Command::new("/usr/sbin/lsof").args(["-a", "-p", &host, "-d", "0-1024", "-F", "f"]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.strip_prefix('f')?.parse().ok()).collect()
        };
        assert!(!fds.contains(&leak.as_raw_fd()), "host holds {} in {fds:?}", leak.as_raw_fd());
        assert_eq!(fds.iter().filter(|&&fd| fd > 2).count(), 1, "only the status pipe: {fds:?}");
        pty.signal(libc::SIGKILL);
    }

    #[test]
    fn line_mode_tracks_the_slave() {
        let config =
            SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), "read a; stty -echo -icanon; echo raw; read b".into()], ..Default::default() };
        let pty = Pty::spawn(&config, PtySize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 }, |_| {}).unwrap();
        let (mut reader, mut writer) = (pty.reader().unwrap(), pty.writer().unwrap());
        assert_eq!(pty.line_mode().unwrap(), (true, true));
        writer.write_all(b"x\n").unwrap();
        let mut output = Vec::new();
        let mut buf = [0u8; 256];
        while !String::from_utf8_lossy(&output).contains("raw") {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(pty.line_mode().unwrap(), (false, false));
    }

    #[test]
    fn foreground_pid_is_none_after_child_killed() {
        let (tx, rx) = std::sync::mpsc::channel();
        let config = SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()], ..Default::default() };
        let size = PtySize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let pty = Pty::spawn(&config, size, move |status| {
            let _ = tx.send(status);
        })
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pty.foreground_pid().is_none() {
            assert!(std::time::Instant::now() < deadline, "no foreground group while running");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        pty.signal(libc::SIGKILL);
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(pty.foreground_pid(), None);
    }
}
