//! Well-known local paths shared by the server and clients. Every per-user
//! location follows this build's [`crate::CHANNEL`] (`silo_channel`).

use std::path::PathBuf;

pub use silo_channel::Home;

fn env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

const MACOS: bool = cfg!(target_os = "macos");

/// Server state directory: `$SILO_HOME`, else the channel's
/// `~/Library/Application Support/silo` (dev: `silo-dev`) on macOS and
/// `$XDG_STATE_HOME/silo` (default `~/.local/state/silo`; dev: `silo-dev`)
/// elsewhere. A `SILO_HOME` naming the other channel's default home is
/// refused (warned about once) and this channel's default used instead.
#[must_use]
pub fn home() -> PathBuf {
    let home = resolve_home();
    if let Some(warning) = home.warning("SILO_HOME", crate::CHANNEL) {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!("silo: {warning}");
            tracing::warn!("{warning}");
        });
    }
    home.path
}

/// [`home`] and the `SILO_HOME` it refused, if any.
#[must_use]
pub fn resolve_home() -> Home {
    resolve_home_with(crate::CHANNEL, &env, MACOS)
}

/// [`resolve_home`] for a `channel` build, environment `var` and OS rule.
#[must_use]
pub fn resolve_home_with(channel: crate::Channel, var: silo_channel::Var, macos: bool) -> Home {
    silo_channel::silo_home(channel, var, macos)
}

/// This channel's default home (what [`home`] is without `SILO_HOME`).
#[must_use]
pub fn default_home() -> PathBuf {
    silo_channel::default_silo_home(crate::CHANNEL, &env, MACOS)
}

/// The app's client cache: `<home>/cache` for a custom home (tests, UI
/// runs), else the channel's `~/Library/Caches/silo` (dev: `silo-dev`;
/// XDG `$XDG_CACHE_HOME/silo` elsewhere).
#[must_use]
pub fn app_cache() -> PathBuf {
    app_cache_in(&home(), crate::CHANNEL, &env, MACOS)
}

fn app_cache_in(home: &std::path::Path, channel: crate::Channel, var: silo_channel::Var, macos: bool) -> PathBuf {
    if silo_channel::same_dir(home, &silo_channel::default_silo_home(channel, var, macos)) {
        silo_channel::cache_dir(var, macos, channel.silo_dir())
    } else {
        home.join("cache")
    }
}

#[must_use]
pub fn socket(home: &std::path::Path) -> PathBuf {
    home.join("server.sock")
}

#[must_use]
pub fn runtime(home: &std::path::Path) -> PathBuf {
    home.join("runtime.json")
}

/// Held by a client while it spawns the server.
#[must_use]
pub fn start_lock(home: &std::path::Path) -> PathBuf {
    home.join("start.lock")
}

/// Held by the running server for its whole lifetime.
#[must_use]
pub fn server_lock(home: &std::path::Path) -> PathBuf {
    home.join("server.lock")
}

/// The server's persistent iroh identity directory.
#[must_use]
pub fn iroh_identity(home: &std::path::Path) -> PathBuf {
    home.join("state").join("iroh")
}

#[must_use]
pub fn log(home: &std::path::Path) -> PathBuf {
    home.join("server.log")
}

/// Clipboard tool shims (`pbpaste`, `xclip`, ...) put first on spawned
/// shells' `PATH`.
#[must_use]
pub fn shims(home: &std::path::Path) -> PathBuf {
    home.join("shims")
}

/// Uploaded files, one `<yyyymmdd-hhmmss>-<id>` directory per upload.
#[must_use]
pub fn drops(home: &std::path::Path) -> PathBuf {
    home.join("drops")
}

/// Search state (docs/FINDER.md §2): fff's frecency and query history
/// (LMDB), in the channel's home so dev and release builds keep their own.
#[must_use]
pub fn search(home: &std::path::Path) -> PathBuf {
    home.join("search")
}

/// Written by "Quit Silo" with the pid of the server it stops, so clients
/// can tell that shutdown from a crash or a restart.
#[must_use]
pub fn quit_marker(home: &std::path::Path) -> PathBuf {
    home.join("quit")
}

/// Record that the server with `pid` is being stopped to quit Silo.
///
/// # Errors
/// The marker cannot be written.
pub fn mark_quit(home: &std::path::Path, pid: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    std::fs::write(quit_marker(home), pid.to_string())
}

