//! The server and menu-bar agent copied out of Silo.app to one fixed place,
//! `<SILO_HOME>/runtime/{silo,silo-menubar}`, so the launchd job never holds
//! the bundle open and the app can be replaced while the server runs. Each
//! build replaces the files there atomically (a fresh inode: running
//! processes keep theirs, and macOS never sees a signed binary overwritten in
//! place). One stable path is one entry in System Settings' privacy lists,
//! and a permission granted to it stays granted across upgrades.

use std::{
    io,
    os::fd::AsFd,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Files of a runtime directory: (name in `Contents/MacOS`, name here).
const FILES: [(&str, &str); 2] = [("silo-server", "silo"), ("silo-menubar", "silo-menubar")];

/// Legacy per-build runtime directories touched this recently are never
/// collected (an older app may have just installed one).
const FRESH: Duration = Duration::from_secs(300);

#[must_use]
pub fn root(home: &Path) -> PathBuf {
    home.join("runtime")
}

/// Where the runtime copies live: [`root`] itself, for every build.
#[must_use]
pub fn dir(home: &Path) -> PathBuf {
    root(home)
}

/// Held across [`install`] and [`collect`] by app instances of any build.
fn lock(home: &Path) -> io::Result<std::fs::File> {
    std::fs::create_dir_all(home)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("runtime.lock"))?;
    rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::LockExclusive)?;
    Ok(file)
}

/// Copy this build's server and agent from the bundle's `Contents/MacOS`
/// (only read, so a disk image or translocated bundle works) into the
/// runtime directory, replacing each file atomically unless it is already
/// the same; returns the runtime `silo`.
///
/// # Errors
/// A file cannot be read, copied or renamed.
pub fn install(home: &Path, macos: &Path) -> io::Result<PathBuf> {
    let _lock = lock(home)?;
    let dir = dir(home);
    std::fs::create_dir_all(&dir)?;
    for (from, to) in FILES {
        let (src, dst) = (macos.join(from), dir.join(to));
        let modified = std::fs::metadata(&src)?.modified()?;
        if same(&src, &dst)? {
            continue;
        }
        let tmp = dir.join(format!(".{to}.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        // clonefile/copyfile: the embedded code signature comes along.
        let copied =
            std::fs::copy(&src, &tmp).and_then(|_| std::fs::File::open(&tmp)?.set_modified(modified)).and_then(|()| std::fs::rename(&tmp, &dst));
        if copied.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        copied?;
        tracing::info!("runtime: installed {}", dst.display());
    }
    Ok(dir.join("silo"))
}

/// `dst` is a copy of `src`: same length and modification time (copies
/// take the source's).
fn same(src: &Path, dst: &Path) -> io::Result<bool> {
    let Ok(dst) = std::fs::metadata(dst) else { return Ok(false) };
    let src = std::fs::metadata(src)?;
    Ok(src.len() == dst.len() && src.modified()? == dst.modified()?)
}

/// Remove the legacy per-build runtime directories (`runtime/<build>/`, from
/// before the fixed path) that no running process was started from and
/// nothing touched recently, and copies left half-done by processes that
/// are gone.
pub fn collect(home: &Path) {
    let Ok(_lock) = lock(home) else { return };
    let Some(running) = running_exes() else { return };
    let Ok(root) = root(home).canonicalize() else { return };
    let Ok(entries) = std::fs::read_dir(&root) else { return };
    let now = SystemTime::now();
    let fresh = |dir: &Path| std::fs::metadata(dir).and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() < FRESH);
    let dirs: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_dir() && !fresh(p)).collect();
    for dir in victims(&dirs, &running) {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!("runtime: removed {}", dir.display()),
            Err(error) => tracing::warn!("runtime: remove {}: {error}", dir.display()),
        }
    }
    for tmp in std::fs::read_dir(&root).into_iter().flatten().filter_map(Result::ok).map(|e| e.path()) {
        if temp_pid(&tmp).is_some_and(|pid| !alive(pid)) && std::fs::remove_file(&tmp).is_ok() {
            tracing::info!("runtime: removed {}", tmp.display());
        }
    }
}

fn victims(dirs: &[PathBuf], running: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter().filter(|dir| !running.iter().any(|exe| exe.starts_with(dir))).cloned().collect()
}

/// The pid in a temp copy's name `.<file>.<pid>`.
fn temp_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?.strip_prefix('.')?;
    name.rsplit_once('.')?.1.parse().ok()
}

fn alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) else { return false };
    !matches!(rustix::process::test_kill_process(pid), Err(rustix::io::Errno::SRCH))
}

/// The executable of process `pid`, if it runs.
#[must_use]
#[cfg(target_os = "macos")]
pub fn exe_of(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let pid = i32::try_from(pid).ok()?;
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), u32::try_from(buf.len()).ok()?) };
    let len = usize::try_from(len).ok().filter(|&n| n > 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len])))
}

#[must_use]
#[cfg(not(target_os = "macos"))]
pub fn exe_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Every running process's executable; `None` when they cannot be listed.
#[cfg(target_os = "macos")]
fn running_exes() -> Option<Vec<PathBuf>> {
    let size = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids = vec![0i32; usize::try_from(size).ok()? + 64];
    let bytes = i32::try_from(pids.len() * std::mem::size_of::<i32>()).ok()?;
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(n).ok().filter(|&n| n > 0)?);
    Some(pids.into_iter().filter_map(|pid| exe_of(u32::try_from(pid).ok()?)).collect())
}

