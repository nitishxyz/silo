//! The server and menu-bar agent copied out of Silo.app to one fixed place,
//! `<SILO_HOME>/runtime/{Silo Server.app,silo-menubar}`, so the launchd job
//! never holds the app bundle open and the app can be replaced while the
//! server runs. The server ships as an app bundle of its own (`Silo
//! Server.app`, from the app's `Contents/Helpers`): macOS privacy prompts and
//! System Settings name and show it, under its own bundle id, for every
//! program run in Silo's terminals (which the server is the responsible
//! process of). Each build replaces the copies atomically (a directory
//! swap: running processes keep theirs, and macOS never sees signed code
//! overwritten in place).
//!
//! The menu-bar agent ships and is copied here as a helper app too (`Silo
//! Menu Bar.app`): its own bundle id, so Launch Services never takes it for
//! Silo.app, and a whole bundle, as its executable's signature seals the
//! bundle's Info.plist and resources (a bare copy fails verification).

use std::{
    io,
    os::fd::AsFd,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use silo_proto::Channel;

/// The server's bundle in the runtime directory: the same name for every
/// channel and build.
pub const SERVER_APP: &str = "Silo Server.app";
/// A server bundle's executable, relative to the bundle.
pub const BUNDLE_EXE: &str = "Contents/MacOS/silo";
/// The menu-bar agent's executable name.
pub const MENUBAR: &str = "silo-menubar";
/// The menu-bar agent's bundle in the runtime directory: the same name for
/// every channel and build.
pub const MENUBAR_APP: &str = "Silo Menu Bar.app";
/// A menu-bar bundle's executable, relative to the bundle.
const MENUBAR_EXE: &str = "Contents/MacOS/silo-menubar";
/// The server copied here before it was a bundle.
const LEGACY_EXE: &str = "silo";

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

/// The runtime server: `<home>/runtime/Silo Server.app/Contents/MacOS/silo`.
#[must_use]
pub fn server_exe(home: &Path) -> PathBuf {
    dir(home).join(SERVER_APP).join(BUNDLE_EXE)
}

/// The runtime menu-bar agent:
/// `<home>/runtime/Silo Menu Bar.app/Contents/MacOS/silo-menubar`.
#[must_use]
pub fn menubar_exe(home: &Path) -> PathBuf {
    dir(home).join(MENUBAR_APP).join(MENUBAR_EXE)
}

/// `channel`'s server helper bundle in app bundle `app`:
/// `<app>/Contents/Helpers/Silo Server.app` (dev: `SiloDev Server.app`).
#[must_use]
pub fn helper_app(app: &Path, channel: Channel) -> PathBuf {
    app.join("Contents/Helpers").join(format!("{}.app", channel.server_name()))
}

/// The server executable of [`helper_app`].
#[must_use]
pub fn helper_exe(app: &Path, channel: Channel) -> PathBuf {
    helper_app(app, channel).join(BUNDLE_EXE)
}

/// `channel`'s menu-bar agent helper bundle in app bundle `app`:
/// `<app>/Contents/Helpers/Silo Menu Bar.app` (dev: `SiloDev Menu Bar.app`).
#[must_use]
pub fn menubar_app(app: &Path, channel: Channel) -> PathBuf {
    app.join("Contents/Helpers").join(format!("{}.app", channel.menubar_name()))
}

/// The agent executable of [`menubar_app`].
#[must_use]
pub fn menubar_helper_exe(app: &Path, channel: Channel) -> PathBuf {
    menubar_app(app, channel).join(MENUBAR_EXE)
}

/// Whether `exe` runs from `home`'s runtime directory: the bundle's `silo`,
/// or the bare `silo` copied there before the bundle (both canonical).
#[must_use]
pub fn is_runtime_exe(exe: &Path, home: &Path) -> bool {
    exe == server_exe(home) || exe == dir(home).join(LEGACY_EXE)
}

/// For a bundled server `<X>.app/Contents/Helpers/<S>.app/Contents/MacOS/silo`:
/// `<S>.app` and `<X>.app`'s menu-bar agent bundle of `<S>`'s channel.
fn sources(server_exe: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let bundle = server_exe
        .ancestors()
        .nth(3)
        .filter(|b| b.extension().is_some_and(|e| e == "app") && b.join(BUNDLE_EXE) == server_exe)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{} is not a server bundle's executable", server_exe.display())))?;
    let app = bundle
        .parent()
        .filter(|helpers| helpers.ends_with("Contents/Helpers"))
        .and_then(|helpers| helpers.parent()?.parent())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{} is not in an app's Contents/Helpers", bundle.display())))?;
    let channel = [Channel::Release, Channel::Dev]
        .into_iter()
        .find(|c| bundle.file_name() == Some(format!("{}.app", c.server_name()).as_ref()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{} is no channel's server bundle", bundle.display())))?;
    Ok((bundle.to_owned(), menubar_app(app, channel)))
}

/// Held across [`install`] and [`collect`] by app instances of any build.
fn lock(home: &Path) -> io::Result<std::fs::File> {
    std::fs::create_dir_all(home)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("runtime.lock"))?;
    rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::LockExclusive)?;
    Ok(file)
}

/// Copy this build's server and menu-bar agent bundles (`server_exe`'s and
/// its sibling, in the app's `Contents/Helpers`) into the
/// runtime directory, only reading the app (a disk image or translocated
/// bundle works), replacing each atomically unless it is already the same;
/// returns the runtime server ([`server_exe`]).
///
/// # Errors
/// `server_exe` is not a bundled server, or a file cannot be read, copied
/// or renamed.
pub fn install(home: &Path, server_exe: &Path) -> io::Result<PathBuf> {
    let (bundle, menubar) = sources(server_exe)?;
    let _lock = lock(home)?;
    let dir = dir(home);
    std::fs::create_dir_all(&dir)?;
    install_bundle(&bundle, &dir.join(SERVER_APP), BUNDLE_EXE)?;
    install_bundle(&menubar, &dir.join(MENUBAR_APP), MENUBAR_EXE)?;
    Ok(self::server_exe(home))
}

/// A temporary name beside `dst`: `.<name>.<pid>` (see [`temp_pid`]).
fn temp_beside(dst: &Path) -> PathBuf {
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    dst.with_file_name(format!(".{name}.{}", std::process::id()))
}

/// Copy the bundle `src` (executable `exe`) to `dst` unless [`same_bundle`]:
/// a fresh copy beside it, swapped in atomically, then the old one removed.
fn install_bundle(src: &Path, dst: &Path, exe: &str) -> io::Result<()> {
    if same_bundle(src, dst, exe)? {
        return Ok(());
    }
    let tmp = temp_beside(dst);
    remove_any(&tmp)?;
    let copied = copy_tree(src, &tmp).and_then(|()| replace_dir(&tmp, dst));
    // After a swap `tmp` holds the old bundle; else a partial copy.
    if let Err(error) = remove_any(&tmp) {
        tracing::warn!("runtime: remove {}: {error}", tmp.display());
    }
    copied?;
    tracing::info!("runtime: installed {}", dst.display());
    Ok(())
}

/// Put directory `new` at `dst`: an atomic swap with an existing `dst` (the
/// old one is then at `new`), else a rename.
fn replace_dir(new: &Path, dst: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(dst).is_err() {
        return std::fs::rename(new, dst);
    }
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    match renameat_with(CWD, new, CWD, dst, RenameFlags::EXCHANGE) {
        Ok(()) => Ok(()),
        // A file system without swaps: move the old one aside first (a
        // moment without a bundle, never a half-written one).
        Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP) => {
            let name = dst.file_name().unwrap_or_default().to_string_lossy();
            let old = dst.with_file_name(format!(".{name}.old.{}", std::process::id()));
            std::fs::rename(dst, &old)?;
            std::fs::rename(new, dst)?;
            std::fs::rename(&old, new)
        }
        Err(error) => Err(error.into()),
    }
}

