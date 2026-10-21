//! Well-known local paths; the naming switch lives in this crate.

use std::path::{Path, PathBuf};

pub use crate::{BINARY, ENV_PREFIX, PROJECT_SHARED_DIR, PROJECT_STATE_DIR};

/// `$OTTO_SILO_HOME`, else macOS Application Support or XDG state home.
#[must_use]
pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os(format!("{ENV_PREFIX}_HOME")).filter(|d| !d.is_empty()) {
        return dir.into();
    }
    let user_home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    if cfg!(target_os = "macos") {
        return user_home.join("Library/Application Support").join(BINARY);
    }
    std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()).map_or_else(|| user_home.join(".local/state"), PathBuf::from).join(BINARY)
}

/// User-level agents, skills, hooks and `AGENTS.md`, shared with the TS otto:
/// `$OTTO_SILO_CONFIG_DIR`, else `$XDG_CONFIG_HOME/otto`, else `~/.config/otto`.
#[must_use]
pub fn user_config() -> PathBuf {
    if let Some(dir) = std::env::var_os(format!("{ENV_PREFIX}_CONFIG_DIR")).filter(|d| !d.is_empty()) {
        return dir.into();
    }
    let user_home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()).map_or_else(|| user_home.join(".config"), PathBuf::from).join("otto")
}

#[must_use]
pub fn socket(home: &Path) -> PathBuf {
    home.join("server.sock")
}
#[must_use]
pub fn db(home: &Path) -> PathBuf {
    home.join("otto.db")
}
#[must_use]
pub fn auth_dir(home: &Path) -> PathBuf {
    home.join("auth")
}
#[must_use]
pub fn log(home: &Path) -> PathBuf {
    home.join("server.log")
}
#[must_use]
pub fn runtime(home: &Path) -> PathBuf {
    home.join("runtime.json")
}
/// Held while a client starts the server.
#[must_use]
pub fn start_lock(home: &Path) -> PathBuf {
    home.join("start.lock")
}
/// Held for the server's lifetime.
#[must_use]
pub fn server_lock(home: &Path) -> PathBuf {
    home.join("server.lock")
}

/// Contents of `runtime.json`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct Runtime {
    pub pid: u32,
    pub socket: PathBuf,
    pub version: String,
    pub server_id: String,
    pub started_at: i64,
}
