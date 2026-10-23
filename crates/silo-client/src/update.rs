//! Updating this machine's Silo to the latest release; shared by the
//! app's updater and `silo update`.
//!
//! - Check: one request for the latest published release (GitHub's
//!   `releases/latest`: neither draft nor pre-release); newer means a
//!   higher `major.minor.patch` than what is installed. Never a downgrade.
//! - Silo.app: the release's `SHA256SUMS` and `Silo-<version>.dmg` go into
//!   a staging directory next to the app (`.silo-update.<pid>`, so the
//!   final rename stays on one volume), the image checked against the
//!   sums. It is mounted read-only and hidden; its Silo.app must pass
//!   `codesign --verify --deep --strict`, carry the release's version and
//!   be signed like the installed app ([`same_identity`]); it is copied
//!   into the staging directory and checked again. [`install`] then swaps
//!   it with the installed app in one atomic rename (`RENAME_SWAP`), so the
//!   destination always holds a whole app; the old one is left in the
//!   staging directory for [`clean_stale`] (a running app may still load
//!   files from it). The new app's launch runs `silo up` as always, which
//!   takes the local server over to the new build live.
//! - A standalone `silo` (install.sh): the release's
//!   `silo-<os>-<arch>.tar.gz`, checked against the sums and unpacked next
//!   to the binary ([`silo_proto::release::download`]), must report the
//!   release's version and, on macOS, be signed like the installed binary
//!   ([`same_signer`]); it is renamed over the binary.
//!
//! The identity rule: a Developer ID build updates only to one of the same
//! team and signing certificate name, also checked as a code requirement
//! anchored at Apple's CA; an ad-hoc signed build only to an ad-hoc signed
//! one (a local build, for tests: no published release is ad hoc).
//! Anything unsigned, of another team or of the other kind is refused; an
//! app must be `xyz.nitish.silo` on both sides.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

use silo_proto::{release, upgrade};

/// Silo.app's bundle identifier.
pub const APP_ID: &str = "xyz.nitish.silo";
/// Posted (a Darwin notification) once [`install`] replaced Silo.app: a
/// running app restarts into the new version.
#[cfg(target_os = "macos")]
const RELAUNCH: &std::ffi::CStr = c"xyz.nitish.silo.relaunch";
/// Largest app image downloaded.
const MAX_IMAGE: u64 = 1024 * 1024 * 1024;
/// Largest latest-release description read.
const MAX_LATEST: u64 = 1024 * 1024;
/// Longest run of `hdiutil`, `codesign` or `ditto`.
const TOOL_TIMEOUT: Duration = Duration::from_secs(300);
/// Staging directories next to the app: `.silo-update.<pid>`.
const STAGE_PREFIX: &str = ".silo-update.";
/// [`PrivateDir`]s: `$TMPDIR/silo-update.<pid>.<random>`.
const TEMP_PREFIX: &str = "silo-update.";

/// The `<string>` after `<key>key</key>` in an XML plist.
#[must_use]
pub fn plist_string<'a>(plist: &'a str, key: &str) -> Option<&'a str> {
    let rest = &plist[plist.find(&format!("<key>{key}</key>"))?..];
    let start = rest.find("<string>")? + "<string>".len();
    Some(rest[start..start + rest[start..].find("</string>")?].trim())
}

/// The version of the Silo.app at `app`, if it is one.
#[must_use]
pub fn app_version(app: &Path) -> Option<String> {
    let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).ok()?;
    (plist_string(&plist, "CFBundleIdentifier") == Some(APP_ID)).then(|| plist_string(&plist, "CFBundleShortVersionString").map(str::to_owned))?
}

/// Where releases are looked up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feed {
    /// The latest release, as GitHub's REST API describes it.
    latest: String,
    /// A directory URL holding the assets instead of the release's.
    assets: Option<String>,
    https: bool,
}

impl Feed {
    #[must_use]
    pub fn github() -> Self {
        Self { latest: upgrade::LATEST.into(), assets: None, https: true }
    }

    /// The test feed at `base`, a loopback http or file URL: `latest.json`
    /// and the assets in that directory.
    #[must_use]
    pub fn local(base: &str) -> Option<Self> {
        let base = base.trim_end_matches('/');
        loopback(base).then(|| Self { latest: format!("{base}/latest.json"), assets: Some(base.to_owned()), https: false })
    }

    /// GitHub, or the test feed `SILO_UPDATE_FEED` and `true`: tests only,
    /// it also lets builds update that otherwise never would (see the app's
    /// updater and `silo update`). `None` when that is set but not a local
    /// feed: no updates.
    #[must_use]
    pub fn from_env() -> Option<(Self, bool)> {
        match std::env::var("SILO_UPDATE_FEED").ok().filter(|v| !v.is_empty()) {
            None => Some((Self::github(), false)),
            Some(base) => {
                let feed = Self::local(&base);
                if feed.is_none() {
                    tracing::warn!("update: SILO_UPDATE_FEED must be a loopback http or file URL; not updating");
                }
                feed.map(|f| (f, true))
            }
        }
    }

    fn asset(&self, version: &str, file: &str) -> Result<String, String> {
        upgrade::release_url(self.assets.as_deref(), version, file).ok_or_else(|| format!("{version:?} is not a release version"))
    }
}

/// `file:///…`, or `http://` to exactly `127.0.0.1` or `localhost` (an
/// optional `:port`) followed by a path: nothing curl could take for
/// another host (no userinfo, no other characters in the authority).
fn loopback(url: &str) -> bool {
    if url.starts_with("file:///") {
        return !url.contains('@');
    }
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let Some((authority, _)) = rest.split_once('/') else { return false };
    let (host, port_ok) = match authority.split_once(':') {
        Some((host, port)) => (host, !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())),
        None => (authority, true),
    };
    matches!(host, "127.0.0.1" | "localhost") && port_ok && !url.contains('@')
}

/// The version of a latest-release description, if it is a published
/// release (`v<major.minor.patch>`).
#[must_use]
pub fn latest_version(json: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Latest {
        tag_name: String,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        prerelease: bool,
    }
    let latest: Latest = serde_json::from_slice(json).ok()?;
    let version = latest.tag_name.strip_prefix('v')?;
    let plain = upgrade::release_version(version) && !version.contains('-');
    (plain && !latest.draft && !latest.prerelease).then(|| version.to_owned())
}

