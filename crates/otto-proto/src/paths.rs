//! Well-known local paths; the naming switch lives in this crate.

use std::path::{Path, PathBuf};

pub use crate::{BINARY, ENV_PREFIX, PROJECT_SHARED_DIR, PROJECT_STATE_DIR};

pub use silo_channel::{CURRENT as CHANNEL, Channel, Home};

use silo_channel::Var;

fn env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

const MACOS: bool = cfg!(target_os = "macos");

/// otto's state directory name for `channel`: `otto-silo`, dev `otto-silo-dev`.
#[must_use]
pub fn dir_name(channel: Channel) -> String {
    format!("{BINARY}{}", channel.suffix())
}

/// One daemon per Silo server: `$OTTO_SILO_HOME`; else, serving this
/// channel's default Silo home ([`silo_home`]), macOS Application Support or
/// the XDG state home `otto-silo` (dev: `otto-silo-dev`); else
/// `<silo home>/otto`. An `OTTO_SILO_HOME` naming the other channel's default
/// otto home is refused (warned about once), as Silo refuses its `SILO_HOME`.
#[must_use]
pub fn home() -> PathBuf {
    let home = resolve_home();
    if let Some(warning) = home.warning(&format!("{ENV_PREFIX}_HOME"), CHANNEL) {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| eprintln!("{BINARY}: {warning}"));
    }
    home.path
}

/// [`home`] and the `OTTO_SILO_HOME` it refused, if any.
#[must_use]
pub fn resolve_home() -> Home {
    resolve_home_with(CHANNEL, &env, MACOS)
}

/// [`resolve_home`] for a `channel` build, environment `var` and OS rule.
#[must_use]
pub fn resolve_home_with(channel: Channel, var: Var, macos: bool) -> Home {
    let own = silo_channel::state_dir(var, macos, &dir_name(channel));
    let other = silo_channel::state_dir(var, macos, &dir_name(channel.other()));
    let given = silo_channel::choose(silo_channel::set(var, &format!("{ENV_PREFIX}_HOME")), own.clone(), &other);
    if given.refused.is_none() && given.path != own {
        return given;
    }
    let silo = silo_channel::silo_home(channel, var, macos).path;
    if !silo_channel::same_dir(&silo, &silo_channel::default_silo_home(channel, var, macos)) {
        return Home { path: silo.join("otto"), refused: given.refused };
    }
    given
}

/// The Silo home this process serves, as `silo_proto::paths::home` resolves it.
#[must_use]
pub fn silo_home() -> PathBuf {
    silo_channel::silo_home(CHANNEL, &env, MACOS).path
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

    fn home_in(channel: Channel, env: Vars, macos: bool) -> Home {
        resolve_home_with(channel, &vars(env), macos)
    }

    #[test]
    fn home_is_one_per_silo_home() {
        use Channel::{Dev, Release};
        let mac = "/h/Library/Application Support/otto-silo";
        let mac_dev = "/h/Library/Application Support/otto-silo-dev";
        let table: &[(Channel, Vars, bool, &str)] = &[
            (Dev, &[("OTTO_SILO_HOME", "/o"), ("SILO_HOME", "/tmp/silo-ui"), ("HOME", "/h")], true, "/o"),
            (Dev, &[("OTTO_SILO_HOME", ""), ("SILO_HOME", "/tmp/silo-ui"), ("HOME", "/h")], true, "/tmp/silo-ui/otto"),
            (Release, &[("HOME", "/h")], true, mac),
            (Dev, &[("HOME", "/h")], true, mac_dev),
            (Release, &[("SILO_HOME", ""), ("HOME", "/h")], true, mac),
            (Release, &[("SILO_HOME", "/h/Library/Application Support/silo"), ("HOME", "/h")], true, mac),
            (Dev, &[("SILO_HOME", "/h/Library/Application Support/silo-dev"), ("HOME", "/h")], true, mac_dev),
            (Release, &[("SILO_HOME", "/tmp/silo-ui"), ("HOME", "/h")], true, "/tmp/silo-ui/otto"),
            (Dev, &[("SILO_HOME", "/tmp/silo-ui"), ("HOME", "/h")], true, "/tmp/silo-ui/otto"),
            (Release, &[("HOME", "/h")], false, "/h/.local/state/otto-silo"),
            (Dev, &[("HOME", "/h")], false, "/h/.local/state/otto-silo-dev"),
            (Release, &[("XDG_STATE_HOME", "/x"), ("SILO_HOME", "/x/silo"), ("HOME", "/h")], false, "/x/otto-silo"),
            (Dev, &[("XDG_STATE_HOME", "/x"), ("SILO_HOME", "/x/silo-dev"), ("HOME", "/h")], false, "/x/otto-silo-dev"),
            (Release, &[("XDG_STATE_HOME", "/x"), ("SILO_HOME", "/h/.local/state/silo"), ("HOME", "/h")], false, "/h/.local/state/silo/otto"),
            // The other channel's Silo home is refused, so otto takes this channel's default.
            (Dev, &[("SILO_HOME", "/h/Library/Application Support/silo"), ("HOME", "/h")], true, mac_dev),
            (Release, &[("SILO_HOME", "/h/.local/state/silo-dev"), ("HOME", "/h")], false, "/h/.local/state/otto-silo"),
        ];
        for (channel, env, macos, want) in table {
            let got = home_in(*channel, env, *macos);
            assert_eq!(got.path, PathBuf::from(want), "{channel} {env:?} macos={macos}");
            assert_eq!(got.refused, None, "{channel} {env:?} macos={macos}");
        }
    }

    #[test]
    fn the_other_channels_otto_home_is_refused() {
        let dev = home_in(Channel::Dev, &[("OTTO_SILO_HOME", "/h/Library/Application Support/otto-silo"), ("HOME", "/h")], true);
        assert_eq!(dev.path, PathBuf::from("/h/Library/Application Support/otto-silo-dev"));
        assert_eq!(dev.refused, Some(PathBuf::from("/h/Library/Application Support/otto-silo")));
        assert!(
            dev.warning("OTTO_SILO_HOME", Channel::Dev)
                .unwrap()
                .starts_with("OTTO_SILO_HOME=/h/Library/Application Support/otto-silo is the release build's home")
        );
        // Refused, then the custom Silo home rule still applies.
        let dev = home_in(Channel::Dev, &[("OTTO_SILO_HOME", "/h/.local/state/otto-silo"), ("SILO_HOME", "/tmp/s"), ("HOME", "/h")], false);
        assert_eq!((dev.path, dev.refused), (PathBuf::from("/tmp/s/otto"), Some(PathBuf::from("/h/.local/state/otto-silo"))));
        let release = home_in(Channel::Release, &[("OTTO_SILO_HOME", "/x/otto-silo-dev"), ("XDG_STATE_HOME", "/x"), ("HOME", "/h")], false);
        assert_eq!((release.path, release.refused), (PathBuf::from("/x/otto-silo"), Some(PathBuf::from("/x/otto-silo-dev"))));
        // Its own default is fine.
        let own = home_in(Channel::Dev, &[("OTTO_SILO_HOME", "/h/Library/Application Support/otto-silo-dev"), ("HOME", "/h")], true);
        assert_eq!((own.path, own.refused), (PathBuf::from("/h/Library/Application Support/otto-silo-dev"), None));
    }

    #[test]
    fn dir_names() {
        assert_eq!((dir_name(Channel::Release), dir_name(Channel::Dev)), ("otto-silo".to_owned(), "otto-silo-dev".to_owned()));
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
