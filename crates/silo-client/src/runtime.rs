//! Where a Mac's server runs from: one copy of the self-contained server
//! bundle at `<SILO_HOME>/runtime/Silo Server.app`, whichever way Silo was
//! installed.
//!
//! ```text
//! Silo Server.app                      server bundle id (xyz.nitish.silo.server)
//! └── Contents/
//!     ├── MacOS/silo                   server, CLI and otto (`silo otto …`)
//!     ├── Helpers/Silo Menu Bar.app    the menu-bar agent
//!     ├── Resources/                   icon
//!     └── Info.plist
//! ```
//!
//! Silo.app ships it as `Contents/Helpers/Silo Server.app` and copies it
//! here when it starts; a server-only install (install.sh) unpacks the
//! release's bundle and its `silo up` (or `silo install`) copies it here.
//! The launchd job, the CLI links ([`link_cli`]) and the menu-bar agent all
//! run this copy, so the bundle it came from can be replaced or removed
//! while the server runs, and installing Silo.app later finds the same
//! path, launchd job and bundle id.
//!
//! The server is a bundle of its own so macOS privacy prompts and System
//! Settings name and show it, under its own bundle id, for every program
//! run in Silo's terminals (the server is their responsible process). The
//! menu-bar agent is a bundle inside it with its own bundle id, so Launch
//! Services never takes it for Silo.app. Each copy replaces the old one
//! atomically (a directory swap: running processes keep their files, and
//! macOS never sees signed code overwritten in place).

use std::{
    io,
    os::fd::AsFd,
    path::{Path, PathBuf},
};

use silo_proto::Channel;

/// The server's bundle in the runtime directory: the same name for every
/// channel and build.
pub const SERVER_APP: &str = silo_proto::upgrade::SERVER_BUNDLE;
/// A server bundle's executable, relative to the bundle.
pub const BUNDLE_EXE: &str = "Contents/MacOS/silo";
/// The menu-bar agent's executable name.
pub const MENUBAR: &str = "silo-menubar";

/// `<home>/runtime`.
#[must_use]
pub fn root(home: &Path) -> PathBuf {
    home.join("runtime")
}

/// The runtime server bundle: `<home>/runtime/Silo Server.app`.
#[must_use]
pub fn server_app(home: &Path) -> PathBuf {
    root(home).join(SERVER_APP)
}

/// The runtime server: `<home>/runtime/Silo Server.app/Contents/MacOS/silo`.
#[must_use]
pub fn server_exe(home: &Path) -> PathBuf {
    server_app(home).join(BUNDLE_EXE)
}

/// The runtime menu-bar agent, inside the runtime server bundle.
#[must_use]
pub fn menubar_exe(home: &Path) -> PathBuf {
    menubar_in(&server_app(home), silo_proto::CHANNEL)
}

/// `channel`'s menu-bar agent inside server bundle `server`:
/// `<server>/Contents/Helpers/<Silo|SiloDev> Menu Bar.app/Contents/MacOS/silo-menubar`.
#[must_use]
pub fn menubar_in(server: &Path, channel: Channel) -> PathBuf {
    server.join("Contents/Helpers").join(format!("{}.app", channel.menubar_name())).join("Contents/MacOS").join(MENUBAR)
}

/// `channel`'s server bundle in app bundle `app`:
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

/// Whether `exe` (canonical) is `home`'s runtime server.
#[must_use]
pub fn is_runtime_exe(exe: &Path, home: &Path) -> bool {
    exe == server_exe(home)
}

/// The server bundle whose executable `exe` is: a channel's
/// `<Silo|SiloDev> Server.app` anywhere (in an app's `Contents/Helpers`,
/// unpacked from a release), or the runtime copy.
#[must_use]
pub fn server_bundle(exe: &Path) -> Option<&Path> {
    let bundle = exe.ancestors().nth(3)?;
    let name = bundle.file_name()?.to_str()?;
    let known = name == SERVER_APP || [Channel::Release, Channel::Dev].iter().any(|c| name == format!("{}.app", c.server_name()));
    (known && bundle.join(BUNDLE_EXE) == exe).then_some(bundle)
}

/// Held across [`install`], [`swap_in`] and [`collect`] by every process.
fn lock(home: &Path) -> io::Result<std::fs::File> {
    std::fs::create_dir_all(home)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("runtime.lock"))?;
    rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::LockExclusive)?;
    Ok(file)
}