/// Whether `candidate` is a higher version than `running`: a higher
/// `major.minor.patch`, or the same one when `running` is its pre-release
/// (`0.2.0-rc.1` < `0.2.0`). The feed never offers pre-releases.
#[must_use]
pub fn is_newer(running: &str, candidate: &str) -> bool {
    use std::cmp::Ordering;
    match upgrade::compare_versions(running, candidate) {
        Some(Ordering::Less) => true,
        Some(Ordering::Equal) => running.contains('-') && !candidate.contains(['-', '+']),
        _ => false,
    }
}

/// A fresh directory only this user can read (`$TMPDIR/silo-update.<pid>.<random>`,
/// mode 0700), removed on drop; downloads and mounts go there.
pub struct PrivateDir(PathBuf);

impl PrivateDir {
    /// # Errors
    /// No directory can be created.
    pub fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        for _ in 0..8 {
            let path = std::env::temp_dir().join(format!("{TEMP_PREFIX}{}.{:016x}", std::process::id(), rand::random::<u64>()));
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("no fresh temporary directory"))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The latest release's version on `feed`.
///
/// # Errors
/// The feed cannot be read or names no release.
pub async fn latest(feed: &Feed) -> Result<String, String> {
    let dir = PrivateDir::new().map_err(|e| format!("create a temporary folder: {e}"))?;
    let file = dir.path().join("latest.json");
    release::fetch(&feed.latest, &file, feed.https, MAX_LATEST).await.map_err(|(_, e)| e)?.ok_or("no published release")?;
    let body = std::fs::read(&file).map_err(|e| e.to_string())?;
    latest_version(&body).ok_or_else(|| "the latest release has no version".into())
}

/// The release newer than `running` published on `feed`, if any.
///
/// # Errors
/// The feed cannot be read.
pub async fn check_latest(feed: &Feed, running: &str) -> Result<Option<String>, String> {
    let version = latest(feed).await?;
    Ok(is_newer(running, &version).then_some(version))
}

/// How code is signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    AdHoc,
    /// A certificate of an Apple team: its team id and leaf name, e.g.
    /// `Developer ID Application: Name (TEAMID)`.
    Team {
        team: String,
        authority: String,
    },
}

/// Code's signing identifier and identity.
pub type Signature = (String, Identity);

/// Signing identifier and identity from `codesign -dvv` output; `None`
/// when unsigned or neither ad hoc nor of a team.
#[must_use]
pub fn parse_signature(text: &str) -> Option<Signature> {
    let field = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).map(str::trim);
    let identifier = field("Identifier")?.to_owned();
    if field("Signature") == Some("adhoc") {
        return Some((identifier, Identity::AdHoc));
    }
    let team = field("TeamIdentifier").filter(|t| *t != "not set" && !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric()))?;
    let authority = field("Authority")?;
    Some((identifier, Identity::Team { team: team.to_owned(), authority: authority.to_owned() }))
}

/// Whether code signed as `candidate` may replace code signed as `running`
/// (the module's identity rule, without identifiers: a binary's is its
/// file name).
///
/// # Errors
/// Why not, for the log.
pub fn same_signer(running: &Identity, candidate: &Identity) -> Result<(), String> {
    if running == candidate { Ok(()) } else { Err(format!("signed as {candidate:?}, not as the installed copy ({running:?})")) }
}

/// Whether an app signed as `candidate` may replace one signed as
/// `running`: [`same_signer`], and both Silo.app.
///
/// # Errors
/// Why not, for the log.
pub fn same_identity(running: &Signature, candidate: &Signature) -> Result<(), String> {
    if running.0 != APP_ID || candidate.0 != APP_ID {
        return Err(format!("bundle identifier {} replacing {}", candidate.0, running.0));
    }
    same_signer(&running.1, &candidate.1)
}

/// The code requirement a team's code must satisfy: Apple-issued, that
/// team and, for an app, that identifier. As `codesign -R` takes it: text
/// after `=` (anything else is read as a file's path).
fn requirement(team: &str, identifier: Option<&str>) -> String {
    let identifier = identifier.map(|id| format!(" and identifier \"{id}\"")).unwrap_or_default();
    format!("=anchor apple generic{identifier} and certificate leaf[subject.OU] = \"{team}\"")
}

/// Run `program` (bounded by [`TOOL_TIMEOUT`]); its output.
async fn run(program: &str, args: &[&OsStr]) -> Result<std::process::Output, String> {
    let child = tokio::process::Command::new(program).args(args).stdin(std::process::Stdio::null()).kill_on_drop(true).output();
    match tokio::time::timeout(TOOL_TIMEOUT, child).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(format!("run {program}: {e}")),
        Err(_) => Err(format!("{program} timed out")),
    }
}

/// Run `program`, failing unless it succeeds.
async fn check(program: &str, args: &[&OsStr]) -> Result<std::process::Output, String> {
    let out = run(program, args).await?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(format!("{program} {:?}: {}", args.first().unwrap_or(&OsStr::new("")), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// How the app or binary at `path` is signed.
///
/// # Errors
/// `codesign` fails, or it is unsigned.
pub async fn signature(path: &Path) -> Result<Signature, String> {
    let out = check("codesign", &["-dvv".as_ref(), path.as_os_str()]).await?;
    // codesign describes the signature on stderr.
    parse_signature(&String::from_utf8_lossy(&out.stderr)).ok_or_else(|| format!("{} is not signed", path.display()))
}

/// `codesign --verify --strict` `path` (`--deep` for an app), against the
/// team's requirement when signed by one.
async fn verify_code(path: &Path, identity: &Identity, app: bool) -> Result<(), String> {
    let requirement = match identity {
        Identity::Team { team, .. } => Some(requirement(team, app.then_some(APP_ID))),
        Identity::AdHoc => None,
    };
    let mut args: Vec<&OsStr> = vec!["--verify".as_ref(), "--strict".as_ref()];
    if app {
        args.push("--deep".as_ref());
    }
    if let Some(requirement) = &requirement {
        args.extend([OsStr::new("-R"), OsStr::new(requirement)]);
    }
    args.push(path.as_os_str());
    check("codesign", &args).await.map(drop)
}

/// `app` is intact, signed like `running` and Silo `version`.
///
/// # Errors
/// Why not.
pub async fn verify_app(app: &Path, version: &str, running: &Signature) -> Result<(), String> {
    let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).map_err(|e| format!("read Info.plist: {e}"))?;
    let found = plist_string(&plist, "CFBundleShortVersionString");
    if found != Some(version) {
        return Err(format!("the image holds Silo {found:?}, not {version}"));
    }
    let candidate = signature(app).await?;
    same_identity(running, &candidate)?;
    verify_code(app, &candidate.1, true).await
}

/// Why an update could not be prepared.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// It could not be downloaded or staged: try again later.
    Retry(String),
    /// The release is not what it must be (checksums, version, signature):
    /// try again later.
    Invalid(String),
    /// The app's folder is not writable: it cannot be replaced.
    Unwritable,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retry(error) => f.write_str(error),
            Self::Invalid(error) => write!(f, "the update failed verification: {error}"),
            Self::Unwritable => f.write_str("Silo.app's folder is not writable"),
        }
    }
}

