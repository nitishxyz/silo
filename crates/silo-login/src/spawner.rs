//! Starting an account's own `silo server` (`docs/LOGIN_SERVER.md` §5).

use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, bail};
use silo_proto::paths;

use crate::users::{self, User};

/// The server's switch for trusting `role: login` from its own uid
/// (`silo_server::TRUST_OWN_LOGIN_ENV`; repeated here so the login server
/// does not link the server).
pub const TRUST_OWN_LOGIN_ENV: &str = "SILO_LOGIN_TRUST_OWN_UID";
/// `PATH` of a started server.
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Where an account's server is and how to start it.
pub trait Spawner: Send + Sync {
    /// The unix socket of `user`'s server.
    fn socket(&self, user: &User) -> PathBuf;
    /// Start `user`'s server, detached; the caller waits for its socket.
    ///
    /// # Errors
    /// It could not be started.
    fn start(&self, user: &User) -> anyhow::Result<()>;
}

/// `user`'s Silo home, as `silo_proto::paths::home` computes it in a
/// process of theirs with nothing but `HOME` set: what the server this
/// spawner starts (with a clean environment) uses. A server the user
/// started with another `SILO_HOME` or `XDG_STATE_HOME` is not found.
#[must_use]
pub fn silo_home_of(user: &User) -> PathBuf {
    let home = user.home.clone().into_os_string();
    let var = move |name: &str| (name == "HOME").then(|| home.clone());
    paths::resolve_home_with(silo_proto::CHANNEL, &var, cfg!(target_os = "macos")).path
}

/// Reap `child` when it exits: it is detached (its own session) and
/// outlives the login server, which never waits on it otherwise.
fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// The login server's spawner, as root: the account's server runs as that
/// user, with that user's groups, in a session of its own.
pub struct AsUser {
    exe: PathBuf,
}

impl AsUser {
    /// Start servers with `exe` (this `silo` binary).
    #[must_use]
    pub fn new(exe: PathBuf) -> Self {
        Self { exe }
    }
}

impl Spawner for AsUser {
    fn socket(&self, user: &User) -> PathBuf {
        paths::socket(&silo_home_of(user))
    }

    fn start(&self, user: &User) -> anyhow::Result<()> {
        if !rustix::process::geteuid().is_root() {
            bail!("starting another account's server needs root");
        }
        let (uid, gid) = (user.uid, user.gid);
        let groups = users::groups(user);
        let cwd = if users::is_dir(&user.home) { user.home.clone() } else { PathBuf::from("/") };
        let mut command = Command::new(&self.exe);
        command
            .args(["server", "--adopt"])
            .env_clear()
            .env("HOME", &user.home)
            .env("USER", &user.name)
            .env("LOGNAME", &user.name)
            .env("SHELL", &user.shell)
            .env("PATH", PATH)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let runtime = PathBuf::from(format!("/run/user/{uid}"));
        if users::is_dir(&runtime) {
            command.env("XDG_RUNTIME_DIR", runtime);
        }
        #[allow(unsafe_code)]
        // SAFETY: the closure runs in the forked child before exec and
        // makes only async-signal-safe calls (setsid, setgroups, setgid,
        // setuid, getters) on data computed before the fork.
        unsafe {
            command.pre_exec(move || {
                let check = |rc: libc::c_int| if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) };
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                check(libc::setgroups(groups.len() as _, groups.as_ptr()))?;
                check(libc::setgid(gid))?;
                check(libc::setuid(uid))?;
                // Every id dropped, for good.
                if libc::getuid() != uid || libc::geteuid() != uid || libc::getgid() != gid || libc::getegid() != gid {
                    return Err(std::io::Error::other("privileges were not dropped"));
                }
                Ok(())
            });
        }
        let child = command.spawn().with_context(|| format!("start {}'s server", user.name))?;
        tracing::info!(account = user.name, pid = child.id(), "started an account's server");
        reap(child);
        Ok(())
    }
}

/// `--unprivileged`: only this process's own account, started without
/// changing user, in this process's Silo home. The server is told to
/// trust `role: login` from its own uid ([`TRUST_OWN_LOGIN_ENV`]).
pub struct Unprivileged {
    exe: PathBuf,
    home: PathBuf,
}

impl Unprivileged {
    #[must_use]
    pub fn new(exe: PathBuf, home: PathBuf) -> Self {
        Self { exe, home }
    }

    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }
}

impl Spawner for Unprivileged {
    fn socket(&self, _user: &User) -> PathBuf {
        paths::socket(&self.home)
    }

    fn start(&self, user: &User) -> anyhow::Result<()> {
        if user.uid != rustix::process::getuid().as_raw() {
            bail!("an unprivileged login server starts only its own account's server");
        }
        let mut command = Command::new(&self.exe);
        command
            .args(["server", "--adopt"])
            .env("SILO_HOME", &self.home)
            .env(TRUST_OWN_LOGIN_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        let child = command.spawn().context("start the server")?;
        tracing::info!(pid = child.id(), "started this account's server");
        reap(child);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_s_home_is_computed_from_its_home_directory_only() {
        let user = User { name: "alice".into(), uid: 1000, gid: 1000, home: "/home/alice".into(), shell: "/bin/bash".into() };
        let home = silo_home_of(&user);
        assert!(home.starts_with("/home/alice"), "{}", home.display());
        assert_eq!(AsUser::new("/usr/bin/silo".into()).socket(&user), paths::socket(&home));
    }
}
