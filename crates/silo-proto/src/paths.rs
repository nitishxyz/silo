//! Well-known local paths shared by the server and clients.

use std::path::PathBuf;

/// Server state directory: `$SILO_HOME`, else
/// `~/Library/Application Support/silo` on macOS and
/// `$XDG_STATE_HOME/silo` (default `~/.local/state/silo`) elsewhere.
#[must_use]
pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("SILO_HOME").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    let user_home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    if cfg!(target_os = "macos") {
        return user_home.join("Library/Application Support/silo");
    }
    std::env::var_os("XDG_STATE_HOME").map_or_else(|| user_home.join(".local/state"), PathBuf::from).join("silo")
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

/// Contents of `runtime.json`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Runtime {
    pub pid: u32,
    pub socket: PathBuf,
    pub version: String,
    /// The server's [`crate::BUILD`].
    pub build: String,
    pub started_at: u64,
}