/// How far [`prepare`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The app image is downloading: `done` of `total` bytes (`None`: the
    /// server didn't say).
    Download { done: u64, total: Option<u64> },
    /// Checksum, mount, signature checks and copy.
    Verify,
}

/// Where [`prepare`] reports its [`Phase`].
pub type Progress = tokio::sync::watch::Sender<Phase>;

/// `done` of `total` in whole percent, 100 only once complete.
#[must_use]
pub fn percent(done: u64, total: u64) -> u8 {
    u8::try_from(done.min(total) * 100 / total.max(1)).unwrap_or(100)
}

/// Bytes in whole (decimal) megabytes, as Finder counts them.
#[must_use]
pub fn megabytes(bytes: u64) -> u64 {
    (bytes + 500_000) / 1_000_000
}

/// How much is downloaded: `42%`, or `84 MB` with no known size.
#[must_use]
pub fn downloaded(done: u64, total: Option<u64>) -> String {
    match total {
        Some(total) => format!("{}%", percent(done, total)),
        None => format!("{} MB", megabytes(done)),
    }
}

/// `<app's folder>/.silo-update.<pid>`.
fn stage_dir(app: &Path) -> PathBuf {
    app.with_file_name(format!("{STAGE_PREFIX}{}", std::process::id()))
}

/// A fresh, empty staging directory for `app`.
fn fresh_stage(app: &Path) -> Result<PathBuf, Failure> {
    let stage = stage_dir(app);
    let _ = std::fs::remove_dir_all(&stage);
    match std::fs::create_dir(&stage) {
        Ok(()) => Ok(stage),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem) => Err(Failure::Unwritable),
        Err(e) => Err(Failure::Retry(format!("create the staging folder: {e}"))),
    }
}

/// Download release `version`'s app image into `dir`, checked against
/// the release's checksums, reporting to `progress`.
///
/// # Errors
/// Not published or not downloadable ([`Failure::Retry`]), not matching
/// ([`Failure::Invalid`]).
pub async fn download_image(feed: &Feed, version: &str, dir: &Path, progress: Option<&Progress>) -> Result<PathBuf, Failure> {
    let image = upgrade::app_image(version);
    let sums = dir.join(upgrade::CHECKSUMS);
    let fetch = async |file: &str, to: &Path, max: u64, progress: Option<&Progress>| -> Result<(), Failure> {
        let url = feed.asset(version, file).map_err(Failure::Invalid)?;
        let report = progress.map(|tx| {
            move |done: u64, total: Option<u64>| {
                tx.send_replace(Phase::Download { done, total });
            }
        });
        release::fetch_with_progress(&url, to, feed.https, max, report)
            .await
            .map_err(|(_, e)| Failure::Retry(e))?
            .ok_or_else(|| Failure::Retry(format!("{file} is not published")))
    };
    fetch(upgrade::CHECKSUMS, &sums, MAX_LATEST, None).await?;
    let sums = std::fs::read_to_string(&sums).map_err(|e| Failure::Retry(e.to_string()))?;
    let expected = upgrade::checksum(&sums, &image).ok_or_else(|| Failure::Invalid(format!("{image} is not in the checksums")))?.to_ascii_lowercase();
    let path = dir.join(&image);
    fetch(&image, &path, MAX_IMAGE, progress).await?;
    if let Some(tx) = progress {
        tx.send_replace(Phase::Verify);
    }
    if release::sha256_file(&path).map_err(|e| Failure::Retry(e.to_string()))? != expected {
        let _ = std::fs::remove_file(&path);
        return Err(Failure::Invalid(format!("{image} does not match the release's {}", upgrade::CHECKSUMS)));
    }
    Ok(path)
}

/// A mounted disk image, detached on drop (also when the update is
/// cancelled midway).
struct Mounted(PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        detach(&self.0);
    }
}

/// Force-detach the image mounted at `mount`.
fn detach(mount: &Path) {
    let out = std::process::Command::new("hdiutil").arg("detach").arg(mount).arg("-force").stdin(std::process::Stdio::null()).output();
    match out {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!("update: hdiutil detach: {}", String::from_utf8_lossy(&out.stderr).trim()),
        Err(error) => tracing::warn!("update: hdiutil detach: {error}"),
    }
}

/// Whether something is mounted at `path` (another device than its parent).
fn mount_point(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev());
    path.parent().is_some_and(|parent| matches!((dev(path), dev(parent)), (Ok(a), Ok(b)) if a != b))
}

/// Mount `image` in `dir`, check its Silo.app and copy it to `dest`.
async fn unpack(image: &Path, dir: &Path, version: &str, running: &Signature, dest: &Path) -> Result<(), Failure> {
    let mount = dir.join("mnt");
    std::fs::create_dir(&mount).map_err(|e| Failure::Retry(format!("create the mount point: {e}")))?;
    let attach = ["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"].map(OsStr::new);
    check("hdiutil", &[&attach[..], &[mount.as_os_str(), image.as_os_str()]].concat()).await.map_err(Failure::Retry)?;
    let _mounted = Mounted(mount.clone());
    let app = mount.join("Silo.app");
    verify_app(&app, version, running).await.map_err(Failure::Invalid)?;
    check("ditto", &[app.as_os_str(), dest.as_os_str()]).await.map(drop).map_err(Failure::Retry)
}