/// Copy the server bundle `server_exe` belongs to ([`server_bundle`]) to
/// the runtime directory, only reading it (a disk image or translocated
/// app works), unless the copy there is already the same; returns the
/// runtime server ([`server_exe`]). The runtime server itself is left as
/// it is.
///
/// # Errors
/// `server_exe` is not a server bundle's, or a file cannot be read, copied
/// or renamed.
pub fn install(home: &Path, server_exe: &Path) -> io::Result<PathBuf> {
    let installed = self::server_exe(home);
    if server_exe == installed {
        return Ok(installed);
    }
    let bundle = server_bundle(server_exe)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{} is not a server bundle's executable", server_exe.display())))?;
    let _lock = lock(home)?;
    std::fs::create_dir_all(root(home))?;
    let dst = server_app(home);
    if same_bundle(bundle, &dst)? {
        return Ok(installed);
    }
    let tmp = temp_beside(&dst);
    remove_any(&tmp)?;
    let copied = copy_tree(bundle, &tmp).and_then(|()| replace_dir(&tmp, &dst));
    // After a swap `tmp` holds the old bundle; else a partial copy.
    crate::launch_services::forget(&tmp);
    if let Err(error) = remove_any(&tmp) {
        tracing::warn!("runtime: remove {}: {error}", tmp.display());
    }
    copied?;
    crate::launch_services::refresh(&dst);
    tracing::info!("runtime: installed {} from {}", dst.display(), bundle.display());
    Ok(installed)
}

/// A fresh staging path for a new server bundle on the runtime
/// directory's volume (for [`swap_in`]): `<home>/runtime/.Silo Server.app.<pid>.n<hex>`,
/// collected once this process is gone.
///
/// # Errors
/// The runtime directory cannot be created.
pub fn staging(home: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(root(home))?;
    let path = root(home).join(format!(".{SERVER_APP}.{}.n{:08x}", std::process::id(), rand::random::<u32>()));
    remove_any(&path)?;
    Ok(path)
}

/// Put the bundle `staged` (a [`staging`] path) in place as the runtime
/// server bundle; `staged` then holds the previous bundle, if there was
/// one (`Ok(true)`): remove it, or [`swap_in`] it again to roll back.
///
/// # Errors
/// The rename fails.
pub fn swap_in(home: &Path, staged: &Path) -> io::Result<bool> {
    let _lock = lock(home)?;
    let dst = server_app(home);
    let had = std::fs::symlink_metadata(&dst).is_ok();
    replace_dir(staged, &dst)?;
    crate::launch_services::forget(staged);
    crate::launch_services::refresh(&dst);
    tracing::info!("runtime: swapped in {}", dst.display());
    Ok(had)
}

