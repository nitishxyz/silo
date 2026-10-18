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
    std::env::var_os("XDG_STATE_HOME")
        .map_or_else(|| user_home.join(".local/state"), PathBuf::from)
        .join("silo")
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

#[must_use]
pub fn log(home: &std::path::Path) -> PathBuf {
    home.join("server.log")
}

/// Contents of `runtime.json`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Runtime {
    pub pid: u32,
    pub socket: PathBuf,
    pub version: String,
    pub ghostty_rev: String,
    pub started_at: u64,
}
