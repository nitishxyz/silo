//! The server as its own responsible process (macOS).
//!
//! macOS privacy controls (TCC: Screen Recording, Automation, Files,
//! Camera…) charge a request to the requesting process's *responsible
//! process*, inherited from parent to child unless a launcher disclaims it.
//! Every shell and agent in Silo descends from the server, so the server
//! must be responsible for itself: then prompts and System Settings name
//! Silo Server (the app's helper bundle, `Silo.app/Contents/Helpers/Silo
//! Server.app`) whoever started it, not the app or a terminal that ran
//! `silo server`. launchd already starts its jobs that way; any other start
//! (the app's plain server, a takeover, `silo server` in a terminal, the
//! otto daemon) re-executes itself in place, keeping its pid, with the
//! responsibility disclaimed ([`at_start`]). Only the long-running daemons
//! do: `pty-host` and everything else keep inheriting the server, so shells
//! and their programs are charged to it.
//!
//! `silo server --daemon` forks twice to detach, and the process macOS keeps
//! as responsible must be the one that lives on: disclaiming before the forks
//! left the pre-fork pid responsible, and once it exited the daemon and every
//! child (pty-host, shells) answered for themselves, so a shell's Screen
//! Recording was its own, never Silo Server's. A daemonizing start therefore
//! disclaims after the forks instead ([`Plan::AfterDaemonize`]): the final
//! process re-executes itself in place ([`after_daemonize`]), told by
//! [`DAEMONIZED`] that it is already detached (and which descriptor is the
//! takeover's status pipe).

use std::ffi::OsStr;

/// Set in the re-executed process so a failure can never loop; removed
/// again at start, so the server's children don't inherit it.
const GUARD: &str = "SILO_DISCLAIMED";

/// Set (with [`GUARD`]) in a daemon re-executed after its forks: it is
/// already detached; the value is the status pipe's descriptor, or `-`.
/// Removed at start like the guard.
const DAEMONIZED: &str = "SILO_DAEMONIZED";

/// When a `silo` run disclaims its responsibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Never: a client command, `pty-host`, a probe…
    Never,
    /// At start, before anything else ([`at_start`]).
    AtStart,
    /// In the final process of `server --daemon`, after the forks that
    /// detach it ([`after_daemonize`]).
    AfterDaemonize,
}

/// [`Plan`] for `silo` run with `args` (without the program name):
/// `server --daemon` (which forks) after daemonizing, other daemons
/// ([`disclaims`]) at start.
pub fn plan<S: AsRef<OsStr>>(args: &[S]) -> Plan {
    if !disclaims(args) {
        return Plan::Never;
    }
    let (first, rest) = command(args);
    let daemon = first == Some("server") && rest.iter().any(|w| w.as_ref() == "--daemon");
    if daemon { Plan::AfterDaemonize } else { Plan::AtStart }
}

/// The subcommand (after a global `--host <value>`) and the words after it.
fn command<S: AsRef<OsStr>>(args: &[S]) -> (Option<&str>, &[S]) {
    let mut i = 0;
    while let Some(word) = args.get(i).map(AsRef::as_ref) {
        if word == "--host" {
            i += 2;
        } else if word.to_str().is_some_and(|w| w.starts_with("--host=")) {
            i += 1;
        } else {
            return (word.to_str(), &args[i + 1..]);
        }
    }
    (None, &[])
}

/// Whether `silo` run with `args` (without the program name) is a
/// long-running daemon that disclaims its responsibility: `server` in all
/// its modes, and `otto server` (but not `otto server stop|status`).
pub fn disclaims<S: AsRef<OsStr>>(args: &[S]) -> bool {
    match command(args) {
        (Some("server"), _) => true,
        (Some("otto"), rest) => {
            let mut rest = rest.iter().map(AsRef::as_ref).filter(|w| !w.to_str().is_some_and(|w| w.starts_with('-')));
            rest.next().is_some_and(|w| w == "server") && !rest.next().is_some_and(|w| w == "stop" || w == "status")
        }
        _ => false,
    }
}

/// A daemon re-executed after its forks ([`after_daemonize`]): already
/// detached, with the takeover's status pipe if it had one.
#[derive(Debug)]
pub struct Daemonized {
    pub status: Option<std::fs::File>,
}