/// A temporary name beside `dst`: `.<name>.<pid>` (see [`temp_pid`]).
fn temp_beside(dst: &Path) -> PathBuf {
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    dst.with_file_name(format!(".{name}.{}", std::process::id()))
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
///
/// # Errors
/// It is there and cannot be removed.
pub fn remove_any(path: &Path) -> io::Result<()> {
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

/// Bundle `dst` is a copy of `src`: every executable and code signature
/// the [`same`] (the server's and its nested menu-bar agent's), and the
/// same Info.plists.
fn same_bundle(src: &Path, dst: &Path) -> io::Result<bool> {
    let mut nested = vec![PathBuf::new()];
    if let Ok(helpers) = std::fs::read_dir(src.join("Contents/Helpers")) {
        nested.extend(helpers.filter_map(Result::ok).map(|e| Path::new("Contents/Helpers").join(e.file_name())));
    }
    for bundle in nested {
        let (s, d) = (src.join(&bundle), dst.join(&bundle));
        let exes = std::fs::read_dir(s.join("Contents/MacOS"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| Path::new("Contents/MacOS").join(e.file_name()));
        for exe in exes {
            if !same(&s.join(&exe), &d.join(&exe))? {
                return Ok(false);
            }
        }
        let read = |b: &Path, file: &str| std::fs::read(b.join(file)).ok();
        if !["Contents/Info.plist", "Contents/_CodeSignature/CodeResources"].iter().all(|f| read(&s, f) == read(&d, f)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Remove what nothing needs any more: everything in the runtime directory
/// but the server bundle that no running process was started from
/// (earlier layouts' copies), and staging copies of processes that are
/// gone.
pub fn collect(home: &Path) {
    let Ok(_lock) = lock(home) else { return };
    let Some(running) = running_exes() else { return };
    let Ok(root) = root(home).canonicalize() else { return };
    let Ok(entries) = std::fs::read_dir(&root) else { return };
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        let unused = match temp_pid(&path) {
            Some(pid) => !alive(pid),
            None => path.file_name() != Some(SERVER_APP.as_ref()) && !running.iter().any(|exe| exe.starts_with(&path)),
        };
        if !unused {
            continue;
        }
        match remove_any(&path) {
            Ok(()) => tracing::info!("runtime: removed {}", path.display()),
            Err(error) => tracing::warn!("runtime: remove {}: {error}", path.display()),
        }
    }
}

/// The pid in a staging copy's name: `.<file>.<pid>`, `.<file>.<pid>.n<hex>`
/// or `.<file>.old.<pid>`.
fn temp_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?.strip_prefix('.')?;
    let mut parts = name.rsplit('.');
    let last = parts.next()?;
    let pid = if last.starts_with('n') { parts.next()? } else { last };
    pid.bytes().all(|b| b.is_ascii_digit()).then(|| pid.parse().ok()).flatten()
}

fn alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) else { return false };
    !matches!(rustix::process::test_kill_process(pid), Err(rustix::io::Errno::SRCH))
}

/// What is at a link path now.
#[derive(Debug, PartialEq, Eq)]
enum Existing {
    Missing,
    Link(PathBuf),
    Other,
}

/// Point the CLI at `silo` (the runtime server, see [`server_exe`]):
/// `<SILO_HOME>/bin/silo` → `silo`, a stable path that survives updates,
/// and `<bin>/silo` → that stable path. `bin` is `~/.local/bin` unless
/// given; it is created when `create` (a server install), else only
/// linked into when it exists (the app). A file or a link to something
/// else there is replaced only when `replace` (`silo install` owns that
/// path); a link into an app or server bundle (which updates move) always.
/// Never edits shell files. The `<bin>/silo` link, if made.
pub fn link_cli(silo_home: &Path, silo: &Path, bin: Option<&Path>, create: bool, replace: bool) -> Option<PathBuf> {
    let stable = silo_home.join("bin/silo");
    if std::fs::read_link(&stable).ok().as_deref() != Some(silo) {
        let _ = std::fs::create_dir_all(silo_home.join("bin"));
        link(&stable, silo);
    }
    let dir = match bin {
        Some(dir) => dir.to_owned(),
        None => std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)?.join(".local/bin"),
    };
    if create {
        let _ = std::fs::create_dir_all(&dir);
    }
    if !dir.is_dir() {
        return None;
    }
    let user = dir.join("silo");
    let existing = match std::fs::symlink_metadata(&user) {
        Err(_) => Existing::Missing,
        Ok(meta) if meta.file_type().is_symlink() => std::fs::read_link(&user).map_or(Existing::Other, Existing::Link),
        Ok(_) => Existing::Other,
    };
    if existing == Existing::Link(stable.clone()) {
        return Some(user);
    }
    if replace || should_link(&existing) {
        link(&user, &stable);
        return Some(user);
    }
    None
}

/// Point `path` at `target` atomically.
fn link(path: &Path, target: &Path) {
    let tmp = path.with_file_name(format!(".silo-link-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    match std::os::unix::fs::symlink(target, &tmp).and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => tracing::info!("cli link: {} -> {}", path.display(), target.display()),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            tracing::warn!("cli link {}: {error}", path.display());
        }
    }
}

/// Missing, or a link into an app or server bundle (which an update moves).
fn should_link(existing: &Existing) -> bool {
    match existing {
        Existing::Missing => true,
        Existing::Link(to) => to.ancestors().any(|dir| dir.extension().is_some_and(|e| e == "app")),
        Existing::Other => false,
    }
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

    /// A fake `Silo.app` holding a server bundle (`server`: its executable's
    /// text) with its menu-bar agent (`agent`); the bundled server's
    /// executable.
    fn app(base: &Path, server: &str, agent: &str) -> PathBuf {
        let app = base.join("Silo.app");
        let bundle = helper_app(&app, Channel::Release);
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(bundle.join("Contents/Resources")).unwrap();
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(bundle.join(BUNDLE_EXE), server).unwrap();
        std::fs::write(bundle.join("Contents/Info.plist"), "<plist/>").unwrap();
        std::fs::write(bundle.join("Contents/Resources/Silo.icns"), "icon").unwrap();
        let menubar = menubar_in(&bundle, Channel::Release);
        std::fs::create_dir_all(menubar.parent().unwrap()).unwrap();
        std::fs::write(menubar, agent).unwrap();
        bundle.join(BUNDLE_EXE)
    }

    #[test]
    fn paths() {
        let app = Path::new("/Applications/Silo.app");
        let server = helper_exe(app, Channel::Release);
        assert_eq!(server, Path::new("/Applications/Silo.app/Contents/Helpers/Silo Server.app/Contents/MacOS/silo"));
        assert_eq!(
            menubar_in(&helper_app(app, Channel::Release), Channel::Release),
            Path::new("/Applications/Silo.app/Contents/Helpers/Silo Server.app/Contents/Helpers/Silo Menu Bar.app/Contents/MacOS/silo-menubar")
        );
        let dev = Path::new("/w/target/debug/SiloDev.app");
        assert_eq!(helper_exe(dev, Channel::Dev), Path::new("/w/target/debug/SiloDev.app/Contents/Helpers/SiloDev Server.app/Contents/MacOS/silo"));
        assert_eq!(server_bundle(&server), Some(helper_app(app, Channel::Release).as_path()));
        assert_eq!(server_bundle(&helper_exe(dev, Channel::Dev)), Some(helper_app(dev, Channel::Dev).as_path()));
        // A release's bundle unpacked anywhere, and the runtime copy.
        assert_eq!(server_bundle(Path::new("/tmp/x/Silo Server.app/Contents/MacOS/silo")), Some(Path::new("/tmp/x/Silo Server.app")));
        let home = Path::new("/h");
        assert_eq!(server_exe(home), Path::new("/h/runtime/Silo Server.app/Contents/MacOS/silo"));
        assert_eq!(server_bundle(&server_exe(home)), Some(server_app(home).as_path()));
        assert!(server_bundle(Path::new("/Applications/Silo.app/Contents/Helpers/Other.app/Contents/MacOS/silo")).is_none());
        assert!(server_bundle(Path::new("/x/Silo Server.app/Contents/MacOS/other")).is_none());
        assert!(server_bundle(Path::new("/usr/local/bin/silo")).is_none());
        assert!(is_runtime_exe(&server_exe(home), home) && !is_runtime_exe(Path::new("/h/runtime/silo"), home));
        assert_eq!(menubar_exe(home), menubar_in(&server_app(home), silo_proto::CHANNEL));
        // The server's own view of its runtime directory agrees.
        assert_eq!(silo_server::bundle::runtime_dir(&server_exe(home), home), Some(root(home)));
    }

    #[test]
    fn install_copies_once_and_swaps_in_changes() {
        let base = temp("install");
        let home = base.join("home");
        let bundled = app(&base, "server", "agent");
        let silo = install(&home, &bundled).unwrap();
        assert_eq!(silo, home.join("runtime/Silo Server.app/Contents/MacOS/silo"));
        assert_eq!(std::fs::read_to_string(&silo).unwrap(), "server");
        let copy = server_app(&home);
        assert_eq!(std::fs::read_to_string(copy.join("Contents/Info.plist")).unwrap(), "<plist/>");
        assert_eq!(std::fs::read_to_string(copy.join("Contents/Resources/Silo.icns")).unwrap(), "icon");
        assert_eq!(std::fs::read_to_string(menubar_in(&copy, Channel::Release)).unwrap(), "agent", "the agent comes inside it");
        assert_eq!(install(&home, &silo).unwrap(), silo, "the runtime copy installs nothing");
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
        // Only the nested agent changed: a new bundle too.
        let dir_before = inode(&copy);
        std::fs::write(menubar_in(server_bundle(&bundled).unwrap(), Channel::Release), "agent2").unwrap();
        install(&home, &bundled).unwrap();
        assert_ne!(inode(&copy), dir_before);
        assert_eq!(std::fs::read_to_string(menubar_in(&copy, Channel::Release)).unwrap(), "agent2");
        let names: Vec<_> = std::fs::read_dir(root(&home)).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, [SERVER_APP], "no copy left behind");
        assert!(install(&home, &base.join("Silo.app/Contents/MacOS/silo")).is_err(), "not a server bundle");
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
        assert_eq!(std::fs::read_dir(root(&home)).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn swaps_a_staged_bundle_in_and_back() {
        let base = temp("swap");
        let home = base.join("home");
        let bundled = app(&base, "one", "agent");
        let silo = install(&home, &bundled).unwrap();
        let staged = staging(&home).unwrap();
        assert!(staged.starts_with(root(&home)) && temp_pid(&staged) == Some(std::process::id()), "{}", staged.display());
        std::fs::create_dir_all(staged.join("Contents/MacOS")).unwrap();
        std::fs::write(staged.join(BUNDLE_EXE), "two").unwrap();
        assert!(swap_in(&home, &staged).unwrap());
        assert_eq!(std::fs::read_to_string(&silo).unwrap(), "two");
        assert_eq!(std::fs::read_to_string(staged.join(BUNDLE_EXE)).unwrap(), "one", "the previous bundle, to roll back to");
        assert!(swap_in(&home, &staged).unwrap());
        assert_eq!(std::fs::read_to_string(&silo).unwrap(), "one");
        remove_any(&staged).unwrap();
        // Into an empty runtime directory: nothing previous.
        let fresh = base.join("fresh");
        let staged = staging(&fresh).unwrap();
        std::fs::create_dir_all(staged.join("Contents/MacOS")).unwrap();
        assert!(!swap_in(&fresh, &staged).unwrap());
        assert!(server_app(&fresh).is_dir() && !staged.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn collect_keeps_only_the_server_bundle() {
        let home = temp("collect");
        let root = root(&home);
        // Earlier layouts' copies, which no process runs here.
        for dir in [SERVER_APP, "Silo Menu Bar.app", "0123456789abcdef"] {
            std::fs::create_dir_all(root.join(dir).join("Contents")).unwrap();
        }
        std::fs::write(root.join("silo"), "").unwrap();
        let (dead, mine) = (root.join(format!(".{SERVER_APP}.999999999.n0a")), root.join(format!(".{SERVER_APP}.{}", std::process::id())));
        std::fs::create_dir_all(dead.join("Contents")).unwrap();
        std::fs::create_dir_all(&mine).unwrap();
        collect(&home);
        let mut left: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, [format!(".{SERVER_APP}.{}", std::process::id()), SERVER_APP.to_owned()]);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn finds_running_executables() {
        let me = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert_eq!(exe_of(std::process::id()).map(|e| e.canonicalize().unwrap()), Some(me.clone()));
        assert!(running_exes().unwrap().iter().any(|e| e.canonicalize().ok().as_ref() == Some(&me)));
        let name = name_of(std::process::id()).unwrap();
        assert!(!name.is_empty() && me.file_name().unwrap().to_str().unwrap().starts_with(&name), "{name}");
        assert_eq!(temp_pid(Path::new("/r/.Silo Server.app.7")), Some(7));
        assert_eq!(temp_pid(Path::new("/r/.Silo Server.app.7.n0a1b2c3d")), Some(7));
        assert_eq!(temp_pid(Path::new("/r/.Silo Server.app.7.n12345678")), Some(7), "an all-digit suffix is no pid");
        assert_eq!(temp_pid(Path::new("/r/.Silo Server.app.old.8")), Some(8));
        assert_eq!(temp_pid(Path::new("/r/Silo Server.app")), None);
        assert_eq!(temp_pid(Path::new("/r/silo")), None);
    }

    #[test]
    fn cli_links() {
        let base = temp("links");
        let (home, bin) = (base.join("h"), base.join("bin"));
        let silo = home.join("runtime/Silo Server.app/Contents/MacOS/silo");
        let stable = home.join("bin/silo");
        assert_eq!(link_cli(&home, &silo, Some(&bin), false, false), None, "no bin folder: only the stable link");
        assert_eq!(std::fs::read_link(&stable).unwrap(), silo);
        assert_eq!(link_cli(&home, &silo, Some(&bin), true, false), Some(bin.join("silo")), "a server install creates it");
        assert_eq!(std::fs::read_link(bin.join("silo")).unwrap(), stable);
        // A file of another install: kept, unless the install owns the path.
        std::fs::remove_file(bin.join("silo")).unwrap();
        std::fs::write(bin.join("silo"), "old").unwrap();
        assert_eq!(link_cli(&home, &silo, Some(&bin), false, false), None);
        assert_eq!(std::fs::read_to_string(bin.join("silo")).unwrap(), "old");
        assert_eq!(link_cli(&home, &silo, Some(&bin), true, true), Some(bin.join("silo")));
        assert_eq!(std::fs::read_link(bin.join("silo")).unwrap(), stable);
        assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 1, "no temp link left");
        let _ = std::fs::remove_dir_all(base);
        // Links into a bundle (moved by updates) are relinked; others kept.
        assert!(should_link(&Existing::Missing));
        assert!(should_link(&Existing::Link("/Applications/Silo.app/Contents/Helpers/Silo Server.app/Contents/MacOS/silo".into())));
        assert!(should_link(&Existing::Link("/tmp/x/Silo Server.app/Contents/MacOS/silo".into())));
        assert!(!should_link(&Existing::Link("/opt/homebrew/bin/silo".into())));
        assert!(!should_link(&Existing::Other));
    }
}
