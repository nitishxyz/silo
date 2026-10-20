//! Upgrade streams: replace a server with another build. Every other
//! stream needs identical builds; this one is understood across builds (it
//! is how one replaces another). Frozen: the frame header
//! (`u32 len | u8 kind | payload`, see [`crate::frame`]), the `UPGRADE_*`
//! and `HOST_BUILD` kinds and the JSON here, which only ever gains optional
//! fields. It has its own [`UpgradeRejected`] so `REJECT` and the data
//! stream types stay free to change.
//!
//! The first stream of a connection starts with `UPGRADE_HELLO` (`{}`);
//! the server answers [`HostBuild`]. The client may stop there (a check)
//! or send [`UpgradeBinary`] and then `UPGRADE_CHUNK` frames holding
//! exactly `size` bytes, or, with `release`, no chunks: the server fetches
//! that release for its platform itself ([`release_url`]) and checks it
//! against the release's [`CHECKSUMS`]. The server checks the binary, runs its
//! `--version`, applies [`decide`], installs it over its own executable
//! (keeping the previous one) and runs its `silo up`, answering
//! [`UpgradeStarted`]. If `silo up` succeeds (the takeover committed and
//! the old server exits, or the build was already running) the stream
//! ends; if it fails, the server restores the previous executable and
//! sends `UPGRADE_REJECTED` ([`HANDOFF`]).
//! `UPGRADE_REJECTED` may come at any point (codes [`UP_TO_DATE`],
//! [`NEWER`], [`BUSY`], others).

use std::cmp::Ordering;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Largest binary accepted.
pub const MAX_BINARY: u64 = 512 * 1024 * 1024;
/// Largest `UPGRADE_CHUNK` payload.
pub const CHUNK: usize = 256 * 1024;
/// Reject code: the host already runs that build.
pub const UP_TO_DATE: &str = "up_to_date";
/// Reject code: the host runs a newer version.
pub const NEWER: &str = "newer";
/// Reject code: the requested release has no asset for the host's platform.
pub const NOT_PUBLISHED: &str = "not_published";
/// Reject code: another upgrade is in progress.
pub const BUSY: &str = "busy";
/// Reject code: the new build could not take over; the old one keeps
/// serving from the restored executable.
pub const HANDOFF: &str = "handoff";

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UpgradeHello {}

/// The running server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostBuild {
    pub version: String,
    pub build: String,
    /// `<os>-<arch>` as in the release assets, e.g. `linux-x86_64`.
    pub platform: String,
    /// Ids of its sessions.
    pub sessions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpgradeBinary {
    /// 0 with `release`.
    pub size: u64,
    /// Lowercase hex SHA-256 of the binary; empty with `release`.
    pub sha256: String,
    /// Install even when [`decide`] says otherwise.
    #[serde(default)]
    pub force: bool,
    /// Fetch this released version (e.g. `0.1.0`) instead of receiving the
    /// binary.
    #[serde(default)]
    pub release: Option<String>,
}

/// Where releases are published: `<RELEASES>/v<version>/<file>`.
pub const RELEASES: &str = "https://github.com/nitishxyz/silo/releases/download";
/// The release's checksums file: `sha256sum` output for every asset.
pub const CHECKSUMS: &str = "SHA256SUMS";
/// The latest published release (neither draft nor pre-release), as
/// GitHub's REST API describes it (`tag_name` is `v<version>`).
pub const LATEST: &str = "https://api.github.com/repos/nitishxyz/silo/releases/latest";

/// The release asset holding `silo` for `platform` (`<os>-<arch>`).
#[must_use]
pub fn asset(platform: &str) -> String {
    format!("silo-{platform}.tar.gz")
}

/// The release asset holding the macOS app (`Silo.app` at its root).
#[must_use]
pub fn app_image(version: &str) -> String {
    format!("Silo-{version}.dmg")
}

/// Whether `version` is a release version: `MAJOR.MINOR.PATCH` with an
/// optional `-<pre-release>` of ASCII alphanumerics, `.` and `-`. Nothing
/// else may reach a release URL (it comes from a peer).
#[must_use]
pub fn release_version(version: &str) -> bool {
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let mut parts = core.split('.');
    let numbers = (0..3).all(|_| parts.next().is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())));
    numbers
        && parts.next().is_none()
        && pre.is_none_or(|pre| !pre.is_empty() && pre.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
}