/// Whether "Quit Silo" stopped the server with `pid`.
#[must_use]
pub fn quit_marked(home: &std::path::Path, pid: u32) -> bool {
    std::fs::read_to_string(quit_marker(home)).is_ok_and(|s| s.trim().parse() == Ok(pid))
}

/// Remove the quit marker unless it names `keep`.
pub fn clear_quit(home: &std::path::Path, keep: Option<u32>) {
    if keep.is_none_or(|pid| !quit_marked(home, pid)) {
        let _ = std::fs::remove_file(quit_marker(home));
    }
}

/// The channel `runtime.json` in `home` records: `None` without a readable
/// file, [`crate::Channel::Release`] from a server that predates channels.
#[must_use]
pub fn runtime_channel(home: &std::path::Path) -> Option<crate::Channel> {
    #[derive(serde::Deserialize)]
    struct Channel {
        #[serde(default)]
        channel: crate::Channel,
    }
    serde_json::from_slice::<Channel>(&std::fs::read(runtime(home)).ok()?).ok().map(|c| c.channel)
}

/// `Err` when `runtime.json` in `home` names the other channel's server.
///
/// # Errors
/// [`crate::OtherChannel`].
pub fn check_runtime_channel(home: &std::path::Path) -> Result<(), crate::OtherChannel> {
    match runtime_channel(home) {
        Some(server) if server != crate::CHANNEL => Err(crate::OtherChannel { server }),
        _ => Ok(()),
    }
}

/// Contents of `runtime.json`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Runtime {
    pub pid: u32,
    pub socket: PathBuf,
    pub version: String,
    /// The server's [`crate::BUILD`].
    pub build: String,
    pub started_at: u64,
    /// The server's [`crate::CHANNEL`]; absent (release) from older builds.
    #[serde(default)]
    pub channel: crate::Channel,
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::Path};

    use super::*;
    use crate::Channel;

    #[test]
    fn app_cache_per_channel() {
        let var = |name: &str| (name == "HOME").then(|| OsString::from("/h"));
        let dev = Path::new("/h/Library/Application Support/silo-dev");
        assert_eq!(app_cache_in(dev, Channel::Dev, &var, true), Path::new("/h/Library/Caches/silo-dev"));
        let release = Path::new("/h/Library/Application Support/silo");
        assert_eq!(app_cache_in(release, Channel::Release, &var, true), Path::new("/h/Library/Caches/silo"));
        assert_eq!(app_cache_in(Path::new("/tmp/silo-ui"), Channel::Dev, &var, true), Path::new("/tmp/silo-ui/cache"));
        assert_eq!(app_cache_in(Path::new("/h/.local/state/silo-dev"), Channel::Dev, &var, false), Path::new("/h/.cache/silo-dev"));
    }

    #[test]
    fn runtime_channel_reads_any_runtime_json() {
        let home = std::env::temp_dir().join(format!("silo-proto-runtime-channel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(runtime_channel(&home), None);
        assert_eq!(check_runtime_channel(&home), Ok(()));
        // A server from before channels: release.
        std::fs::write(runtime(&home), r#"{"pid":1,"socket":"/s","version":"0.1.0","started_at":0}"#).unwrap();
        assert_eq!(runtime_channel(&home), Some(Channel::Release));
        std::fs::write(runtime(&home), format!(r#"{{"pid":1,"channel":"{}"}}"#, crate::CHANNEL.other())).unwrap();
        assert_eq!(runtime_channel(&home), Some(crate::CHANNEL.other()));
        let refused = check_runtime_channel(&home).unwrap_err();
        assert_eq!(refused.server, crate::CHANNEL.other());
        assert!(refused.to_string().contains(&format!("a {} build never takes over", crate::CHANNEL)), "{refused}");
        std::fs::write(runtime(&home), format!(r#"{{"channel":"{}"}}"#, crate::CHANNEL)).unwrap();
        assert_eq!(check_runtime_channel(&home), Ok(()));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn runtime_without_channel_is_release() {
        let old = r#"{"pid":1,"socket":"/s","version":"0.1.0","build":"b","started_at":0}"#;
        assert_eq!(serde_json::from_str::<Runtime>(old).unwrap().channel, Channel::Release);
    }
}
