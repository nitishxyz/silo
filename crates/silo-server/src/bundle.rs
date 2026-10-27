//! Exposing `silo` to shells spawned by a server run from its runtime
//! directory (`<SILO_HOME>/runtime/Silo Server.app/Contents/MacOS/silo`,
//! installed by the app; `<SILO_HOME>/runtime/silo` before the server was a
//! bundle),
//! through the stable `<SILO_HOME>/bin/silo` link that survives upgrades,
//! and the clipboard tool shims in `<SILO_HOME>/shims`.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Tools shimmed to `silo clipboard shim <tool>`.
pub const SHIMMED: &[&str] = &["pbpaste", "pbcopy", "xclip", "xsel", "wl-paste", "wl-copy"];

/// The server bundle in the runtime directory and its executable (as
/// `silo_client::runtime` installs them).
const RUNTIME_SERVER: &str = "Silo Server.app/Contents/MacOS/silo";

/// `home`'s runtime directory when `exe` is the app's copy of the server in
/// it: the server bundle's `silo`, or the bare `silo` from before it.
#[must_use]
pub fn runtime_dir(exe: &Path, home: &Path) -> Option<PathBuf> {
    let dir = home.join("runtime");
    (exe == dir.join(RUNTIME_SERVER) || exe == dir.join("silo")).then_some(dir)
}

/// The `silo` the shims run: the stable `<home>/bin/silo` for a
/// runtime-directory server, else this executable.
fn silo_exe(home: &Path) -> Option<PathBuf> {
    let exe = std::env::current_exe().and_then(|e| e.canonicalize()).ok()?;
    let home = home.canonicalize().unwrap_or_else(|_| home.to_owned());
    Some(match runtime_dir(&exe, &home) {
        Some(_) => home.join("bin").join("silo"),
        None => exe,
    })
}

/// A shim script: `exec '<silo>' clipboard shim <tool> "$@"`.
fn shim_script(silo: &Path, tool: &str) -> String {
    let quoted = silo.to_string_lossy().replace('\'', r"'\''");
    format!("#!/bin/sh\nexec '{quoted}' clipboard shim {tool} \"$@\"\n")
}

/// Write the clipboard shims into `<home>/shims` (only files that differ).
///
/// # Errors
/// The directory or a shim cannot be written.
pub fn install_shims(home: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let Some(silo) = silo_exe(home) else { return Ok(()) };
    let dir = silo_proto::paths::shims(home);
    std::fs::create_dir_all(&dir)?;
    for tool in SHIMMED {
        let path = dir.join(tool);
        let script = shim_script(&silo, tool);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(script.as_str()) {
            let tmp = dir.join(format!(".{tool}.tmp"));
            std::fs::write(&tmp, &script)?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
            std::fs::rename(&tmp, &path)?;
        }
    }
    Ok(())
}

/// The canonical `SILO_HOME` and the directories that lead a spawned
/// shell's `PATH`: `<SILO_HOME>/shims` and, for a runtime-directory server,
/// `<SILO_HOME>/bin`.
pub fn path_lead() -> &'static (PathBuf, Vec<PathBuf>) {
    static LEAD: OnceLock<(PathBuf, Vec<PathBuf>)> = OnceLock::new();
    LEAD.get_or_init(|| {
        let home = silo_proto::paths::home();
        let home = home.canonicalize().unwrap_or(home);
        let exe = std::env::current_exe().and_then(|e| e.canonicalize()).ok();
        let bin = exe.and_then(|exe| runtime_dir(&exe, &home)).map(|_| home.join("bin"));
        let lead = std::iter::once(silo_proto::paths::shims(&home)).chain(bin).collect();
        (home, lead)
    })
}

/// Environment for a spawned program of block `block_id`: `SILO_BLOCK`, and
/// `PATH` led by [`path_lead`] (login shells restore that order after their
/// startup files through [`crate::shell`]).
pub fn shell_env(block_id: &str) -> Vec<(OsString, OsString)> {
    static PATH: OnceLock<OsString> = OnceLock::new();
    let path = PATH.get_or_init(|| {
        let rest = std::env::var_os("PATH").unwrap_or_default();
        std::env::join_paths(path_lead().1.iter().cloned().chain(std::env::split_paths(&rest))).unwrap_or(rest)
    });
    vec![("PATH".into(), path.clone()), ("SILO_BLOCK".into(), block_id.into())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shims_exec_silo_and_are_rewritten_only_when_changed() {
        let script = shim_script(Path::new("/a b/it's/silo"), "xclip");
        assert_eq!(script, "#!/bin/sh\nexec '/a b/it'\\''s/silo' clipboard shim xclip \"$@\"\n");
        let home = std::env::temp_dir().join(format!("silo-shims-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        install_shims(&home).unwrap();
        let dir = silo_proto::paths::shims(&home);
        for tool in SHIMMED {
            assert!(std::fs::read_to_string(dir.join(tool)).unwrap().contains(&format!("clipboard shim {tool} ")));
        }
        let before = std::fs::metadata(dir.join("xclip")).unwrap().modified().unwrap();
        install_shims(&home).unwrap();
        assert_eq!(std::fs::metadata(dir.join("xclip")).unwrap().modified().unwrap(), before);
        let env = shell_env("b1");
        assert!(env.contains(&("SILO_BLOCK".into(), "b1".into())));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn runtime_server_is_recognized() {
        let home = Path::new("/Users/bat/Library/Application Support/silo");
        assert_eq!(runtime_dir(&home.join("runtime/silo"), home), Some(home.join("runtime")));
        assert_eq!(runtime_dir(&home.join("runtime/Silo Server.app/Contents/MacOS/silo"), home), Some(home.join("runtime")));
    }

    #[test]
    fn other_servers_add_nothing() {
        let home = Path::new("/h");
        for exe in [
            "/h/runtime/old/silo",
            "/h/runtime/silo-menubar",
            "/h/runtime/Silo Server.app/Contents/MacOS/silo-menubar",
            "/h/runtime/Other.app/Contents/MacOS/silo",
            "/Applications/Silo.app/Contents/Helpers/Silo Server.app/Contents/MacOS/silo",
            "/h/abc/silo",
            "/Applications/Silo.app/Contents/MacOS/silo-server",
            "/Users/me/dev/term/target/debug/silo",
            "silo",
            "/",
        ] {
            assert_eq!(runtime_dir(Path::new(exe), home), None, "{exe}");
        }
    }
}