#[cfg(not(target_os = "macos"))]
fn running_exes() -> Option<Vec<PathBuf>> {
    let entries = std::fs::read_dir("/proc").ok()?;
    Some(entries.filter_map(Result::ok).filter_map(|e| exe_of(e.file_name().to_str()?.parse().ok()?)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("silo-runtime-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn one_runtime_dir_for_every_build() {
        let home = Path::new("/Users/bat/Library/Application Support/silo");
        assert_eq!(dir(home), home.join("runtime"));
        assert_eq!(dir(home), root(home));
    }

    #[test]
    fn install_copies_once_and_replaces_changes_with_a_fresh_file() {
        let base = temp("install");
        let (home, macos) = (base.join("home"), base.join("Silo.app/Contents/MacOS"));
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(macos.join("silo-server"), "server").unwrap();
        std::fs::write(macos.join("silo-menubar"), "agent").unwrap();
        let silo = install(&home, &macos).unwrap();
        assert_eq!(silo, home.join("runtime/silo"));
        assert_eq!(std::fs::read_to_string(&silo).unwrap(), "server");
        assert_eq!(std::fs::read_to_string(home.join("runtime/silo-menubar")).unwrap(), "agent");
        let inode = |p: &Path| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(p).unwrap());
        let before = inode(&silo);
        install(&home, &macos).unwrap();
        assert_eq!(inode(&silo), before, "the same build leaves the file alone");
        // Another build: the same path, a new file (a running server keeps the old one).
        let running = std::fs::File::open(&silo).unwrap();
        std::fs::write(macos.join("silo-server"), "server2").unwrap();
        install(&home, &macos).unwrap();
        assert_eq!(std::fs::read_to_string(&silo).unwrap(), "server2");
        assert_ne!(inode(&silo), before, "replaced by a rename, not written in place");
        assert_eq!(std::os::unix::fs::MetadataExt::ino(&running.metadata().unwrap()), before);
        let mut names: Vec<_> = std::fs::read_dir(home.join("runtime")).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, ["silo", "silo-menubar"]);
        assert!(install(&home, &base).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn installs_from_a_read_only_bundle() {
        use std::os::unix::fs::PermissionsExt;
        let base = temp("read-only");
        let (home, macos) = (base.join("home"), base.join("Silo.app/Contents/MacOS"));
        std::fs::create_dir_all(&macos).unwrap();
        for name in ["silo-server", "silo-menubar"] {
            std::fs::write(macos.join(name), name).unwrap();
            std::fs::set_permissions(macos.join(name), std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        std::fs::set_permissions(&macos, std::fs::Permissions::from_mode(0o555)).unwrap();
        let silo = install(&home, &macos).unwrap();
        assert_eq!(std::fs::read_to_string(&silo).unwrap(), "silo-server");
        std::fs::set_permissions(&macos, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(macos.join("silo-menubar"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(macos.join("silo-menubar"), "new").unwrap();
        install(&home, &macos).unwrap();
        assert_eq!(std::fs::read_to_string(dir(&home).join("silo-menubar")).unwrap(), "new");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn collect_removes_legacy_build_dirs_and_keeps_the_files() {
        let home = temp("collect");
        let old = SystemTime::now() - FRESH - Duration::from_secs(1);
        let root = root(&home);
        for build in ["a", "b", "new"] {
            std::fs::create_dir_all(root.join(build)).unwrap();
            if build != "new" {
                std::fs::File::open(root.join(build)).unwrap().set_modified(old).unwrap();
            }
        }
        std::fs::write(root.join("silo"), "").unwrap();
        let (dead, mine) = (root.join(".silo.999999999"), root.join(format!(".silo.{}", std::process::id())));
        std::fs::write(&dead, "").unwrap();
        std::fs::write(&mine, "").unwrap();
        collect(&home);
        let mut left: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left, [format!(".silo.{}", std::process::id()).as_str(), "new", "silo"]);
        assert!(!dead.exists() && mine.exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn finds_running_executables() {
        let me = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert_eq!(exe_of(std::process::id()).map(|e| e.canonicalize().unwrap()), Some(me.clone()));
        assert!(running_exes().unwrap().iter().any(|e| e.canonicalize().ok().as_ref() == Some(&me)));
        assert_eq!(temp_pid(Path::new("/r/b/.silo-menubar.42")), Some(42));
        assert_eq!(temp_pid(Path::new("/r/b/silo")), None);
    }

    #[test]
    fn collects_unused_dirs_only() {
        let root = Path::new("/h/runtime");
        let dirs = ["old", "busy", "agent"].map(|d| root.join(d));
        let running = [PathBuf::from("/h/runtime/busy/silo"), PathBuf::from("/h/runtime/agent/silo-menubar"), PathBuf::from("/usr/bin/login")];
        assert_eq!(victims(&dirs, &running), [root.join("old")]);
        assert_eq!(victims(&dirs, &[PathBuf::from("/h/runtime/silo")]), dirs.to_vec(), "the fixed copy keeps no legacy dir");
    }
}