/// At the very top of `main`, before any thread: for a daemon disclaiming
/// at start ([`Plan::AtStart`]) not responsible for itself, re-execute this
/// program in place (same pid, arguments and environment) as its own
/// responsible process. Returns only if nothing was to be done or the
/// re-exec failed (logged; the process then runs on as it is), with
/// [`Daemonized`] when this is a daemon re-executed after its forks.
#[cfg(target_os = "macos")]
pub fn at_start() -> Option<Daemonized> {
    let guarded = std::env::var_os(GUARD).is_some();
    let daemonized = std::env::var_os(DAEMONIZED);
    // SAFETY: single-threaded (nothing has started a thread yet).
    unsafe {
        std::env::remove_var(GUARD);
        std::env::remove_var(DAEMONIZED);
    }
    if let Some(value) = daemonized {
        return Some(Daemonized { status: status_pipe(&value) });
    }
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if guarded || plan(&args) != Plan::AtStart || self_responsible() {
        return None;
    }
    if let Err(error) = reexec_disclaimed(&[]) {
        eprintln!("silo: could not become its own responsible process: {error}");
    }
    None
}

#[cfg(not(target_os = "macos"))]
pub fn at_start() -> Option<Daemonized> {
    None
}

/// [`DAEMONIZED`]'s descriptor as a file, close-on-exec again; `None` for
/// `-` or a descriptor that isn't open.
#[cfg(target_os = "macos")]
fn status_pipe(value: &OsStr) -> Option<std::fs::File> {
    use std::os::fd::FromRawFd;
    let fd: libc::c_int = value.to_str()?.parse().ok().filter(|fd| *fd > 2)?;
    // SAFETY: the descriptor was handed over by the process this one
    // re-executed (`after_daemonize`) and nothing else owns it; F_SETFD on
    // a closed one fails and it is left alone.
    unsafe { (libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0).then(|| std::fs::File::from_raw_fd(fd)) }
}

/// In the final process of `server --daemon`, right after the forks (still
/// single-threaded): re-execute in place, disclaimed, as a detached daemon
/// ([`DAEMONIZED`], keeping `status` open across the exec). Returns
/// `status` only if the re-exec failed (logged; it then runs on charged to
/// whoever started it).
#[cfg(target_os = "macos")]
pub fn after_daemonize(status: Option<std::fs::File>) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    let fd = status.as_ref().map(AsRawFd::as_raw_fd);
    // SAFETY: flags of a descriptor this process owns.
    if let Some(fd) = fd
        && unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0
    {
        eprintln!("silo: could not become its own responsible process: {}", std::io::Error::last_os_error());
        return status;
    }
    let value = fd.map_or_else(|| "-".to_owned(), |fd| fd.to_string());
    let error = reexec_disclaimed(&[(DAEMONIZED, &value)]).err();
    if let Some(fd) = fd {
        // SAFETY: as above.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    if let Some(error) = error {
        eprintln!("silo: could not become its own responsible process: {error}");
    }
    status
}

#[cfg(not(target_os = "macos"))]
pub fn after_daemonize(status: Option<std::fs::File>) -> Option<std::fs::File> {
    status
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    // Private libSystem (libquarantine) functions, since macOS 10.14: how
    // launchd and terminal apps start processes responsible for themselves.
    fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
    fn responsibility_spawnattrs_setdisclaim(attrs: *mut libc::posix_spawnattr_t, disclaim: libc::c_int) -> libc::c_int;
}

#[cfg(target_os = "macos")]
fn self_responsible() -> bool {
    // SAFETY: plain queries on this process.
    let pid = unsafe { libc::getpid() };
    let responsible = unsafe { responsibility_get_pid_responsible_for_pid(pid) };
    // An error (-1) is no reason to re-exec.
    responsible <= 0 || responsible == pid
}

