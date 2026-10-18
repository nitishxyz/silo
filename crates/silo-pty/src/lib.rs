//! PTY allocation and child supervision (macOS and Linux).

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use rustix::{
    fs::{Mode, OFlags, open},
    pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt},
    termios::{Winsize, tcgetpgrp, tcsetwinsize},
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
                && !shell.is_empty() {
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
}

/// A spawned PTY child. Dropping it hangs up the child.
pub struct Pty {
    master: File,
    child: Arc<ChildState>,
}

impl Pty {
    /// Spawn `config` on a new PTY. `on_exit` runs on a supervisor thread
    /// once the child was reaped.
    ///
    /// # Errors
    /// PTY allocation or spawn failures.
    pub fn spawn(
        config: &SpawnConfig,
        size: PtySize,
        on_exit: impl FnOnce(ExitStatus) + Send + 'static,
    ) -> io::Result<Self> {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
        // macOS posix_openpt has no O_CLOEXEC; set it before spawning anything.
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
        grantpt(&master)?;
        unlockpt(&master)?;
        let name = ptsname(&master, Vec::new())?;
        let slave = File::from(open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
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
            let base = std::path::Path::new(&program)
                .file_name()
                .unwrap_or(OsStr::new("sh"));
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
        let state = Arc::new(ChildState {
            pid: i32::try_from(child.id()).map_err(io::Error::other)?,
            exited: AtomicBool::new(false),
            status: Mutex::new(None),
        });
        let worker = state.clone();
        std::thread::Builder::new()
            .name("silo-pty-child".into())
            .spawn(move || {
                let status = child.wait();
                worker.exited.store(true, Ordering::Release);
                if let Ok(status) = status {
                    if let Ok(mut slot) = worker.status.lock() {
                        *slot = Some(status);
                    }
                    on_exit(status);
                }
            })?;
        Ok(Self {
            master: File::from(master),
            child: state,
        })
    }

    /// A handle for blocking reads of child output.
    ///
    /// # Errors
    /// Fails if the descriptor cannot be duplicated.
    pub fn reader(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    /// A handle for blocking writes of input to the child.
    ///
    /// # Errors
    /// Fails if the descriptor cannot be duplicated.
    pub fn writer(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    /// # Errors
    /// Fails if the kernel rejects the window size.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        set_size(&self.master, size)
    }

    #[must_use]
    pub fn pid(&self) -> i32 {
        self.child.pid
    }

    #[must_use]
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.child.status.lock().ok().and_then(|s| *s)
    }

    /// Process group currently in the PTY foreground.
    #[must_use]
    pub fn foreground_pid(&self) -> Option<i32> {
        tcgetpgrp(&self.master).ok().map(rustix::process::Pid::as_raw_nonzero).map(|p| p.get())
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
        self.signal(libc::SIGHUP);
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
        let config = SpawnConfig {
            command: vec!["/bin/sh".into(), "-c".into(), "stty size; read x; echo got:$x; exit 3".into()],
            ..Default::default()
        };
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
}