/// URL of `file` in release `version`; `base` (like install.sh's
/// `SILO_DOWNLOAD_BASE`) is a directory URL holding the assets instead.
/// `None` unless `version` is a [`release_version`].
#[must_use]
pub fn release_url(base: Option<&str>, version: &str, file: &str) -> Option<String> {
    if !release_version(version) {
        return None;
    }
    Some(match base.map(|b| b.trim_end_matches('/')).filter(|b| !b.is_empty()) {
        Some(base) => format!("{base}/{file}"),
        None => format!("{RELEASES}/v{version}/{file}"),
    })
}

/// `file`'s lowercase hex SHA-256 in `sha256sum` output.
#[must_use]
pub fn checksum<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        (name == file && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
    })
}

/// Why release `version` cannot be installed on `platform`.
#[must_use]
pub fn not_published(version: &str, platform: &str) -> String {
    format!("silo {version} isn't published for {platform}; use --local or --binary")
}

/// The new binary is installed and taking over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpgradeStarted {
    pub version: String,
    pub build: String,
}

/// The upgrade was refused or failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpgradeRejected {
    pub code: String,
    pub reason: String,
}

/// The frozen upgrade-stream messages' JSON Schemas, without prose, keys
/// sorted; pinned by `upgrade.json` (see the `frozen_schema` test).
#[must_use]
pub fn upgrade_schema() -> serde_json::Value {
    fn plain(value: serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match value {
            Value::Object(map) => {
                let mut entries: Vec<(String, Value)> = map.into_iter().filter(|(k, _)| k != "description").collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                Value::Object(entries.into_iter().map(|(k, v)| (k, plain(v))).collect())
            }
            Value::Array(items) => Value::Array(items.into_iter().map(plain).collect()),
            other => other,
        }
    }
    use schemars::schema_for;
    plain(serde_json::json!({
        "UpgradeHello": schema_for!(UpgradeHello).to_value(),
        "HostBuild": schema_for!(HostBuild).to_value(),
        "UpgradeBinary": schema_for!(UpgradeBinary).to_value(),
        "UpgradeStarted": schema_for!(UpgradeStarted).to_value(),
        "UpgradeRejected": schema_for!(UpgradeRejected).to_value(),
    }))
}

/// This machine's `<os>-<arch>`.
#[must_use]
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// `silo --version` output (without the `silo ` prefix clap adds).
#[must_use]
pub fn version_text(version: &str, build: &str) -> String {
    format!("{version} ({build})")
}

/// Version and build from `silo --version` output; builds before the build
/// was shown have none.
#[must_use]
pub fn parse_version(output: &str) -> Option<(String, Option<String>)> {
    let rest = output.trim().strip_prefix("silo ")?;
    let mut words = rest.split_whitespace();
    let version = words.next()?.to_owned();
    let build = words.next().and_then(|b| b.strip_prefix('(')?.strip_suffix(')')).map(str::to_owned);
    Some((version, build))
}

/// Whether to replace a server running `running` (version, build if known)
/// with `candidate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Upgrade,
    UpToDate,
    /// The running version is newer.
    Newer,
}

#[must_use]
pub fn decide(running: (&str, Option<&str>), candidate: (&str, &str), force: bool) -> Decision {
    if force {
        Decision::Upgrade
    } else if running.1 == Some(candidate.1) {
        Decision::UpToDate
    } else if compare_versions(running.0, candidate.0) == Some(Ordering::Greater) {
        Decision::Newer
    } else {
        Decision::Upgrade
    }
}