/// Remove a file, link or directory tree at `path`, if any.
fn remove_any(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    }
}

/// Copy one file, keeping its modification time ([`same`] relies on it).
fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    let modified = std::fs::metadata(src)?.modified()?;
    // clonefile/copyfile: the embedded code signature comes along.
    std::fs::copy(src, dst)?;
    std::fs::File::open(dst)?.set_modified(modified)
}

/// Copy directory `src` to the new path `dst`: files (with their
/// modification times), directories and symlinks (as links).
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::create_dir(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            copy_file(&from, &to)?;
        }
    }
    Ok(())
}

/// `dst` is a copy of `src`: same length and modification time (copies
/// take the source's).
fn same(src: &Path, dst: &Path) -> io::Result<bool> {
    let Ok(dst) = std::fs::metadata(dst) else { return Ok(false) };
    let src = std::fs::metadata(src)?;
    Ok(src.len() == dst.len() && src.modified()? == dst.modified()?)
}

/// Bundle `dst` is a copy of `src`: the [`same`] executable `exe`, and the
/// same Info.plist and sealed resources list.
fn same_bundle(src: &Path, dst: &Path, exe: &str) -> io::Result<bool> {
    if !same(&src.join(exe), &dst.join(exe))? {
        return Ok(false);
    }
    let read = |bundle: &Path, file: &str| std::fs::read(bundle.join(file)).ok();
    Ok(["Contents/Info.plist", "Contents/_CodeSignature/CodeResources"].iter().all(|file| read(src, file) == read(dst, file)))
}

