//! Exposing `silo` to shells spawned by a server run from its runtime
//! directory (`<SILO_HOME>/runtime/<build>/silo`, installed by the app),
//! through the stable `<SILO_HOME>/bin/silo` link that survives upgrades.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// `exe`'s directory when it is `home`'s runtime directory for `build`.
#[must_use]
pub fn runtime_dir(exe: &Path, home: &Path, build: &str) -> Option<PathBuf> {
    let dir = exe.parent()?;
    (exe.file_name()? == "silo" && dir == home.join("runtime").join(build)).then(|| dir.to_owned())
}

/// Environment for spawned shells of a runtime-directory server:
/// `<SILO_HOME>/bin` first on `PATH` (as passed; shell startup files may
/// reorder it).
pub fn shell_env() -> &'static [(OsString, OsString)] {
    static ENV: OnceLock<Vec<(OsString, OsString)>> = OnceLock::new();
    ENV.get_or_init(|| {
        let home = silo_proto::paths::home();
        let home = home.canonicalize().unwrap_or(home);
        let exe = std::env::current_exe().and_then(|e| e.canonicalize()).ok();
        if exe.and_then(|exe| runtime_dir(&exe, &home, silo_proto::BUILD)).is_none() {
            return Vec::new();
        }
        let dir = home.join("bin");
        let rest = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&rest))).unwrap_or(rest);
        vec![("PATH".into(), path)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_server_is_recognized() {
        let home = Path::new("/Users/bat/Library/Application Support/silo");
        assert_eq!(runtime_dir(&home.join("runtime/abc/silo"), home, "abc"), Some(home.join("runtime/abc")));
    }

    #[test]
    fn other_servers_add_nothing() {
        let home = Path::new("/h");
        for exe in [
            "/h/runtime/old/silo",
            "/h/runtime/abc/silo-menubar",
            "/h/abc/silo",
            "/Applications/Silo.app/Contents/MacOS/silo-server",
            "/Users/me/dev/term/target/debug/silo",
            "silo",
            "/",
        ] {
            assert_eq!(runtime_dir(Path::new(exe), home, "abc"), None, "{exe}");
        }
    }
}
