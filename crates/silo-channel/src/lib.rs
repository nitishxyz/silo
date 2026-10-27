//! The build's channel, [`CURRENT`]: a release build (the signed Silo.app,
//! the published `silo`) or a dev build (everything else: `cargo build`,
//! `cargo test`, scripts/dev-bundle.sh). It is fixed at compile time by
//! `SILO_CHANNEL` (see `build.rs`), never guessed at run time, and every
//! name two channels could otherwise share follows it: the state homes
//! ([`silo_home`], and otto's in `otto_proto::paths`), caches, the service
//! manager's job names and the app's bundle id. A dev build and an installed
//! Silo therefore never share state, a server, otto or sign-ins.
//!
//! Std, serde and schemars only: re-exported by `silo-proto` and by the
//! wasm-safe `otto-proto`.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A build's channel. On the wire (hellos, `runtime.json`, the live
/// upgrade's transfer) a missing value is [`Channel::Release`]: every build
/// from before channels existed is a release build or behaves like one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Release,
    Dev,
}

/// This build's channel (`SILO_CHANNEL` at compile time).
pub const CURRENT: Channel = match env!("SILO_BUILD_CHANNEL").as_bytes() {
    b"release" => Channel::Release,
    _ => Channel::Dev,
};

impl Channel {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Dev => "dev",
        }
    }

    /// The other channel.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Release => Self::Dev,
            Self::Dev => Self::Release,
        }
    }

    /// Appended to per-user directory and job names: `""` or `"-dev"`.
    #[must_use]
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Release => "",
            Self::Dev => "-dev",
        }
    }

    /// Silo's state and cache directory name: `silo`, `silo-dev`.
    #[must_use]
    pub const fn silo_dir(self) -> &'static str {
        match self {
            Self::Release => "silo",
            Self::Dev => "silo-dev",
        }
    }

    /// The macOS app's bundle id (scripts/bundle-macos.sh,
    /// scripts/dev-bundle.sh).
    #[must_use]
    pub const fn bundle_id(self) -> &'static str {
        match self {
            Self::Release => "xyz.nitish.silo",
            Self::Dev => "xyz.nitish.silo.dev",
        }
    }

    /// The app's name in Silo's own UI (the menu-bar agent): `Silo`, `Silo Dev`.
    #[must_use]
    pub const fn app_name(self) -> &'static str {
        match self {
            Self::Release => "Silo",
            Self::Dev => "Silo Dev",
        }
    }

    /// The server's helper app bundle id (`Silo.app/Contents/Helpers/Silo
    /// Server.app`): the identity macOS privacy prompts and System Settings
    /// show for programs run in Silo's terminals.
    #[must_use]
    pub const fn server_bundle_id(self) -> &'static str {
        match self {
            Self::Release => "xyz.nitish.silo.server",
            Self::Dev => "xyz.nitish.silo.dev.server",
        }
    }

    /// The server's helper app name (`<name>.app` in the app's
    /// `Contents/Helpers`, its `CFBundleName`): `Silo Server`, `SiloDev Server`.
    #[must_use]
    pub const fn server_name(self) -> &'static str {
        match self {
            Self::Release => "Silo Server",
            Self::Dev => "SiloDev Server",
        }
    }

    /// The launchd job running the server (`silo up`).
    #[must_use]
    pub const fn launchd_label(self) -> &'static str {
        match self {
            Self::Release => "dev.silo.server",
            Self::Dev => "dev.silo.server.dev",
        }
    }

    /// The systemd user unit running the server (`silo up`).
    #[must_use]
    pub const fn systemd_unit(self) -> &'static str {
        match self {
            Self::Release => "silo.service",
            Self::Dev => "silo-dev.service",
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// An environment lookup (`std::env::var_os`, or a table in tests).
pub type Var<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// `name` in `var`, unless unset or empty.
#[must_use]
pub fn set(var: Var, name: &str) -> Option<OsString> {
    var(name).filter(|v| !v.is_empty())
}

fn user_home(var: Var) -> PathBuf {
    set(var, "HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
}

/// The per-user state directory `name`: `~/Library/Application Support/<name>`
/// on macOS, `$XDG_STATE_HOME/<name>` (default `~/.local/state/<name>`)
/// elsewhere.
#[must_use]
pub fn state_dir(var: Var, macos: bool, name: &str) -> PathBuf {
    if macos {
        return user_home(var).join("Library/Application Support").join(name);
    }
    set(var, "XDG_STATE_HOME").map_or_else(|| user_home(var).join(".local/state"), PathBuf::from).join(name)
}

/// The per-user cache directory `name`: `~/Library/Caches/<name>` on macOS,
/// `$XDG_CACHE_HOME/<name>` (default `~/.cache/<name>`) elsewhere.
#[must_use]
pub fn cache_dir(var: Var, macos: bool, name: &str) -> PathBuf {
    if macos {
        return user_home(var).join("Library/Caches").join(name);
    }
    set(var, "XDG_CACHE_HOME").map_or_else(|| user_home(var).join(".cache"), PathBuf::from).join(name)
}

/// Whether `a` and `b` name one directory: equal paths (ignoring trailing
/// and doubled separators), or one directory once symlinks are resolved.
#[must_use]
pub fn same_dir(a: &Path, b: &Path) -> bool {
    a.components().eq(b.components()) || std::fs::canonicalize(a).ok().zip(std::fs::canonicalize(b).ok()).is_some_and(|(a, b)| a == b)
}

/// A resolved home and the override it refused, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    pub path: PathBuf,
    /// The override (`SILO_HOME`, `OTTO_SILO_HOME`) naming the other
    /// channel's default home, which this build will not use.
    pub refused: Option<PathBuf>,
}

impl Home {
    /// The warning for [`Home::refused`], naming the variable `var`.
    #[must_use]
    pub fn warning(&self, var: &str, channel: Channel) -> Option<String> {
        let refused = self.refused.as_ref()?;
        Some(format!(
            "{var}={} is the {} build's home; this {} build never uses it and uses {} instead",
            refused.display(),
            channel.other(),
            channel,
            self.path.display()
        ))
    }
}

/// The override `given`, else `own`; an override naming `other` (the other
/// channel's default) is refused.
#[must_use]
pub fn choose(given: Option<OsString>, own: PathBuf, other: &Path) -> Home {
    match given.map(PathBuf::from) {
        Some(dir) if same_dir(&dir, other) => Home { path: own, refused: Some(dir) },
        Some(dir) => Home { path: dir, refused: None },
        None => Home { path: own, refused: None },
    }
}

/// `channel`'s default Silo home: [`state_dir`] `silo` or `silo-dev`.
#[must_use]
pub fn default_silo_home(channel: Channel, var: Var, macos: bool) -> PathBuf {
    state_dir(var, macos, channel.silo_dir())
}

/// The Silo home of a `channel` build: `$SILO_HOME`, unless it names the
/// other channel's default home; else [`default_silo_home`].
#[must_use]
pub fn silo_home(channel: Channel, var: Var, macos: bool) -> Home {
    let other = default_silo_home(channel.other(), var, macos);
    choose(set(var, "SILO_HOME"), default_silo_home(channel, var, macos), &other)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Vars = &'static [(&'static str, &'static str)];
    fn vars(pairs: Vars) -> impl Fn(&str) -> Option<OsString> {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(*v))
    }

    #[test]
    fn current_is_dev_unless_built_for_release() {
        // `cargo test` builds without SILO_CHANNEL unless a release script
        // runs it; either way the constant matches the build script.
        let want = if option_env!("SILO_CHANNEL") == Some("release") { Channel::Release } else { Channel::Dev };
        assert_eq!(CURRENT, want);
    }

    #[test]
    fn names_per_channel() {
        use Channel::{Dev, Release};
        assert_eq!((Release.silo_dir(), Dev.silo_dir()), ("silo", "silo-dev"));
        assert_eq!((Release.bundle_id(), Dev.bundle_id()), ("xyz.nitish.silo", "xyz.nitish.silo.dev"));
        assert_eq!((Release.server_bundle_id(), Dev.server_bundle_id()), ("xyz.nitish.silo.server", "xyz.nitish.silo.dev.server"));
        assert_eq!((Release.server_name(), Dev.server_name()), ("Silo Server", "SiloDev Server"));
        for channel in [Release, Dev] {
            assert_eq!(channel.server_bundle_id(), format!("{}.server", channel.bundle_id()));
        }
        assert_eq!((Release.launchd_label(), Dev.launchd_label()), ("dev.silo.server", "dev.silo.server.dev"));
        assert_eq!((Release.systemd_unit(), Dev.systemd_unit()), ("silo.service", "silo-dev.service"));
        assert_eq!((Release.suffix(), Dev.suffix()), ("", "-dev"));
        assert_eq!(Release.other(), Dev);
    }

    #[test]
    fn wire_form_and_old_peer_default() {
        assert_eq!(serde_json::to_string(&Channel::Dev).unwrap(), "\"dev\"");
        assert_eq!(serde_json::from_str::<Channel>("\"release\"").unwrap(), Channel::Release);
        assert_eq!(Channel::default(), Channel::Release, "a peer without a channel is a release build");
    }

    #[test]
    fn silo_home_per_channel_and_os() {
        use Channel::{Dev, Release};
        let table: &[(Channel, Vars, bool, &str, Option<&str>)] = &[
            // Defaults, both OS rules.
            (Release, &[("HOME", "/h")], true, "/h/Library/Application Support/silo", None),
            (Dev, &[("HOME", "/h")], true, "/h/Library/Application Support/silo-dev", None),
            (Release, &[("HOME", "/h")], false, "/h/.local/state/silo", None),
            (Dev, &[("HOME", "/h")], false, "/h/.local/state/silo-dev", None),
            (Dev, &[("HOME", "/h"), ("XDG_STATE_HOME", "/x")], false, "/x/silo-dev", None),
            (Dev, &[("HOME", "/h"), ("XDG_STATE_HOME", "")], false, "/h/.local/state/silo-dev", None),
            (Dev, &[("HOME", "/h"), ("SILO_HOME", "")], true, "/h/Library/Application Support/silo-dev", None),
            // Overrides.
            (Dev, &[("HOME", "/h"), ("SILO_HOME", "/tmp/silo-ui")], true, "/tmp/silo-ui", None),
            (Release, &[("HOME", "/h"), ("SILO_HOME", "/tmp/silo-ui")], false, "/tmp/silo-ui", None),
            (Dev, &[("HOME", "/h"), ("SILO_HOME", "/h/Library/Application Support/silo-dev")], true, "/h/Library/Application Support/silo-dev", None),
            (Release, &[("HOME", "/h"), ("SILO_HOME", "/h/Library/Application Support/silo")], true, "/h/Library/Application Support/silo", None),
            // Cross-channel: refused.
            (
                Dev,
                &[("HOME", "/h"), ("SILO_HOME", "/h/Library/Application Support/silo")],
                true,
                "/h/Library/Application Support/silo-dev",
                Some("/h/Library/Application Support/silo"),
            ),
            (
                Dev,
                &[("HOME", "/h"), ("SILO_HOME", "/h/Library/Application Support/silo/")],
                true,
                "/h/Library/Application Support/silo-dev",
                Some("/h/Library/Application Support/silo/"),
            ),
            (
                Release,
                &[("HOME", "/h"), ("SILO_HOME", "/h/Library/Application Support/silo-dev")],
                true,
                "/h/Library/Application Support/silo",
                Some("/h/Library/Application Support/silo-dev"),
            ),
            (Dev, &[("HOME", "/h"), ("SILO_HOME", "/h/.local/state/silo")], false, "/h/.local/state/silo-dev", Some("/h/.local/state/silo")),
            (Release, &[("HOME", "/h"), ("XDG_STATE_HOME", "/x"), ("SILO_HOME", "/x/silo-dev")], false, "/x/silo", Some("/x/silo-dev")),
            // Another OS's rule names no channel's home here.
            (Dev, &[("HOME", "/h"), ("SILO_HOME", "/h/.local/state/silo")], true, "/h/.local/state/silo", None),
        ];
        for (channel, env, macos, want, refused) in table {
            let got = silo_home(*channel, &vars(env), *macos);
            assert_eq!(got.path, PathBuf::from(want), "{channel} {env:?} macos={macos}");
            assert_eq!(got.refused, refused.map(PathBuf::from), "{channel} {env:?} macos={macos}");
        }
    }

    #[test]
    fn refusal_is_explained() {
        let home = silo_home(Channel::Dev, &vars(&[("HOME", "/h"), ("SILO_HOME", "/h/Library/Application Support/silo")]), true);
        let warning = home.warning("SILO_HOME", Channel::Dev).unwrap();
        assert!(warning.contains("release build's home") && warning.ends_with("uses /h/Library/Application Support/silo-dev instead"), "{warning}");
        assert_eq!(silo_home(Channel::Dev, &vars(&[("HOME", "/h")]), true).warning("SILO_HOME", Channel::Dev), None);
    }

    #[test]
    fn same_dir_follows_symlinks() {
        let root = std::env::temp_dir().join(format!("silo-channel-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert!(same_dir(&root.join("link"), &root.join("real")));
        assert!(same_dir(&root.join("real/"), &root.join("real")));
        assert!(!same_dir(&root.join("real"), &root.join("other")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cache_dir_per_os() {
        assert_eq!(cache_dir(&vars(&[("HOME", "/h")]), true, "silo-dev"), PathBuf::from("/h/Library/Caches/silo-dev"));
        assert_eq!(cache_dir(&vars(&[("HOME", "/h")]), false, "silo"), PathBuf::from("/h/.cache/silo"));
        assert_eq!(cache_dir(&vars(&[("HOME", "/h"), ("XDG_CACHE_HOME", "/c")]), false, "silo"), PathBuf::from("/c/silo"));
    }
}
