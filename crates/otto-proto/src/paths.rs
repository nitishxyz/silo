//! Well-known local paths; the naming switch lives in this crate.

use std::path::{Path, PathBuf};

pub use crate::{BINARY, ENV_PREFIX, PROJECT_SHARED_DIR, PROJECT_STATE_DIR};

/// One daemon per Silo server: `$OTTO_SILO_HOME`; else, serving Silo's default
/// home ([`silo_home`]), macOS Application Support or the XDG state home; else
/// `<silo home>/otto`.
#[must_use]
pub fn home() -> PathBuf {
    home_in(|name| std::env::var_os(name), cfg!(target_os = "macos"))
}
fn home_in(var: impl Fn(&str) -> Option<std::ffi::OsString>, macos: bool) -> PathBuf {
    if let Some(dir) = var(&format!("{ENV_PREFIX}_HOME")).filter(|d| !d.is_empty()) {
        return dir.into();
    }
    let silo = silo_home_in(&var, macos);
    if silo != silo_home_in(|name| var(name).filter(|_| name != "SILO_HOME"), macos) {
        return silo.join("otto");
    }
    let user_home = var("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    if macos {
        return user_home.join("Library/Application Support").join(BINARY);
    }
    var("XDG_STATE_HOME").filter(|d| !d.is_empty()).map_or_else(|| user_home.join(".local/state"), PathBuf::from).join(BINARY)
}

/// The Silo home this process serves, as `silo_proto::paths::home` resolves it:
/// `$SILO_HOME`, else macOS Application Support or the XDG state home.
#[must_use]
pub fn silo_home() -> PathBuf {
    silo_home_in(|name| std::env::var_os(name), cfg!(target_os = "macos"))
}
fn silo_home_in(var: impl Fn(&str) -> Option<std::ffi::OsString>, macos: bool) -> PathBuf {
    if let Some(dir) = var("SILO_HOME").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    let user_home = var("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    if macos {
        return user_home.join("Library/Application Support/silo");
    }
    var("XDG_STATE_HOME").map_or_else(|| user_home.join(".local/state"), PathBuf::from).join("silo")
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
    /// The Silo home the daemon serves ([`silo_home`]).
    #[serde(default)]
    pub silo_home: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    type Vars = &'static [(&'static str, &'static str)];
    fn vars(pairs: Vars) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| std::ffi::OsString::from(*v))
    }

    #[test]
    fn home_is_one_per_silo_home() {
        let mac = "/h/Library/Application Support/otto-silo";
        let table: &[(Vars, bool, &str)] = &[
            (&[("OTTO_SILO_HOME", "/o"), ("SILO_HOME", "/tmp/silo-dev"), ("HOME", "/h")], true, "/o"),
            (&[("OTTO_SILO_HOME", ""), ("SILO_HOME", "/tmp/silo-dev"), ("HOME", "/h")], true, "/tmp/silo-dev/otto"),
            (&[("HOME", "/h")], true, mac),
            (&[("SILO_HOME", ""), ("HOME", "/h")], true, mac),
            (&[("SILO_HOME", "/h/Library/Application Support/silo"), ("HOME", "/h")], true, mac),
            (&[("SILO_HOME", "/tmp/silo-dev"), ("HOME", "/h")], true, "/tmp/silo-dev/otto"),
            (&[("HOME", "/h")], false, "/h/.local/state/otto-silo"),
            (&[("XDG_STATE_HOME", "/x"), ("SILO_HOME", "/x/silo"), ("HOME", "/h")], false, "/x/otto-silo"),
            (&[("XDG_STATE_HOME", "/x"), ("SILO_HOME", "/h/.local/state/silo"), ("HOME", "/h")], false, "/h/.local/state/silo/otto"),
        ];
        for (env, macos, want) in table {
            assert_eq!(home_in(vars(env), *macos), PathBuf::from(want), "{env:?} macos={macos}");
        }
    }

    #[test]
    fn silo_home_matches_silo() {
        assert_eq!(silo_home_in(vars(&[("SILO_HOME", "/s"), ("HOME", "/h")]), true), PathBuf::from("/s"));
        assert_eq!(silo_home_in(vars(&[("HOME", "/h")]), true), PathBuf::from("/h/Library/Application Support/silo"));
        assert_eq!(silo_home_in(vars(&[("XDG_STATE_HOME", "/x"), ("HOME", "/h")]), false), PathBuf::from("/x/silo"));
    }

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