/// `posix_spawn` with `POSIX_SPAWN_SETEXEC` (an exec keeping the pid) and
/// the responsibility disclaimed, [`GUARD`] and `extra` added to the
/// environment; returns only on failure.
#[cfg(target_os = "macos")]
fn reexec_disclaimed(extra: &[(&str, &str)]) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let exe = std::env::current_exe()?;
    let cstr = |bytes: Vec<u8>| CString::new(bytes).map_err(|_| std::io::Error::other("an argument holds a NUL byte"));
    let path = cstr(exe.as_os_str().as_bytes().to_vec())?;
    let argv = std::env::args_os().map(|a| cstr(a.as_bytes().to_vec())).collect::<std::io::Result<Vec<_>>>()?;
    let env = std::env::vars_os()
        .filter(|(k, _)| k != GUARD && !extra.iter().any(|(e, _)| k == e))
        .map(|(k, v)| cstr([k.as_bytes(), b"=", v.as_bytes()].concat()))
        .chain([cstr(format!("{GUARD}=1").into_bytes())])
        .chain(extra.iter().map(|(k, v)| cstr(format!("{k}={v}").into_bytes())))
        .collect::<std::io::Result<Vec<_>>>()?;
    let argv_ptrs: Vec<*mut libc::c_char> = argv.iter().map(|a| a.as_ptr().cast_mut()).chain([std::ptr::null_mut()]).collect();
    let env_ptrs: Vec<*mut libc::c_char> = env.iter().map(|e| e.as_ptr().cast_mut()).chain([std::ptr::null_mut()]).collect();
    // SAFETY: the attributes are initialized before use and destroyed on
    // every path; the strings outlive the call, and the pointer arrays are
    // NUL-terminated.
    unsafe {
        let mut attrs: libc::posix_spawnattr_t = std::ptr::null_mut();
        let check = |code: libc::c_int| if code == 0 { Ok(()) } else { Err(std::io::Error::from_raw_os_error(code)) };
        check(libc::posix_spawnattr_init(&raw mut attrs))?;
        let spawned = check(libc::posix_spawnattr_setflags(&raw mut attrs, libc::POSIX_SPAWN_SETEXEC as libc::c_short))
            .and_then(|()| check(responsibility_spawnattrs_setdisclaim(&raw mut attrs, 1)))
            .and_then(|()| {
                let mut pid = 0;
                check(libc::posix_spawn(&raw mut pid, path.as_ptr(), std::ptr::null(), &raw const attrs, argv_ptrs.as_ptr(), env_ptrs.as_ptr()))
            });
        libc::posix_spawnattr_destroy(&raw mut attrs);
        spawned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<&str> {
        line.split_whitespace().collect()
    }

    #[test]
    fn only_the_daemons_disclaim() {
        for line in [
            "server",
            "server --daemon --idle-exit 0",
            "server --takeover --daemon --idle-exit 0",
            "server --adopt",
            "server --idle-exit 0 --adopt",
            "--host mac server",
            "--host=mac server --adopt",
            "otto server",
            "otto server --daemon",
            "otto server --foreground",
            "otto server --idle-exit 300",
        ] {
            assert!(disclaims(&words(line)), "{line}");
        }
        for line in [
            "",
            "pty-host",
            "pty-host --session s1",
            "up",
            "down",
            "attach",
            "ls",
            "kill-server",
            "otto",
            "otto run hi",
            "otto server stop",
            "otto server status",
            "otto sessions",
            "mcp",
            "--host server ls",
            "report working --app server",
            "--help",
        ] {
            assert!(!disclaims(&words(line)), "{line}");
        }
    }

    #[test]
    fn a_forking_daemon_disclaims_after_its_forks() {
        for line in [
            "server --daemon --idle-exit 0",
            "server --takeover --daemon --idle-exit 0",
            "--host mac server --daemon",
            "server --idle-exit 0 --daemon",
        ] {
            assert_eq!(plan(&words(line)), Plan::AfterDaemonize, "{line}");
        }
        for line in ["server", "server --adopt", "server --idle-exit 0 --adopt", "otto server --daemon", "otto server", "--host=mac server"] {
            assert_eq!(plan(&words(line)), Plan::AtStart, "{line}");
        }
        for line in ["", "pty-host", "up", "otto server stop", "ls --daemon", "--host server ls"] {
            assert_eq!(plan(&words(line)), Plan::Never, "{line}");
        }
    }

    /// The real thing on this Mac: the test binary re-executes itself
    /// disclaimed (as a daemon would) and reports, from the same pid, that
    /// it is now its own responsible process.
    #[cfg(target_os = "macos")]
    #[test]
    fn reexec_keeps_the_pid_and_makes_it_responsible_for_itself() {
        const CHILD: &str = "SILO_DISCLAIM_TEST_CHILD";
        const NAME: &str = "disclaim::tests::reexec_keeps_the_pid_and_makes_it_responsible_for_itself";
        if std::env::var_os(CHILD).is_some() {
            if std::env::var_os(GUARD).is_none() {
                reexec_disclaimed(&[]).unwrap();
                unreachable!("posix_spawn SETEXEC returned");
            }
            // SAFETY: plain queries on this process.
            let (pid, responsible) = unsafe { (libc::getpid(), responsibility_get_pid_responsible_for_pid(libc::getpid())) };
            println!("disclaim-probe {pid} {responsible}");
            return;
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env_remove(GUARD)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}");
        let line = stdout.lines().find_map(|l| Some(l.split_once("disclaim-probe ")?.1)).unwrap_or_else(|| panic!("no probe line: {stdout}"));
        let (got_pid, responsible) = line.split_once(' ').unwrap();
        assert_eq!(got_pid, pid.to_string(), "re-executed in place");
        assert_eq!(responsible, pid.to_string(), "responsible for itself");
    }

    /// `server --daemon` on this Mac: a test child daemonizes like the
    /// server (double fork, `setsid`, the parent gone and reaped before the
    /// daemon checks), the final process re-executes itself disclaimed, and
    /// then it is responsible for itself and a child it starts names it
    /// (disclaiming before the forks left a dead pid responsible, so each
    /// child answered for itself).
    #[cfg(target_os = "macos")]
    #[test]
    fn a_daemon_is_responsible_for_itself_and_its_children() {
        const CHILD: &str = "SILO_DAEMON_TEST_CHILD";
        const REPORT: &str = "SILO_DAEMON_TEST_REPORT";
        const NAME: &str = "disclaim::tests::a_daemon_is_responsible_for_itself_and_its_children";
        let args = ["--exact", NAME, "--nocapture", "--test-threads=1"];
        // SAFETY: plain queries on this process.
        let me = || unsafe { (libc::getpid(), responsibility_get_pid_responsible_for_pid(libc::getpid())) };
        if std::env::var_os(REPORT).is_some() {
            let (pid, responsible) = me();
            println!("daemon-child {pid} {responsible}");
            return;
        }
        if let Some(log) = std::env::var_os(CHILD) {
            if std::env::var_os(DAEMONIZED).is_none() {
                let log = std::fs::OpenOptions::new().append(true).open(log).unwrap();
                crate::daemonize_to(false, Some(log));
                // Only if the re-exec failed.
                println!("daemon-failed");
                return;
            }
            // The re-executed daemon: what `main` does first.
            let daemonized = at_start();
            assert!(daemonized.is_some_and(|d| d.status.is_none()));
            assert!(std::env::var_os(GUARD).is_none() && std::env::var_os(DAEMONIZED).is_none(), "children never inherit them");
            let (pid, responsible) = me();
            let child = std::process::Command::new(std::env::current_exe().unwrap()).args(args).env(REPORT, "1").output().unwrap();
            let out = String::from_utf8_lossy(&child.stdout);
            let child = out.lines().find_map(|l| Some(l.split_once("daemon-child ")?.1)).unwrap_or("none -1").split_once(' ').unwrap().1.to_owned();
            println!("daemon-probe {pid} {responsible} {child}");
            return;
        }
        let log = std::env::temp_dir().join(format!("silo-daemon-test-{}.log", std::process::id()));
        std::fs::write(&log, "").unwrap();
        let mut started = std::process::Command::new(std::env::current_exe().unwrap())
            .args(args)
            .env(CHILD, &log)
            .env_remove(GUARD)
            .env_remove(DAEMONIZED)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let starter = started.id();
        assert!(started.wait().unwrap().success());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let text = loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("daemon-probe ") || text.contains("daemon-failed") || std::time::Instant::now() > deadline {
                break text;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let _ = std::fs::remove_file(&log);
        let line = text.lines().find_map(|l| Some(l.split_once("daemon-probe ")?.1)).unwrap_or_else(|| panic!("no probe line: {text}"));
        let fields: Vec<&str> = line.split(' ').collect();
        let [pid, responsible, child] = fields[..] else { panic!("{line}") };
        assert_ne!(pid, starter.to_string(), "detached");
        assert_eq!(responsible, pid, "the daemon is responsible for itself");
        assert_eq!(child, pid, "its child names it");
    }
}