/// Download, check and stage release `version` of the Silo.app at `app`
/// (signed as `running`), reporting to `progress`; the staged app.
///
/// # Errors
/// [`Failure`].
pub async fn prepare(feed: &Feed, version: &str, app: &Path, running: &Signature, progress: Option<&Progress>) -> Result<PathBuf, Failure> {
    let stage = fresh_stage(app)?;
    let staged = stage.join("Silo.app");
    let result = async {
        let image = download_image(feed, version, &stage, progress).await?;
        let mounts = PrivateDir::new().map_err(|e| Failure::Retry(format!("create a temporary folder: {e}")))?;
        let unpacked = unpack(&image, mounts.path(), version, running, &staged).await;
        let _ = std::fs::remove_file(&image);
        let _ = std::fs::remove_file(stage.join(upgrade::CHECKSUMS));
        unpacked?;
        verify_app(&staged, version, running).await.map_err(Failure::Invalid)
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    result.map(|()| staged)
}

/// Swap `staged` and `app` in one rename: `app` becomes the new app and
/// `staged` holds the old one.
///
/// # Errors
/// The rename fails (the folder is not writable, `staged` is gone).
#[cfg(target_os = "macos")]
pub fn install(staged: &Path, app: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
    let (from, to) = (c(staged)?, c(app)?);
    // SAFETY: both are valid NUL-terminated paths.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// The staging directory holding the app [`install`] replaced.
#[must_use]
pub fn replaced(staged: &Path) -> Option<&Path> {
    staged.parent().filter(|d| d.file_name().and_then(OsStr::to_str).is_some_and(|n| n.starts_with(STAGE_PREFIX)))
}

/// Open `app` once this process has exited (it inherits the environment).
///
/// # Errors
/// The waiter cannot be started.
pub fn relaunch(app: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; exec /usr/bin/open "$2""#)
        .arg("silo-relaunch")
        .arg(std::process::id().to_string())
        .arg(app)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

/// Remove staging directories (and the apps they replaced) next to `app`
/// left by processes that are gone, and their [`PrivateDir`]s, detaching
/// an image still mounted there.
pub fn clean_stale(app: &Path) {
    clean_stale_temp(&std::env::temp_dir());
    let Some(dir) = app.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        let pid = path.file_name().and_then(|n| n.to_str()?.strip_prefix(STAGE_PREFIX)?.parse::<i32>().ok());
        if pid.is_some_and(|pid| !alive(pid)) && std::fs::remove_dir_all(&path).is_ok() {
            tracing::info!("update: removed {}", path.display());
        }
    }
}

/// [`PrivateDir`]s in `tmp` of processes that are gone.
fn clean_stale_temp(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else { return };
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        let pid = path.file_name().and_then(|n| n.to_str()?.strip_prefix(TEMP_PREFIX)?.split('.').next()?.parse::<i32>().ok());
        let ours = std::fs::symlink_metadata(&path)
            .is_ok_and(|m| m.is_dir() && std::os::unix::fs::MetadataExt::uid(&m) == rustix::process::getuid().as_raw());
        if !ours || pid.is_none_or(alive) {
            continue;
        }
        let mount = path.join("mnt");
        if mount_point(&mount) {
            detach(&mount);
        }
        if !mount_point(&mount) && std::fs::remove_dir_all(&path).is_ok() {
            tracing::info!("update: removed {}", path.display());
        }
    }
}

/// Whether process `pid` exists.
#[must_use]
pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Processes running the app at `app` (canonical).
#[cfg(target_os = "macos")]
#[must_use]
pub fn running_apps(app: &Path) -> Vec<i32> {
    use std::os::unix::ffi::OsStrExt;
    let exe = app.join("Contents/MacOS/Silo");
    let mut pids = vec![0i32; 8192];
    let size = i32::try_from(pids.len() * std::mem::size_of::<i32>()).unwrap_or(i32::MAX);
    // SAFETY: the buffer holds `size` bytes.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    pids.truncate(usize::try_from(count).unwrap_or(0));
    let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    pids.into_iter()
        .filter(|&pid| {
            // SAFETY: the buffer holds `path.len()` bytes.
            let n = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), u32::try_from(path.len()).unwrap_or(0)) };
            usize::try_from(n).is_ok_and(|n| n > 0 && Path::new(OsStr::from_bytes(&path[..n])) == exe)
        })
        .collect()
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn notify_post(name: *const std::ffi::c_char) -> u32;
    fn notify_register_file_descriptor(name: *const std::ffi::c_char, notify_fd: *mut i32, flags: i32, out_token: *mut i32) -> u32;
}

/// Tell running apps that Silo.app was replaced on disk. Any process may
/// post this, so an app acts on it only when its bundle on disk is another
/// version.
#[cfg(target_os = "macos")]
pub fn post_relaunch() {
    // SAFETY: a valid NUL-terminated name.
    let status = unsafe { notify_post(RELAUNCH.as_ptr()) };
    if status != 0 {
        tracing::warn!(status, "update: notify_post failed");
    }
}

/// Call `f` (on a thread of its own) whenever [`post_relaunch`] posts.
///
/// # Errors
/// The notification cannot be registered for.
#[cfg(target_os = "macos")]
pub fn on_relaunch(f: impl Fn() + Send + 'static) -> std::io::Result<()> {
    use std::{io::Read as _, os::fd::FromRawFd as _};
    let (mut fd, mut token) = (-1, 0);
    // SAFETY: valid name and out pointers; flags 0 creates a new descriptor.
    let status = unsafe { notify_register_file_descriptor(RELAUNCH.as_ptr(), &raw mut fd, 0, &raw mut token) };
    if status != 0 || fd < 0 {
        return Err(std::io::Error::other(format!("notify_register_file_descriptor: {status}")));
    }
    // SAFETY: the descriptor was just created for us and is owned here.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    std::thread::Builder::new().name("silo-relaunch".into()).spawn(move || {
        // Each post writes the 4-byte token.
        let mut token = [0u8; 4];
        while file.read_exact(&mut token).is_ok() {
            f();
        }
    })?;
    Ok(())
}

/// How the running `silo` is installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// It comes with Silo.app: the bundle's own server (that bundle), or a
    /// runtime copy under `<SILO_HOME>/runtime` (which bundle is unknown).
    App(Option<PathBuf>),
    /// A binary of its own (install.sh).
    Standalone(PathBuf),
    /// A debug build, or anything in a cargo target directory.
    Dev,
}

