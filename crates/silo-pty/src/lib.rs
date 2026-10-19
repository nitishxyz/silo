//! PTY allocation and child supervision (macOS and Linux).

pub mod handoff;
mod threads;
mod watch;

pub use threads::{PtyEvent, PtyInput};

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    os::{fd::OwnedFd, unix::process::CommandExt},
    path::PathBuf,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use threads::IoThreads;

use rustix::{
    fs::{Mode, OFlags, open},
    pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt},
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
}

/// A PTY and its process. Dropping it hangs up the process group; use
/// [`Pty::detach`] to hand the process to another owner instead.
pub struct Pty {
    master: Arc<OwnedFd>,
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
    /// Spawn `config` on a new PTY. `on_exit` runs on a supervisor thread
    /// once the child was reaped.
    ///
    /// # Errors
    /// PTY allocation or spawn failures.
    pub fn spawn(config: &SpawnConfig, size: PtySize, on_exit: impl FnOnce(ExitStatus) + Send + 'static) -> io::Result<Self> {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
        // macOS posix_openpt has no O_CLOEXEC; set it before spawning anything.
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
        grantpt(&master)?;
        unlockpt(&master)?;
        let name = ptsname(&master, Vec::new())?;
        let slave = File::from(open(name.as_c_str(), OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC, Mode::empty())?);
        set_size(&master, size)?;

        let (program, args, login) = if config.command.is_empty() {
            (user_shell().into_os_string(), Vec::new(), true)
        } else {
            (config.command[0].clone(), config.command[1..].to_vec(), false)
        };
        let mut command = Command::new(&program);
        command.args(&args);
        if login {
            // A leading dash makes the shell a login shell, so a daemon
            // started from a GUI app still gets the user's PATH.
            let base = std::path::Path::new(&program).file_name().unwrap_or(OsStr::new("sh"));
            let mut arg0 = OsString::from("-");
            arg0.push(base);
            command.arg0(arg0);
        }
        command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "silo")
            .env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"))
            .env_remove("COLUMNS")
            .env_remove("LINES")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        if std::env::var_os("LANG").is_none() {
            command.env("LANG", "en_US.UTF-8");
        }
        for (key, value) in &config.env {
            command.env(key, value);
        }
        command.current_dir(config.cwd.clone().unwrap_or_else(home_dir));
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Restore default signal dispositions the daemon may ignore.
                for signal in [libc::SIGPIPE, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP, libc::SIGCHLD] {
                    libc::signal(signal, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let state = ChildState::new(i32::try_from(child.id()).map_err(io::Error::other)?);
        let worker = state.clone();
        // The child is reaped even after `detach` (to avoid zombies), but its
        // exit is then no longer reported here.
        std::thread::Builder::new().name("silo-pty-child".into()).spawn(move || {
            let status = child.wait();
            worker.exited.store(true, Ordering::Release);
            if let Ok(status) = status {
                if let Ok(mut slot) = worker.status.lock() {
                    *slot = Some(status);
                }
                if worker.notify.swap(false, Ordering::AcqRel) {
                    on_exit(status);
                }
            }
        })?;
        Ok(Self::new(master, state, size, None))
    }

    fn new(master: OwnedFd, child: Arc<ChildState>, size: PtySize, watcher: Option<JoinHandle<()>>) -> Self {
        Self { master: Arc::new(master), child, size: Mutex::new(size), io: None, watcher, detached: false }
    }

    /// Wrap an inherited PTY master whose session leader `pid` is not our
    /// child (e.g. received over [`handoff`]). Exit is observed through a
    /// pidfd on Linux and kqueue `EVFILT_PROC` on macOS, so it is reported
    /// even while the process is an unreaped zombie; the status is unknown,
    /// so `on_exit` gets `None`. Master EOF/EIO is reported separately as
    /// [`PtyEvent::Closed`].
    ///
    /// # Errors
    /// Fails if `pid` is invalid, the descriptor is not a usable terminal or
    /// a thread cannot be spawned. On failure the master is handed back
    /// unharmed in [`AdoptError`], so the program is not hung up.
    pub fn adopt(master: OwnedFd, pid: i32, size: PtySize, on_exit: impl FnOnce(Option<i32>) + Send + 'static) -> Result<Self, AdoptError> {
        let fail = |error, master| Err(AdoptError { error, detached: Detached { master, pid, size } });
        if pid <= 0 {
            return fail(io::Error::new(io::ErrorKind::InvalidInput, "invalid pid"), master);
        }
        if let Err(e) = rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC) {
            return fail(e.into(), master);
        }
        if let Err(e) = set_size(&master, size) {
            return fail(e, master);
        }
        let watch = match watch::Watch::new(pid) {
            Ok(watch) => watch,
            Err(e) => return fail(e, master),
        };
        let state = ChildState::new(pid);
        let worker = state.clone();
        let watcher = std::thread::Builder::new().name("silo-pty-watch".into()).spawn(move || {
            if watch.wait(&worker.stop) {
                worker.exited.store(true, Ordering::Release);
                if worker.notify.swap(false, Ordering::AcqRel) {
                    on_exit(None);
                }
            }
        });
        match watcher {
            Ok(watcher) => Ok(Self::new(master, state, size, Some(watcher))),
            Err(e) => fail(e, master),
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
        on_exit: impl FnOnce(Option<i32>) + Send + 'static,
        on_event: impl FnMut(PtyEvent) + Send + 'static,
    ) -> Result<(Self, PtyInput), AdoptError> {
        let mut pty = Self::adopt(detached.master, detached.pid, detached.size, on_exit)?;
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
    /// master, so it is moved out, not duplicated.
    #[must_use]
    pub fn detach(mut self) -> Detached {
        self.detached = true;
        self.child.notify.store(false, Ordering::Release);
        self.stop_threads();
        let (master, pid, size) = (self.master.clone(), self.child.pid, self.size());
        drop(self);
        // Nothing else clones the Arc once the I/O threads are joined.
        let master = Arc::try_unwrap(master).expect("pty master still shared after detach");
        Detached { master, pid, size }
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
            // SAFETY: the child is unreaped (the supervisor only reaps after
            // setting `exited`... races leave at worst a stale signal to a
            // process group that no longer exists).
            unsafe {
                libc::kill(-self.child.pid, signal);
            }
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.stop_threads();
        if !self.detached {
            self.signal(libc::SIGHUP);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

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
        assert_eq!(status.code(), Some(3));
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
