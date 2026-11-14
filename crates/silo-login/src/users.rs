//! Unix accounts: who exists, and whether they may log in now.

use std::{
    ffi::{CStr, CString},
    path::{Path, PathBuf},
};

use silo_proto::login::LoginError;

use crate::code;

/// The first uid of a regular account (`login.defs` `UID_MIN`).
pub const MIN_REGULAR_UID: u32 = 1000;
/// Past the last regular uid (`UID_MAX`); `nobody` (65534) is above it.
pub const MAX_REGULAR_UID: u32 = 60000;

/// An account, from the password database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
}

/// The accounts the login server consults; a fake in tests.
pub trait Users: Send + Sync {
    fn user(&self, name: &str) -> Option<User>;
    /// Whether the account's password entry is locked (`passwd -l`) or
    /// the account expired.
    fn locked(&self, name: &str) -> bool;
    /// Whether `/etc/nologin` exists (non-root logins are off).
    fn nologin(&self) -> bool;
}

/// Whether `name` is a regular account (what `*` devices may use).
pub fn regular(users: &dyn Users, name: &str) -> bool {
    users.user(name).is_some_and(|u| (MIN_REGULAR_UID..MAX_REGULAR_UID).contains(&u.uid))
}

fn refused(message: impl Into<String>) -> LoginError {
    LoginError { code: code::REFUSED.to_owned(), message: message.into() }
}

/// The account `name` if it may log in now.
///
/// # Errors
/// `refused`: it does not exist, its shell is `nologin` or `false`, it is
/// locked, or `/etc/nologin` exists and it is not root.
pub fn check(users: &dyn Users, name: &str) -> Result<User, LoginError> {
    let user = users.user(name).ok_or_else(|| refused(format!("there is no account {name}")))?;
    let shell = user.shell.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if matches!(shell.as_str(), "nologin" | "false") {
        return Err(refused(format!("the account {name} has no login shell")));
    }
    if users.locked(name) {
        return Err(refused(format!("the account {name} is locked")));
    }
    if user.uid != 0 && users.nologin() {
        return Err(refused("logins are disabled on this machine (/etc/nologin)"));
    }
    Ok(user)
}

/// The system's accounts: `getpwnam_r`, `/etc/shadow`, `/etc/nologin`.
pub struct System;

impl Users for System {
    fn user(&self, name: &str) -> Option<User> {
        lookup(name)
    }

    fn locked(&self, name: &str) -> bool {
        // Readable only by root; an unprivileged login server cannot tell
        // and serves only its own account anyway.
        let Ok(shadow) = std::fs::read_to_string("/etc/shadow") else { return false };
        let today = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86_400);
        shadow_locked(&shadow, name, today)
    }

    fn nologin(&self) -> bool {
        Path::new("/etc/nologin").exists()
    }
}

/// Whether `name`'s `/etc/shadow` line says locked (`!` before the hash)
/// or expired (field 8, days since the epoch, at or before `today`).
fn shadow_locked(shadow: &str, name: &str, today: u64) -> bool {
    let Some(line) = shadow.lines().find(|l| l.split(':').next() == Some(name)) else { return false };
    let fields: Vec<&str> = line.split(':').collect();
    let locked = fields.get(1).is_some_and(|hash| hash.starts_with('!'));
    let expired = fields.get(7).and_then(|f| f.parse::<u64>().ok()).is_some_and(|day| day <= today);
    locked || expired
}

fn passwd_buf() -> Vec<u8> {
    vec![0u8; 4096]
}

/// `name`'s password entry.
#[must_use]
pub fn lookup(name: &str) -> Option<User> {
    let cname = CString::new(name).ok()?;
    let mut buf = passwd_buf();
    loop {
        #[allow(unsafe_code)]
        // SAFETY: `passwd` is plain data, filled by getpwnam_r below.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        #[allow(unsafe_code)]
        // SAFETY: every pointer is valid for the call; the strings it
        // stores point into `buf`, read below before `buf` is dropped.
        let rc = unsafe { libc::getpwnam_r(cname.as_ptr(), &raw mut pwd, buf.as_mut_ptr().cast(), buf.len(), &raw mut found) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || found.is_null() {
            return None;
        }
        let text = |p: *const libc::c_char| {
            if p.is_null() {
                return Vec::new();
            }
            #[allow(unsafe_code)]
            // SAFETY: a non-null field of a successful getpwnam_r is a
            // NUL-terminated string in `buf`.
            let s = unsafe { CStr::from_ptr(p) };
            s.to_bytes().to_vec()
        };
        let path = |bytes: Vec<u8>| PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(bytes));
        return Some(User {
            name: String::from_utf8(text(pwd.pw_name)).ok()?,
            uid: pwd.pw_uid,
            gid: pwd.pw_gid,
            home: path(text(pwd.pw_dir)),
            shell: path(text(pwd.pw_shell)),
        });
    }
}