/// How `exe` (canonical) is installed, for state directory `home`
/// (canonical); `release` is false for debug builds.
#[must_use]
pub fn origin(exe: &Path, home: &Path, release: bool) -> Origin {
    // Cargo marks its target directories with CACHEDIR.TAG.
    if !release || exe.ancestors().skip(1).any(|dir| dir.join("CACHEDIR.TAG").is_file()) {
        return Origin::Dev;
    }
    let dir = exe.parent();
    if exe.file_name() == Some(OsStr::new("silo")) && dir.and_then(Path::parent) == Some(&crate::runtime::root(home)) {
        return Origin::App(None);
    }
    let bundle =
        dir.filter(|d| exe.file_name() == Some(OsStr::new("silo-server")) && d.ends_with("Contents/MacOS")).and_then(|d| d.parent()?.parent());
    match bundle.filter(|b| b.extension() == Some(OsStr::new("app"))) {
        Some(app) => Origin::App(Some(app.to_owned())),
        None => Origin::Standalone(exe.to_owned()),
    }
}

/// Folders Silo.app is installed in: /Applications and ~/Applications
/// (only the latter with the test feed, so tests never see a real app).
#[must_use]
pub fn app_roots(user_home: &Path, test: bool) -> Vec<PathBuf> {
    let user = user_home.join("Applications");
    if test { vec![user] } else { vec![PathBuf::from("/Applications"), user] }
}

/// The installed Silo.app (canonical) in the first of `roots` holding one.
#[must_use]
pub fn find_app(roots: &[PathBuf]) -> Option<PathBuf> {
    roots.iter().map(|root| root.join("Silo.app")).find(|app| app_version(app).is_some()).and_then(|app| app.canonicalize().ok())
}

