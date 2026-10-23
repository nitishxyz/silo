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

/// User-level agents, skills, hooks and `AGENTS.md`, this harness's own (not the TS otto's):
/// `$OTTO_SILO_CONFIG_DIR`, else `$XDG_CONFIG_HOME/otto-silo`, else `~/.config/otto-silo`.
#[must_use]
pub fn user_config() -> PathBuf {
    user_config_in(|name| std::env::var_os(name))
}
fn user_config_in(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> PathBuf {
    if let Some(dir) = var(&format!("{ENV_PREFIX}_CONFIG_DIR")).filter(|d| !d.is_empty()) {
        return dir.into();
    }
    let user_home = var("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    var("XDG_CONFIG_HOME").filter(|d| !d.is_empty()).map_or_else(|| user_home.join(".config"), PathBuf::from).join(BINARY)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn user_config_prefers_env_then_xdg_then_home() {
        fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
            move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| std::ffi::OsString::from(*v))
        }
        assert_eq!(user_config_in(env(&[("OTTO_SILO_CONFIG_DIR", "/c"), ("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")])), PathBuf::from("/c"));
        assert_eq!(user_config_in(env(&[("OTTO_SILO_CONFIG_DIR", ""), ("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")])), PathBuf::from("/x/otto-silo"));
        assert_eq!(user_config_in(env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/h")])), PathBuf::from("/h/.config/otto-silo"));
        assert_eq!(user_config_in(env(&[("HOME", "/h")])), PathBuf::from("/h/.config/otto-silo"));
    }
}
