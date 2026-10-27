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

use std::ffi::OsStr;

/// Set in the re-executed process so a failure can never loop; removed
/// again at start, so the server's children don't inherit it.
const GUARD: &str = "SILO_DISCLAIMED";

/// Whether `silo` run with `args` (without the program name) is a
/// long-running daemon that disclaims its responsibility: `server` in all
/// its modes, and `otto server` (but not `otto server stop|status`).
pub fn disclaims<S: AsRef<OsStr>>(args: &[S]) -> bool {
    let mut words = args.iter().map(AsRef::as_ref);
    // The global `--host <value>` (or `--host=value`) may come first.
    let first = loop {
        match words.next() {
            Some(word) if word == "--host" => {
                words.next();
            }
            Some(word) if word.to_str().is_some_and(|w| w.starts_with("--host=")) => {}
            other => break other,
        }
    };
    match first.and_then(OsStr::to_str) {
        Some("server") => true,
        Some("otto") => {
            let mut rest = words.filter(|w| !w.to_str().is_some_and(|w| w.starts_with('-')));
            rest.next().is_some_and(|w| w == "server") && !rest.next().is_some_and(|w| w == "stop" || w == "status")
        }
        _ => false,
    }
}

/// At the very top of `main`, before any thread: for a daemon
/// ([`disclaims`]) not responsible for itself, re-execute this program in
/// place (same pid, arguments and environment) as its own responsible
/// process. Returns only if nothing was to be done or the re-exec failed
/// (logged; the process then runs on as it is).
#[cfg(target_os = "macos")]
pub fn at_start() {
    let guarded = std::env::var_os(GUARD).is_some();
    if guarded {
        // SAFETY: single-threaded (nothing has started a thread yet).
        unsafe { std::env::remove_var(GUARD) };
    }
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if guarded || !disclaims(&args) || self_responsible() {
        return;
    }
    if let Err(error) = reexec_disclaimed() {
        eprintln!("silo: could not become its own responsible process: {error}");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn at_start() {}

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
/// the responsibility disclaimed; returns only on failure.
#[cfg(target_os = "macos")]
fn reexec_disclaimed() -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let exe = std::env::current_exe()?;
    let cstr = |bytes: Vec<u8>| CString::new(bytes).map_err(|_| std::io::Error::other("an argument holds a NUL byte"));
    let path = cstr(exe.as_os_str().as_bytes().to_vec())?;
    let argv = std::env::args_os().map(|a| cstr(a.as_bytes().to_vec())).collect::<std::io::Result<Vec<_>>>()?;
    let env = std::env::vars_os()
        .filter(|(k, _)| k != GUARD)
        .map(|(k, v)| cstr([k.as_bytes(), b"=", v.as_bytes()].concat()))
        .chain([cstr(format!("{GUARD}=1").into_bytes())])
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
                reexec_disclaimed().unwrap();
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
}