/// This process's account.
#[must_use]
pub fn current() -> Option<User> {
    let uid = rustix::process::getuid().as_raw();
    let mut buf = passwd_buf();
    loop {
        #[allow(unsafe_code)]
        // SAFETY: `passwd` is plain data, filled by getpwuid_r below.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        #[allow(unsafe_code)]
        // SAFETY: as in `lookup`.
        let rc = unsafe { libc::getpwuid_r(uid, &raw mut pwd, buf.as_mut_ptr().cast(), buf.len(), &raw mut found) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || found.is_null() || pwd.pw_name.is_null() {
            return None;
        }
        #[allow(unsafe_code)]
        // SAFETY: a successful getpwuid_r's `pw_name` is a NUL-terminated
        // string in `buf`.
        let name = unsafe { CStr::from_ptr(pwd.pw_name) };
        return lookup(name.to_str().ok()?);
    }
}

/// `user`'s supplementary groups, as `initgroups` would set them
/// (`getgrouplist`). Computed before forking: the child of a threaded
/// process may only make async-signal-safe calls, and reading the group
/// database is not one.
#[must_use]
pub fn groups(user: &User) -> Vec<libc::gid_t> {
    let Ok(name) = CString::new(user.name.as_bytes()) else { return vec![user.gid] };
    let mut count: libc::c_int = 64;
    loop {
        let mut list: Vec<libc::gid_t> = vec![0; usize::try_from(count).unwrap_or(64)];
        let mut n = count;
        #[cfg(target_os = "macos")]
        #[allow(unsafe_code, clippy::cast_possible_wrap)]
        // SAFETY: `list` holds `n` entries; macOS takes them as c_int.
        let rc = unsafe { libc::getgrouplist(name.as_ptr(), user.gid as libc::c_int, list.as_mut_ptr().cast(), &raw mut n) };
        #[cfg(not(target_os = "macos"))]
        #[allow(unsafe_code)]
        // SAFETY: `list` holds `n` entries.
        let rc = unsafe { libc::getgrouplist(name.as_ptr(), user.gid, list.as_mut_ptr(), &raw mut n) };
        if rc >= 0 {
            list.truncate(usize::try_from(n).unwrap_or(0));
            return list;
        }
        if n <= count || count >= 65536 {
            return vec![user.gid];
        }
        count = n;
    }
}

/// Whether `path` names an existing directory (for `XDG_RUNTIME_DIR`).
#[must_use]
pub fn is_dir(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<User>, bool, bool);

    impl Users for Fake {
        fn user(&self, name: &str) -> Option<User> {
            self.0.iter().find(|u| u.name == name).cloned()
        }
        fn locked(&self, name: &str) -> bool {
            self.1 && name == "locked"
        }
        fn nologin(&self) -> bool {
            self.2
        }
    }

    fn user(name: &str, uid: u32, shell: &str) -> User {
        User { name: name.into(), uid, gid: uid, home: format!("/home/{name}").into(), shell: shell.into() }
    }

    #[test]
    fn refusals_and_regular_accounts() {
        let users = Fake(
            vec![
                user("alice", 1000, "/bin/bash"),
                user("root", 0, "/bin/sh"),
                user("svc", 999, "/usr/sbin/nologin"),
                user("off", 1001, "/bin/false"),
                user("locked", 1002, "/bin/bash"),
                user("nobody", 65534, "/bin/sh"),
            ],
            true,
            false,
        );
        assert_eq!(check(&users, "alice").unwrap().uid, 1000);
        assert!(check(&users, "root").is_ok());
        for refused in ["missing", "svc", "off", "locked"] {
            assert_eq!(check(&users, refused).unwrap_err().code, code::REFUSED, "{refused}");
        }
        assert!(regular(&users, "alice") && !regular(&users, "root") && !regular(&users, "svc") && !regular(&users, "nobody"));
        let closed = Fake(users.0.clone(), false, true);
        assert_eq!(check(&closed, "alice").unwrap_err().code, code::REFUSED, "/etc/nologin");
        assert!(check(&closed, "root").is_ok(), "root still logs in");
    }

    #[test]
    fn shadow_lines_say_locked_or_expired() {
        let shadow = "root:$6$x:19000:0:99999:7:::\nalice:!$6$y:19000:0:99999:7:::\nbob:$6$z:19000:0:99999:7::100:\ncarol:*:19000::::::\n";
        assert!(!shadow_locked(shadow, "root", 20000));
        assert!(shadow_locked(shadow, "alice", 20000), "passwd -l");
        assert!(shadow_locked(shadow, "bob", 20000), "expired");
        assert!(!shadow_locked(shadow, "bob", 99), "not yet expired");
        assert!(!shadow_locked(shadow, "carol", 20000), "no password is not locked: keys still work");
        assert!(!shadow_locked(shadow, "dave", 20000));
    }

    #[test]
    fn this_process_s_account_is_found() {
        let me = current().expect("the test's own account");
        assert_eq!(lookup(&me.name), Some(me.clone()));
        assert!(groups(&me).contains(&me.gid));
        assert!(lookup("no such user here").is_none());
    }
}