/// Semver order of the `major.minor.patch` cores (pre-release and build
/// metadata ignored); `None` when either does not parse.
#[must_use]
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let core = |v: &str| -> Option<[u64; 3]> {
        let v = v.split(['-', '+']).next()?;
        let mut parts = v.split('.').map(|p| p.parse::<u64>().ok());
        let parsed = [parts.next()??, parts.next()??, parts.next()??];
        parts.next().is_none().then_some(parsed)
    };
    Some(core(a)?.cmp(&core(b)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions() {
        assert_eq!(decide(("0.1.0", Some("aa")), ("0.1.0", "bb"), false), Decision::Upgrade);
        assert_eq!(decide(("0.1.0", None), ("0.1.0", "bb"), false), Decision::Upgrade);
        assert_eq!(decide(("0.1.0", Some("aa")), ("0.2.0", "bb"), false), Decision::Upgrade);
        assert_eq!(decide(("0.1.0", Some("bb")), ("0.1.0", "bb"), false), Decision::UpToDate);
        assert_eq!(decide(("0.2.0", Some("aa")), ("0.1.9", "bb"), false), Decision::Newer);
        assert_eq!(decide(("0.10.0", Some("aa")), ("0.9.0", "bb"), false), Decision::Newer);
        assert_eq!(decide(("0.2.0", Some("aa")), ("0.1.0", "bb"), true), Decision::Upgrade);
        assert_eq!(decide(("0.1.0", Some("bb")), ("0.1.0", "bb"), true), Decision::Upgrade);
    }

    #[test]
    fn releases() {
        assert_eq!(asset("linux-x86_64"), "silo-linux-x86_64.tar.gz");
        assert_eq!(
            release_url(None, "0.1.0", &asset("linux-aarch64")).unwrap(),
            "https://github.com/nitishxyz/silo/releases/download/v0.1.0/silo-linux-aarch64.tar.gz"
        );
        assert_eq!(release_url(Some("http://127.0.0.1:8000/r/"), "0.1.0", CHECKSUMS).unwrap(), "http://127.0.0.1:8000/r/SHA256SUMS");
        assert_eq!(release_url(Some(""), "0.1.0", CHECKSUMS).unwrap(), format!("{RELEASES}/v0.1.0/SHA256SUMS"));
        assert_eq!(release_url(None, "0.1.0/../../../../x/y/releases/download/v1", CHECKSUMS), None);
        assert_eq!(release_url(Some("http://127.0.0.1:8000/r"), "../1.0.0", CHECKSUMS), None);
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!("{a}  silo-linux-x86_64.tar.gz\n{b} *silo-macos-aarch64.tar.gz\nnothex  silo-linux-aarch64.tar.gz\n");
        assert_eq!(checksum(&sums, "silo-linux-x86_64.tar.gz"), Some(a.as_str()));
        assert_eq!(checksum(&sums, "silo-macos-aarch64.tar.gz"), Some(b.as_str()));
        assert_eq!(checksum(&sums, "silo-linux-aarch64.tar.gz"), None);
        assert_eq!(checksum(&sums, "silo-linux-x86_64.tar"), None);
        assert_eq!(not_published("0.1.0", "linux-aarch64"), "silo 0.1.0 isn't published for linux-aarch64; use --local or --binary");
    }

    #[test]
    fn release_versions() {
        for ok in ["0.1.0", "10.20.30", "1.0.0-rc.1", "1.0.0-alpha-2.x", "0.0.0-0"] {
            assert!(release_version(ok), "{ok}");
        }
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "1.2.x",
            "1..3",
            "1.2.3-",
            "1.2.3+meta",
            "1.2.3-a/b",
            "1.2.3-a b",
            "1.2.3/../x",
            "0.1.0/../../../../x/y/releases/download/v1",
            "1.2.3\n",
            "١.2.3",
        ] {
            assert!(!release_version(bad), "{bad:?}");
        }
    }

    /// Pins the frozen upgrade-stream JSON to `upgrade.json`: it may only gain
    /// optional fields. Update with
    /// `SILO_UPDATE_UPGRADE=1 cargo test -p silo-proto frozen_schema`.
    #[test]
    fn frozen_schema() {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/upgrade.json");
        let current = serde_json::to_string_pretty(&upgrade_schema()).unwrap() + "\n";
        if std::env::var_os("SILO_UPDATE_UPGRADE").is_some() {
            std::fs::write(file, &current).unwrap();
            return;
        }
        let pinned = std::fs::read_to_string(file).unwrap_or_default();
        assert!(
            pinned == current,
            "the upgrade stream's JSON changed; it is read across builds, so only add optional fields, then update upgrade.json: \
             SILO_UPDATE_UPGRADE=1 cargo test -p silo-proto frozen_schema"
        );
    }

    #[test]
    fn versions() {
        assert_eq!(compare_versions("1.2.3", "1.2.3-rc.1"), Some(Ordering::Equal));
        assert_eq!(compare_versions("1.2.10", "1.2.9"), Some(Ordering::Greater));
        assert_eq!(compare_versions("1.2", "1.2.0"), None);
        assert_eq!(parse_version("silo 0.1.0 (abc)\n"), Some(("0.1.0".into(), Some("abc".into()))));
        assert_eq!(parse_version("silo 0.1.0"), Some(("0.1.0".into(), None)));
        assert_eq!(parse_version("bash: silo: not found"), None);
        assert_eq!(format!("silo {}", version_text("0.1.0", "abc")), "silo 0.1.0 (abc)");
    }
}