/// Remove what nothing needs any more: the legacy per-build runtime
/// directories (`runtime/<build>/`, from before the fixed path) that no
/// running process was started from and nothing touched recently, the
/// legacy bare `runtime/silo` and `runtime/silo-menubar` (from before the
/// bundles) once no process runs them, and copies left half-done by
/// processes that are gone.
pub fn collect(home: &Path) {
    let Ok(_lock) = lock(home) else { return };
    let Some(running) = running_exes() else { return };
    let Ok(root) = root(home).canonicalize() else { return };
    let Ok(entries) = std::fs::read_dir(&root) else { return };
    let now = SystemTime::now();
    let fresh = |dir: &Path| std::fs::metadata(dir).and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() < FRESH);
    let legacy = |p: &Path| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n != SERVER_APP && n != MENUBAR_APP && !n.starts_with('.'));
    let dirs: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_dir() && legacy(p) && !fresh(p)).collect();
    let unused = [LEGACY_EXE, MENUBAR].map(|name| root.join(name)).into_iter().filter(|exe| exe.is_file() && !running.contains(exe));
    for path in victims(&dirs, &running).into_iter().chain(unused) {
        match remove_any(&path) {
            Ok(()) => tracing::info!("runtime: removed {}", path.display()),
            Err(error) => tracing::warn!("runtime: remove {}: {error}", path.display()),
        }
    }
    for tmp in std::fs::read_dir(&root).into_iter().flatten().filter_map(Result::ok).map(|e| e.path()) {
        if temp_pid(&tmp).is_some_and(|pid| !alive(pid)) && remove_any(&tmp).is_ok() {
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

/// The name of process `pid` (its executable's file name, as it was
/// started: it survives the file's renaming or removal), if it runs.
#[must_use]
#[cfg(target_os = "macos")]
pub fn name_of(pid: u32) -> Option<String> {
    let mut buf = [0u8; 64];
    let len = unsafe { libc::proc_name(i32::try_from(pid).ok()?, buf.as_mut_ptr().cast(), 64) };
    let len = usize::try_from(len).ok().filter(|&n| n > 0)?;
    Some(String::from_utf8_lossy(&buf[..len]).into_owned())
}

#[must_use]
#[cfg(not(target_os = "macos"))]
pub fn name_of(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|n| n.trim_end().to_owned())
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

    /// A fake `Silo.app` holding a server bundle (`server`: its executable's
    /// text) and agent; the bundled server's executable.
    fn app(base: &Path, server: &str, agent: &str) -> PathBuf {
        let app = base.join("Silo.app");
        let bundle = helper_app(&app, Channel::Release);
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(bundle.join("Contents/Resources")).unwrap();
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(bundle.join(BUNDLE_EXE), server).unwrap();
        std::fs::write(bundle.join("Contents/Info.plist"), "<plist/>").unwrap();
        std::fs::write(bundle.join("Contents/Resources/Silo.icns"), "icon").unwrap();
        let menubar = menubar_helper_exe(&app, Channel::Release);
        std::fs::create_dir_all(menubar.parent().unwrap()).unwrap();
        std::fs::write(menubar, agent).unwrap();
        bundle.join(BUNDLE_EXE)
    }

    #[test]
    fn helper_paths() {
        let app = Path::new("/Applications/Silo.app");
        assert_eq!(helper_exe(app, Channel::Release), Path::new("/Applications/Silo.app/Contents/Helpers/Silo Server.app/Contents/MacOS/silo"));
        let dev = Path::new("/w/target/debug/SiloDev.app");
        assert_eq!(helper_exe(dev, Channel::Dev), Path::new("/w/target/debug/SiloDev.app/Contents/Helpers/SiloDev Server.app/Contents/MacOS/silo"));
        let (bundle, agent) = sources(&helper_exe(app, Channel::Release)).unwrap();
        assert_eq!(bundle, helper_app(app, Channel::Release));
        assert_eq!(agent, Path::new("/Applications/Silo.app/Contents/Helpers/Silo Menu Bar.app"));
        assert_eq!(menubar_helper_exe(app, Channel::Release), agent.join("Contents/MacOS/silo-menubar"));
        let (_, dev_agent) = sources(&helper_exe(dev, Channel::Dev)).unwrap();
        assert_eq!(
            menubar_helper_exe(dev, Channel::Dev),
            Path::new("/w/target/debug/SiloDev.app/Contents/Helpers/SiloDev Menu Bar.app/Contents/MacOS/silo-menubar")
        );
        assert_eq!(dev_agent, menubar_app(dev, Channel::Dev));
        assert!(sources(Path::new("/Applications/Silo.app/Contents/Helpers/Other.app/Contents/MacOS/silo")).is_err(), "no channel's server");
        assert!(sources(Path::new("/Applications/Silo.app/Contents/MacOS/silo-server")).is_err(), "the old layout");
        assert!(sources(Path::new("/x/Silo Server.app/Contents/MacOS/silo")).is_err(), "not in an app's Helpers");
        let home = Path::new("/h");
        assert_eq!(server_exe(home), Path::new("/h/runtime/Silo Server.app/Contents/MacOS/silo"));
        assert_eq!(menubar_exe(home), Path::new("/h/runtime/Silo Menu Bar.app/Contents/MacOS/silo-menubar"));
        assert!(is_runtime_exe(&server_exe(home), home) && is_runtime_exe(Path::new("/h/runtime/silo"), home));
        assert!(!is_runtime_exe(Path::new("/h/runtime/silo-menubar"), home) && !is_runtime_exe(Path::new("/h/runtime/b/silo"), home));
        // The server's own view of its runtime directory agrees.
        assert_eq!(silo_server::bundle::runtime_dir(&server_exe(home), home), Some(dir(home)));
        assert_eq!(silo_server::bundle::runtime_dir(&dir(home).join(LEGACY_EXE), home), Some(dir(home)));
    }

    #[test]
    fn install_copies_once_and_swaps_in_changes() {
        let base = temp("install");
        let home = base.join("home");
        let bundled = app(&base, "server", "agent");
        let silo = install(&home, &bundled).unwrap();
        assert_eq!(silo, home.join("runtime/Silo Server.app/Contents/MacOS/silo"));
        assert_eq!(std::fs::read_to_string(&silo).unwrap(), "server");
        let copy = home.join("runtime").join(SERVER_APP);
        assert_eq!(std::fs::read_to_string(copy.join("Contents/Info.plist")).unwrap(), "<plist/>");
        assert_eq!(std::fs::read_to_string(copy.join("Contents/Resources/Silo.icns")).unwrap(), "icon");
        assert_eq!(std::fs::read_to_string(menubar_exe(&home)).unwrap(), "agent");
        let inode = |p: &Path| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(p).unwrap());
        let (before, dir_before) = (inode(&silo), inode(&copy));
        install(&home, &bundled).unwrap();
        assert_eq!((inode(&silo), inode(&copy)), (before, dir_before), "the same build leaves the bundle alone");
        // Another build: the same path, a new bundle (a running server keeps its files).
        let running = std::fs::File::open(&silo).unwrap();
        std::fs::write(&bundled, "server2").unwrap();
        install(&home, &bundled).unwrap();
        assert_eq!(std::fs::read_to_string(&silo).unwrap(), "server2");
        assert_ne!(inode(&copy), dir_before, "swapped in, not written in place");
        assert_eq!(std::os::unix::fs::MetadataExt::ino(&running.metadata().unwrap()), before);
        // Only the Info.plist changed: still a new bundle.
        let dir_before = inode(&copy);
        let plist = bundled.ancestors().nth(2).unwrap().join("Info.plist");
        std::fs::write(&plist, "<plist>2</plist>").unwrap();
        install(&home, &bundled).unwrap();
        assert_ne!(inode(&copy), dir_before);
        assert_eq!(std::fs::read_to_string(copy.join("Contents/Info.plist")).unwrap(), "<plist>2</plist>");
        let mut names: Vec<_> = std::fs::read_dir(home.join("runtime")).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, [MENUBAR_APP, SERVER_APP], "no copy left behind");
        // Only the agent changed: its bundle is swapped, the server's left alone.
        let (server_dir, agent_dir) = (inode(&copy), inode(&home.join("runtime").join(MENUBAR_APP)));
        std::fs::write(menubar_helper_exe(&base.join("Silo.app"), Channel::Release), "agent2").unwrap();
        install(&home, &bundled).unwrap();
        assert_eq!(std::fs::read_to_string(menubar_exe(&home)).unwrap(), "agent2");
        assert_eq!(inode(&copy), server_dir);
        assert_ne!(inode(&home.join("runtime").join(MENUBAR_APP)), agent_dir);
        assert!(install(&home, &base.join("Silo.app/Contents/MacOS/silo-menubar")).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn installs_from_a_read_only_bundle() {
        use std::os::unix::fs::PermissionsExt;
        let base = temp("read-only");
        let home = base.join("home");
        let bundled = app(&base, "server", "agent");
        let read_only = |path: &Path, mode| std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        let bundle = bundled.ancestors().nth(3).unwrap().to_owned();
        for dir in ["Contents/MacOS", "Contents/Resources", "Contents", ""] {
            for entry in std::fs::read_dir(bundle.join(dir)).unwrap() {
                read_only(&entry.unwrap().path(), 0o555);
            }
        }
        read_only(&bundle, 0o555);
        let silo = install(&home, &bundled).unwrap();
        assert_eq!(std::fs::read_to_string(&silo).unwrap(), "server");
        // A read-only copy is still replaced, and removed.
        read_only(&bundle, 0o755);
        read_only(&bundle.join("Contents"), 0o755);
        read_only(&bundle.join("Contents/MacOS"), 0o755);
        read_only(&bundled, 0o755);
        std::fs::write(&bundled, "new").unwrap();
        install(&home, &bundled).unwrap();
        assert_eq!(std::fs::read_to_string(&silo).unwrap(), "new");
        assert_eq!(std::fs::read_dir(dir(&home)).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn collect_removes_legacy_build_dirs_and_files() {
        let home = temp("collect");
        let old = SystemTime::now() - FRESH - Duration::from_secs(1);
        let root = root(&home);
        for build in ["a", "b", "new", SERVER_APP, MENUBAR_APP] {
            std::fs::create_dir_all(root.join(build)).unwrap();
            if build != "new" {
                std::fs::File::open(root.join(build)).unwrap().set_modified(old).unwrap();
            }
        }
        // The bare `silo` and `silo-menubar` from before the bundles: no process runs them here.
        std::fs::write(root.join("silo"), "").unwrap();
        std::fs::write(root.join("silo-menubar"), "").unwrap();
        let (dead, mine) = (root.join(".silo-menubar.999999999"), root.join(format!(".silo-menubar.{}", std::process::id())));
        std::fs::write(&dead, "").unwrap();
        std::fs::write(&mine, "").unwrap();
        let dead_bundle = root.join(".Silo Server.app.999999999");
        std::fs::create_dir_all(dead_bundle.join("Contents")).unwrap();
        collect(&home);
        let mut left: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, [format!(".silo-menubar.{}", std::process::id()).as_str(), MENUBAR_APP, SERVER_APP, "new"]);
        assert!(!dead.exists() && mine.exists() && !dead_bundle.exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn finds_running_executables() {
        let me = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert_eq!(exe_of(std::process::id()).map(|e| e.canonicalize().unwrap()), Some(me.clone()));
        assert!(running_exes().unwrap().iter().any(|e| e.canonicalize().ok().as_ref() == Some(&me)));
        let name = name_of(std::process::id()).unwrap();
        assert!(!name.is_empty() && me.file_name().unwrap().to_str().unwrap().starts_with(&name), "{name}");
        assert_eq!(temp_pid(Path::new("/r/b/.silo-menubar.42")), Some(42));
        assert_eq!(temp_pid(Path::new("/r/b/silo")), None);
        assert_eq!(temp_pid(Path::new("/r/.Silo Server.app.7")), Some(7));
        assert_eq!(temp_pid(Path::new("/r/.Silo Server.app.old.8")), Some(8));
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
