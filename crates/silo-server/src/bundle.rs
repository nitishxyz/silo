//! Exposing the bundled `silo` CLI to shells spawned by an app-bundled server.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// `…/X.app/Contents/Resources/bin` for an executable at
/// `…/X.app/Contents/MacOS/<exe>`; `None` outside an app bundle.
#[must_use]
pub fn bin_dir(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let is_app = app.extension().is_some_and(|e| e == "app");
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && is_app).then(|| contents.join("Resources/bin"))
}

/// Environment for spawned shells: the bundle's bin dir on `PATH` when this
/// server runs from an app bundle (first as passed; shell startup files may
/// reorder it).
pub fn shell_env() -> &'static [(OsString, OsString)] {
    static ENV: OnceLock<Vec<(OsString, OsString)>> = OnceLock::new();
    ENV.get_or_init(|| {
        let Some(dir) = std::env::current_exe().and_then(|e| e.canonicalize()).ok().as_deref().and_then(bin_dir) else {
            return Vec::new();
        };
        let rest = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&rest))).unwrap_or(rest);
        vec![("PATH".into(), path)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_server_maps_to_resources_bin() {
        assert_eq!(
            bin_dir(Path::new("/Applications/Silo.app/Contents/MacOS/silo-server")),
            Some(PathBuf::from("/Applications/Silo.app/Contents/Resources/bin"))
        );
        assert_eq!(
            bin_dir(Path::new("/tmp/My Apps/FakeSilo.app/Contents/MacOS/silo-server")),
            Some(PathBuf::from("/tmp/My Apps/FakeSilo.app/Contents/Resources/bin"))
        );
    }

    #[test]
    fn unbundled_server_has_no_bin_dir() {
        for exe in [
            "/Users/me/dev/term/target/debug/silo",
            "/usr/local/bin/silo",
            "/Silo.app/MacOS/silo-server",
            "/x/Silo/Contents/MacOS/silo-server",
            "/x/Silo.app/Contents/Resources/silo-server",
            "silo",
            "/",
        ] {
            assert_eq!(bin_dir(Path::new(exe)), None, "{exe}");
        }
    }
}