/// Download release `version` of `silo` for this platform next to `exe`
/// (the installed standalone binary) and check it; the staged binary, to
/// be renamed over `exe`.
///
/// # Errors
/// Not published, not downloadable, not matching, not that version, or
/// (macOS) not signed like `exe`.
pub async fn prepare_binary(feed: &Feed, version: &str, exe: &Path) -> Result<PathBuf, String> {
    let dir = exe.parent().ok_or("the binary has no folder")?;
    let staged = dir.join(format!(".silo.new.{}", std::process::id()));
    let _ = std::fs::remove_file(&staged);
    let platform = upgrade::platform();
    let result = async {
        release::download(&staged, feed.assets.as_deref(), version, &platform).await.map_err(|(code, error)| match code {
            upgrade::NOT_PUBLISHED => format!("silo {version} isn't published for {platform}"),
            _ if error.contains("ermission denied") => format!("can't write to {}", dir.display()),
            _ => error,
        })?;
        let (found, _) = release::version_of(&staged).await.map_err(|(_, e)| e)?;
        if found != version {
            return Err(format!("the release's silo reports {found}, not {version}"));
        }
        if cfg!(target_os = "macos") {
            let (running, candidate) = (signature(exe).await?, signature(&staged).await?);
            same_signer(&running.1, &candidate.1)?;
            verify_code(&staged, &candidate.1, false).await?;
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => Ok(staged),
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's directory, removed when the test ends, failed or not.
    struct Temp(PathBuf);

    impl std::ops::Deref for Temp {
        type Target = PathBuf;
        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp(name: &str) -> Temp {
        let dir = std::env::temp_dir().join(format!("silo-update-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Temp(dir.canonicalize().unwrap())
    }

    #[test]
    fn private_dirs() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let (a, b) = (PrivateDir::new().unwrap(), PrivateDir::new().unwrap());
        assert_ne!(a.path(), b.path(), "unpredictable names");
        let name = a.path().file_name().unwrap().to_str().unwrap().to_owned();
        assert!(name.starts_with(&format!("silo-update.{}.", std::process::id())), "{name}");
        assert_eq!(std::fs::metadata(a.path()).unwrap().permissions().mode() & 0o777, 0o700);
        let path = a.path().to_owned();
        drop(a);
        assert!(!path.exists(), "removed on drop");
        // A gone process's directory is cleaned, a live one's kept.
        let tmp = temp("stale-temp");
        let gone = tmp.join(format!("silo-update.{}.0123", i32::MAX));
        let live = tmp.join(format!("silo-update.{}.0123", std::process::id()));
        for dir in [&gone, &live] {
            std::fs::create_dir_all(dir.join("mnt")).unwrap();
        }
        clean_stale_temp(&tmp);
        assert!(!gone.exists() && live.exists());
        assert_eq!(std::fs::metadata(&live).unwrap().uid(), rustix::process::getuid().as_raw());
        assert!(!mount_point(&live.join("mnt")));
    }

    #[test]
    fn latest_release_versions() {
        let json = |tag: &str, draft: bool, pre: bool| format!(r#"{{"tag_name":"{tag}","draft":{draft},"prerelease":{pre},"assets":[]}}"#);
        assert_eq!(latest_version(json("v0.1.2", false, false).as_bytes()).as_deref(), Some("0.1.2"));
        assert_eq!(latest_version(br#"{"tag_name":"v10.0.1"}"#).as_deref(), Some("10.0.1"));
        assert_eq!(latest_version(json("v0.1.2", true, false).as_bytes()), None, "draft");
        assert_eq!(latest_version(json("v0.2.0-rc.1", false, false).as_bytes()), None, "pre-release tag");
        assert_eq!(latest_version(json("v0.2.0", false, true).as_bytes()), None, "pre-release");
        assert_eq!(latest_version(json("0.1.2", false, false).as_bytes()), None, "tags start with v");
        assert_eq!(latest_version(json("v0.1.2/../x", false, false).as_bytes()), None);
        assert_eq!(latest_version(b"<html>rate limited</html>"), None);
    }

    #[test]
    fn newer_is_a_higher_version() {
        assert!(is_newer("0.1.1", "0.1.2"));
        assert!(is_newer("0.1.9", "0.1.10"));
        assert!(is_newer("0.9.0", "1.0.0"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.1", "garbage"));
        // A pre-release is older than its release, never newer.
        assert!(is_newer("0.2.0-rc.1", "0.2.0"));
        assert!(is_newer("0.2.0-rc.1", "0.2.1"));
        assert!(!is_newer("0.2.0", "0.2.0-rc.1"));
        assert!(!is_newer("0.2.0-rc.1", "0.2.0-rc.2"), "the feed never offers pre-releases");
        assert!(!is_newer("0.2.0-rc.1", "0.1.9"));
    }

    #[test]
    fn download_amounts() {
        assert_eq!(percent(0, 200), 0);
        assert_eq!(percent(84, 200), 42);
        assert_eq!(percent(199, 200), 99, "100 only once complete");
        assert_eq!(percent(200, 200), 100);
        assert_eq!(percent(300, 200), 100, "never past it");
        assert_eq!(megabytes(84_400_000), 84);
        assert_eq!(megabytes(84_600_000), 85);
        assert_eq!(megabytes(0), 0);
        assert_eq!(downloaded(84_000_000, Some(200_000_000)), "42%");
        assert_eq!(downloaded(84_000_000, None), "84 MB");
    }

    #[test]
    fn feeds() {
        assert_eq!(
            Feed::github().asset("0.1.2", "Silo-0.1.2.dmg").unwrap(),
            "https://github.com/nitishxyz/silo/releases/download/v0.1.2/Silo-0.1.2.dmg"
        );
        let local = Feed::local("http://127.0.0.1:8000/r/").unwrap();
        assert_eq!(local.latest, "http://127.0.0.1:8000/r/latest.json");
        assert_eq!(local.asset("0.1.2", upgrade::CHECKSUMS).unwrap(), "http://127.0.0.1:8000/r/SHA256SUMS");
        assert!(Feed::local("file:///tmp/feed").is_some());
        assert!(Feed::local("http://localhost:8000/feed").is_some());
        assert!(Feed::local("http://127.0.0.1/feed").is_some(), "no port");
        for remote in [
            "https://example.com/feed",
            "http://10.0.0.1:80/x",
            "http://127.0.0.1.evil.com/x",
            "http://127.0.0.1:80@evil.com/x",
            "http://localhost:1@x/",
            "http://127.0.0.1@evil.com/x",
            "http://evil.com/@127.0.0.1:80/x",
            "http://127.0.0.1:80/@evil.com",
            "http://127.0.0.1:/x",
            "http://127.0.0.1:8a/x",
            "http://127.0.0.1:80",
            "http://localhost.:1/x",
            "http://[::1]:80/x",
            "file://host/x",
            "file:///x@y",
        ] {
            assert_eq!(Feed::local(remote), None, "{remote}");
        }
        assert!(local.asset("../0.1.2", upgrade::CHECKSUMS).is_err());
    }

    const DEVELOPER: &str = "Executable=/Applications/Silo.app/Contents/MacOS/Silo\nIdentifier=xyz.nitish.silo\nFormat=app bundle with Mach-O thin (arm64)\n\
        CodeDirectory v=20500 size=77000 flags=0x10000(runtime) hashes=2400+7 location=embedded\nSignature size=9000\n\
        Authority=Developer ID Application: Nitish Kumar (FRUHQC68JC)\nAuthority=Developer ID Certification Authority\nAuthority=Apple Root CA\n\
        Timestamp=Oct 20, 2026\nInfo.plist entries=15\nTeamIdentifier=FRUHQC68JC\n";
    const ADHOC: &str = "Executable=/Applications/Silo.app/Contents/MacOS/Silo\nIdentifier=xyz.nitish.silo\n\
        CodeDirectory v=20400 size=77000 flags=0x2(adhoc) hashes=2400+3 location=embedded\nSignature=adhoc\nTeamIdentifier=not set\n";

    #[test]
    fn signatures() {
        let team = Identity::Team { team: "FRUHQC68JC".into(), authority: "Developer ID Application: Nitish Kumar (FRUHQC68JC)".into() };
        assert_eq!(parse_signature(DEVELOPER), Some(("xyz.nitish.silo".into(), team)));
        assert_eq!(parse_signature(ADHOC), Some(("xyz.nitish.silo".into(), Identity::AdHoc)));
        assert_eq!(parse_signature("Executable=/x\nIdentifier=xyz.nitish.silo\nTeamIdentifier=not set\n"), None, "neither");
        assert_eq!(parse_signature("code object is not signed at all\n"), None);
        let odd = DEVELOPER.replace("TeamIdentifier=FRUHQC68JC", "TeamIdentifier=a\" or anchor");
        assert_eq!(parse_signature(&odd), None, "a team id that could change the requirement");
    }

    #[test]
    fn identity_rule() {
        let id = |text: &str| parse_signature(text).unwrap();
        let (developer, adhoc) = (id(DEVELOPER), id(ADHOC));
        assert!(same_identity(&developer, &developer).is_ok());
        assert!(same_identity(&adhoc, &adhoc).is_ok(), "local builds update to local builds");
        assert!(same_identity(&adhoc, &developer).is_err(), "an ad-hoc app never takes a team's");
        assert!(same_identity(&developer, &adhoc).is_err(), "nor the other way");
        let other_team = id(&DEVELOPER.replace("FRUHQC68JC", "ABCDE12345"));
        assert!(same_identity(&developer, &other_team).is_err());
        let other_cert = id(&DEVELOPER.replace("Nitish Kumar", "Someone Else"));
        assert!(same_identity(&developer, &other_cert).is_err(), "same team, another certificate name");
        let other_app = id(&DEVELOPER.replace("Identifier=xyz.nitish.silo", "Identifier=xyz.nitish.silo.dev"));
        assert!(same_identity(&developer, &other_app).is_err());
        assert!(same_identity(&other_app, &other_app).is_err(), "only Silo.app");
        // A binary's identifier is its file name: only the signer counts.
        let binary = id(&DEVELOPER.replace("Identifier=xyz.nitish.silo", "Identifier=silo"));
        assert!(same_signer(&developer.1, &binary.1).is_ok());
        assert!(same_signer(&adhoc.1, &binary.1).is_err());
        assert_eq!(
            requirement("FRUHQC68JC", Some(APP_ID)),
            r#"=anchor apple generic and identifier "xyz.nitish.silo" and certificate leaf[subject.OU] = "FRUHQC68JC""#
        );
        assert_eq!(requirement("FRUHQC68JC", None), r#"=anchor apple generic and certificate leaf[subject.OU] = "FRUHQC68JC""#);
    }

    /// codesign takes the team's requirement as one and evaluates it:
    /// Apple's own code is no team's.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn team_requirements_are_evaluated() {
        let team = Identity::Team { team: "FRUHQC68JC".into(), authority: "Developer ID Application: X (FRUHQC68JC)".into() };
        let error = verify_code(Path::new("/usr/bin/true"), &team, false).await.unwrap_err();
        assert!(error.contains("failed to satisfy"), "{error}");
        verify_code(Path::new("/usr/bin/true"), &Identity::AdHoc, false).await.unwrap();
    }

    #[test]
    fn reads_plists() {
        let plist =
            "<dict>\n  <key>CFBundleName</key><string>Silo</string>\n  <key>CFBundleIdentifier</key><string>xyz.nitish.silo</string>\n</dict>";
        assert_eq!(plist_string(plist, "CFBundleIdentifier"), Some("xyz.nitish.silo"));
        assert_eq!(plist_string(plist, "CFBundleVersion"), None);
        assert_eq!(plist_string("", "CFBundleIdentifier"), None);
    }

    /// A minimal bundle at `path` with `version` in its Info.plist.
    fn bundle(path: &Path, version: &str) {
        std::fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
        std::fs::copy("/usr/bin/true", path.join("Contents/MacOS/Silo")).unwrap();
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n  <key>CFBundleIdentifier</key><string>xyz.nitish.silo</string>\n  \
             <key>CFBundleExecutable</key><string>Silo</string>\n  <key>CFBundlePackageType</key><string>APPL</string>\n  \
             <key>CFBundleShortVersionString</key><string>{version}</string>\n</dict></plist>\n"
        );
        std::fs::write(path.join("Contents/Info.plist"), plist).unwrap();
    }

    fn version_of(app: &Path) -> String {
        app_version(app).unwrap()
    }

    #[test]
    fn install_origins() {
        let dir = temp("origin");
        let home = dir.join("state");
        let runtime = home.join("runtime/0123456789abcdef/silo");
        assert_eq!(origin(&runtime, &home, true), Origin::App(None), "the app's runtime copy");
        let bundled = Path::new("/Applications/Silo.app/Contents/MacOS/silo-server");
        assert_eq!(origin(bundled, &home, true), Origin::App(Some("/Applications/Silo.app".into())));
        let standalone = dir.join(".local/bin/silo");
        assert_eq!(origin(&standalone, &home, true), Origin::Standalone(standalone.clone()));
        assert_eq!(origin(Path::new("/usr/local/bin/silo"), &home, true), Origin::Standalone("/usr/local/bin/silo".into()));
        assert_eq!(origin(&standalone, &home, false), Origin::Dev, "a debug build");
        // Anything in a cargo target directory, bundles included.
        std::fs::create_dir_all(dir.join("w/target/release")).unwrap();
        std::fs::write(dir.join("w/target/CACHEDIR.TAG"), "Signature: 8a477f597d28d172789f06886806bc55").unwrap();
        assert_eq!(origin(&dir.join("w/target/release/silo"), &home, true), Origin::Dev);
        assert_eq!(origin(&dir.join("w/target/Silo.app/Contents/MacOS/silo-server"), &home, true), Origin::Dev);
        // Another home's runtime copy, or a `silo` elsewhere in a bundle, is standalone.
        assert_eq!(origin(&dir.join("other/runtime/x/silo"), &home, true), Origin::Standalone(dir.join("other/runtime/x/silo")));
        // Apps are looked for where installed; with the test feed only in the home's.
        assert_eq!(app_roots(Path::new("/Users/bat"), false), [PathBuf::from("/Applications"), "/Users/bat/Applications".into()]);
        assert_eq!(app_roots(Path::new("/Users/bat"), true), [PathBuf::from("/Users/bat/Applications")]);
        let roots = app_roots(&dir, true);
        assert_eq!(find_app(&roots), None);
        std::fs::create_dir_all(dir.join("Applications/Silo.app/Contents")).unwrap();
        assert_eq!(find_app(&roots), None, "not an app without its Info.plist");
        bundle(&dir.join("Applications/Silo.app"), "0.1.1");
        assert_eq!(find_app(&roots), Some(dir.join("Applications/Silo.app")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn replaces_the_bundle_atomically() {
        let dir = temp("replace");
        let app = dir.join("Silo.app");
        bundle(&app, "0.1.1");
        let stage = fresh_stage(&app).unwrap();
        assert_eq!(stage, dir.join(format!(".silo-update.{}", std::process::id())));
        let staged = stage.join("Silo.app");
        bundle(&staged, "0.1.2");
        install(&staged, &app).unwrap();
        assert_eq!(version_of(&app), "0.1.2", "the new app is in place");
        assert_eq!(version_of(&staged), "0.1.1", "the old one waits in the staging folder");
        assert_eq!(replaced(&staged), Some(stage.as_path()));
        assert_eq!(replaced(&app), None);
        assert!(running_apps(&app).is_empty());
        // Ours is kept; a gone process's is removed with the app it replaced.
        let gone = dir.join(format!(".silo-update.{}", i32::MAX));
        bundle(&gone.join("Silo.app"), "0.1.0");
        std::fs::create_dir(dir.join(".silo-update.x")).unwrap();
        clean_stale(&app);
        assert!(stage.exists() && !gone.exists() && dir.join(".silo-update.x").exists());
        // Nothing staged: refused, the app left alone.
        std::fs::remove_dir_all(&stage).unwrap();
        assert!(install(&staged, &app).is_err());
        assert_eq!(version_of(&app), "0.1.2");
    }

    #[test]
    fn an_unwritable_folder_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("unwritable");
        let app = dir.join("Silo.app");
        bundle(&app, "0.1.1");
        std::fs::set_permissions(&*dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let staged = fresh_stage(&app);
        std::fs::set_permissions(&*dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(staged, Err(Failure::Unwritable));
    }

    /// `files` as a file:// feed.
    fn feed(dir: &Path, files: &[(&str, &[u8])]) -> Feed {
        let root = dir.join("feed");
        std::fs::create_dir_all(&root).unwrap();
        for (name, body) in files {
            std::fs::write(root.join(name), body).unwrap();
        }
        Feed::local(&format!("file://{}", root.display())).unwrap()
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    #[tokio::test]
    async fn downloads_a_checked_image() {
        let dir = temp("download");
        let sums = format!("{}  Silo-0.1.2.dmg\n{}  Silo-0.1.3.dmg\n", sha(b"image"), sha(b"other"));
        let feed =
            feed(&dir, &[("SHA256SUMS", sums.as_bytes()), ("Silo-0.1.2.dmg", b"image"), ("Silo-0.1.3.dmg", b"tampered"), ("Silo-0.1.4.dmg", b"x")]);
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();
        let (progress, phases) = tokio::sync::watch::channel(Phase::Download { done: 0, total: None });
        let image = download_image(&feed, "0.1.2", &out, Some(&progress)).await.unwrap();
        assert_eq!(std::fs::read(&image).unwrap(), b"image");
        assert_eq!(*phases.borrow(), Phase::Verify, "checked after the download");
        assert!(matches!(download_image(&feed, "0.1.3", &out, None).await, Err(Failure::Invalid(e)) if e.contains("does not match")));
        assert!(!out.join("Silo-0.1.3.dmg").exists(), "a mismatch is not kept");
        assert!(matches!(download_image(&feed, "0.1.4", &out, None).await, Err(Failure::Invalid(e)) if e.contains("not in the checksums")));
        assert!(matches!(download_image(&feed, "0.1.5", &dir.join("missing"), None).await, Err(Failure::Retry(_))), "not downloadable");
        std::fs::write(dir.join("feed/latest.json"), br#"{"tag_name":"v0.1.2"}"#).unwrap();
        assert_eq!(check_latest(&feed, "0.1.1").await.unwrap().as_deref(), Some("0.1.2"));
        assert_eq!(check_latest(&feed, "0.1.2").await.unwrap(), None);
        assert_eq!(latest(&feed).await.unwrap(), "0.1.2");
        std::fs::remove_file(dir.join("feed/latest.json")).unwrap();
        assert!(check_latest(&feed, "0.1.1").await.is_err());
    }

    /// A `silo` script reporting `version`, packed as this platform's
    /// release asset.
    fn tarball(dir: &Path, version: &str) -> Vec<u8> {
        let pack = dir.join(format!("pack-{version}"));
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("silo"), format!("#!/bin/sh\necho 'silo {version} (b{version})'\n")).unwrap();
        std::fs::set_permissions(pack.join("silo"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let archive = dir.join(format!("{version}.tar.gz"));
        let ok = std::process::Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(&pack).arg("silo").status().unwrap();
        assert!(ok.success());
        std::fs::read(archive).unwrap()
    }

    /// The standalone binary path up to the signature check, which needs
    /// Mach-O binaries (the CLI's integration test covers it).
    #[tokio::test]
    async fn stages_a_checked_binary() {
        let dir = temp("binary");
        let asset = upgrade::asset(&upgrade::platform());
        let (good, mislabelled) = (tarball(&dir, "0.1.2"), tarball(&dir, "0.1.1"));
        let sums = format!("{}  {asset}\n", sha(&good));
        let feed = feed(&dir, &[("SHA256SUMS", sums.as_bytes()), (&asset, &good)]);
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let exe = bin.join("silo");
        std::fs::write(&exe, "installed").unwrap();
        let staged = prepare_binary(&feed, "0.1.2", &exe).await;
        if cfg!(target_os = "macos") {
            // A script carries no signature.
            assert!(staged.unwrap_err().contains("not signed"));
        } else {
            let staged = staged.unwrap();
            assert_eq!(staged, bin.join(format!(".silo.new.{}", std::process::id())));
            assert_eq!(release::version_of(&staged).await.unwrap().0, "0.1.2");
            std::fs::remove_file(staged).unwrap();
        }
        // A tarball whose binary is another version, or not published.
        let sums = format!("{}  {asset}\n", sha(&mislabelled));
        let feed = self::feed(&dir, &[("SHA256SUMS", sums.as_bytes()), (&asset, &mislabelled)]);
        assert!(prepare_binary(&feed, "0.1.2", &exe).await.unwrap_err().contains("reports 0.1.1, not 0.1.2"));
        let feed = self::feed(&dir, &[("SHA256SUMS", b"")]);
        assert!(prepare_binary(&feed, "0.1.2", &exe).await.unwrap_err().contains("isn't published"));
        let left: Vec<_> = std::fs::read_dir(&bin).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["silo"], "nothing staged is left behind");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "installed");
    }

    /// The whole app path on a real disk image of ad-hoc signed bundles.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn stages_a_verified_app_from_an_image() {
        let dir = temp("prepare");
        let sign = |app: &Path| {
            let ok = std::process::Command::new("codesign").args(["--force", "--sign", "-", "--timestamp=none"]).arg(app).output().unwrap();
            assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
        };
        let app = dir.join("Applications/Silo.app");
        bundle(&app, "0.1.1");
        sign(&app);
        let image = |version: &str, name: &str| {
            let src = dir.join(format!("src-{name}"));
            bundle(&src.join("Silo.app"), version);
            sign(&src.join("Silo.app"));
            let dmg = dir.join(name);
            let made = std::process::Command::new("hdiutil")
                .args(["create", "-quiet", "-volname", "Silo", "-fs", "HFS+", "-format", "UDZO", "-srcfolder"])
                .arg(&src)
                .arg(&dmg)
                .output()
                .unwrap();
            assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
            std::fs::read(dmg).unwrap()
        };
        let good = image("0.1.2", "good.dmg");
        let mislabelled = image("0.1.2", "bad.dmg");
        let sums = format!("{}  Silo-0.1.2.dmg\n{}  Silo-0.1.3.dmg\n", sha(&good), sha(&mislabelled));
        let feed = feed(&dir, &[("SHA256SUMS", sums.as_bytes()), ("Silo-0.1.2.dmg", &good), ("Silo-0.1.3.dmg", &mislabelled)]);
        let running = signature(&app).await.unwrap();
        assert_eq!(running.1, Identity::AdHoc);
        let staged = prepare(&feed, "0.1.2", &app, &running, None).await.unwrap();
        assert_eq!(staged, stage_dir(&app).join("Silo.app"));
        assert_eq!(version_of(&staged), "0.1.2");
        let left: Vec<_> = std::fs::read_dir(stage_dir(&app)).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["Silo.app"], "only the app stays staged");
        // The image of 0.1.3 holds 0.1.2: refused, nothing staged.
        let error = prepare(&feed, "0.1.3", &app, &running, None).await.unwrap_err();
        assert!(matches!(&error, Failure::Invalid(e) if e.contains("not 0.1.3")), "{error:?}");
        assert!(!stage_dir(&app).exists());
        // A team-signed app never takes the ad-hoc one.
        let team = (APP_ID.to_owned(), Identity::Team { team: "FRUHQC68JC".into(), authority: "Developer ID Application: X (FRUHQC68JC)".into() });
        let error = prepare(&feed, "0.1.2", &app, &team, None).await.unwrap_err();
        assert!(matches!(&error, Failure::Invalid(e) if e.contains("not as the installed copy")), "{error:?}");
    }
}
