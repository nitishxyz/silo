//! Self-update of a release Silo.app, without the UI (that is
//! `updater.rs`).
//!
//! - Check: one request for the latest published release (GitHub's
//!   `releases/latest`: neither draft nor pre-release); newer means a
//!   higher `major.minor.patch` than this build's.
//! - Download: the release's `SHA256SUMS` and `Silo-<version>.dmg` into a
//!   staging directory next to the app (`.silo-update.<pid>`, so the final
//!   rename stays on one volume), the image checked against the sums.
//! - Verify: the image is mounted read-only and hidden, and its Silo.app
//!   must pass `codesign --verify --deep --strict`, carry this release's
//!   version and be signed like the running app ([`same_identity`]); it is
//!   then copied into the staging directory and checked again.
//! - Replace: the staged app and the running one swap names in one atomic
//!   rename (`RENAME_SWAP`), so the destination always holds a whole app.
//!   The old app is left in the staging directory, removed by the next
//!   launch ([`clean_stale`]); a waiter opens the new app once this
//!   process has exited. Its launch runs `silo up` as always, which takes
//!   the local server over to the new build live.
//!
//! The identity rule: a Developer ID app updates only to an app of the
//! same team and signing certificate name, also checked as a code
//! requirement anchored at Apple's CA; an ad-hoc signed app only to an
//! ad-hoc signed one (a local build, for tests: no published release is
//! ad hoc). Anything unsigned, of another team or of the other kind is
//! refused, and both must be `xyz.nitish.silo`.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

use silo_proto::{release, upgrade};

/// Largest app image downloaded.
const MAX_IMAGE: u64 = 1024 * 1024 * 1024;
/// Largest latest-release description read.
const MAX_LATEST: u64 = 1024 * 1024;
/// Longest run of `hdiutil`, `codesign` or `ditto`.
const TOOL_TIMEOUT: Duration = Duration::from_secs(300);
/// Staging directories next to the app: `.silo-update.<pid>`.
const STAGE_PREFIX: &str = ".silo-update.";

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
        let local = ["http://127.0.0.1:", "http://localhost:", "file:///"].iter().any(|p| base.starts_with(p));
        local.then(|| Self { latest: format!("{base}/latest.json"), assets: Some(base.to_owned()), https: false })
    }

    /// GitHub, or `SILO_UPDATE_FEED` (tests only: also lets a release app
    /// outside (~)/Applications, with its own state directory, update).
    /// `None` when that is set but not a local feed: no updates.
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

/// Whether `candidate` is a higher version than `running`.
#[must_use]
pub fn is_newer(running: &str, candidate: &str) -> bool {
    upgrade::compare_versions(running, candidate) == Some(std::cmp::Ordering::Less)
}

/// How an app is signed.
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

/// Bundle identifier and identity from `codesign -dvv` output; `None`
/// when unsigned or neither ad hoc nor of a team.
#[must_use]
pub fn parse_signature(text: &str) -> Option<(String, Identity)> {
    let field = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).map(str::trim);
    let identifier = field("Identifier")?.to_owned();
    if field("Signature") == Some("adhoc") {
        return Some((identifier, Identity::AdHoc));
    }
    let team = field("TeamIdentifier").filter(|t| *t != "not set" && !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric()))?;
    let authority = field("Authority")?;
    Some((identifier, Identity::Team { team: team.to_owned(), authority: authority.to_owned() }))
}

/// Whether an app signed as `candidate` may replace one signed as
/// `running` (the module's identity rule).
///
/// # Errors
/// Why not, for the log.
pub fn same_identity(running: &(String, Identity), candidate: &(String, Identity)) -> Result<(), String> {
    if running.0 != crate::conn::BUNDLE_ID || candidate.0 != crate::conn::BUNDLE_ID {
        return Err(format!("bundle identifier {} replacing {}", candidate.0, running.0));
    }
    if running.1 != candidate.1 {
        return Err(format!("signed as {:?}, not as the running app ({:?})", candidate.1, running.1));
    }
    Ok(())
}

/// The code requirement a team's app must satisfy: Apple-issued, this
/// bundle, that team.
fn requirement(team: &str) -> String {
    format!("anchor apple generic and identifier \"{}\" and certificate leaf[subject.OU] = \"{team}\"", crate::conn::BUNDLE_ID)
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

/// How `app` is signed.
///
/// # Errors
/// `codesign` fails, or the app is unsigned.
pub async fn signature(app: &Path) -> Result<(String, Identity), String> {
    let out = check("codesign", &["-dvv".as_ref(), app.as_os_str()]).await?;
    // codesign describes the signature on stderr.
    parse_signature(&String::from_utf8_lossy(&out.stderr)).ok_or_else(|| format!("{} is not signed", app.display()))
}

/// `app` is intact, signed like `running` and Silo `version`.
async fn verify(app: &Path, version: &str, running: &(String, Identity)) -> Result<(), String> {
    let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).map_err(|e| format!("read Info.plist: {e}"))?;
    let found = crate::conn::plist_string(&plist, "CFBundleShortVersionString");
    if found != Some(version) {
        return Err(format!("the image holds Silo {found:?}, not {version}"));
    }
    let candidate = signature(app).await?;
    same_identity(running, &candidate)?;
    let requirement = match &candidate.1 {
        Identity::Team { team, .. } => Some(requirement(team)),
        Identity::AdHoc => None,
    };
    let mut args: Vec<&OsStr> = vec!["--verify".as_ref(), "--deep".as_ref(), "--strict".as_ref()];
    if let Some(requirement) = &requirement {
        args.extend([OsStr::new("-R"), OsStr::new(requirement)]);
    }
    args.push(app.as_os_str());
    check("codesign", &args).await.map(drop)
}

/// The newer release than `running` published on `feed`, if any.
///
/// # Errors
/// The feed cannot be read.
pub async fn check_latest(feed: &Feed, running: &str, dir: &Path) -> Result<Option<String>, String> {
    let file = dir.join(format!("silo-latest.{}.json", std::process::id()));
    let fetched = release::fetch(&feed.latest, &file, feed.https, MAX_LATEST).await;
    let body = std::fs::read(&file);
    let _ = std::fs::remove_file(&file);
    fetched.map_err(|(_, e)| e)?.ok_or("no published release")?;
    let version = latest_version(&body.map_err(|e| e.to_string())?).ok_or("the latest release has no version")?;
    Ok(is_newer(running, &version).then_some(version))
}

/// Why an update could not be prepared.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// Try again at the next check.
    Retry(String),
    /// The app's folder is not writable: it cannot be replaced.
    Unwritable,
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
/// the release's checksums.
///
/// # Errors
/// Not published, not downloadable or not matching.
pub async fn download(feed: &Feed, version: &str, dir: &Path) -> Result<PathBuf, String> {
    let image = upgrade::app_image(version);
    let sums = dir.join(upgrade::CHECKSUMS);
    release::fetch(&feed.asset(version, upgrade::CHECKSUMS)?, &sums, feed.https, MAX_LATEST).await.map_err(|(_, e)| e)?.ok_or("no checksums")?;
    let sums = std::fs::read_to_string(&sums).map_err(|e| e.to_string())?;
    let expected = upgrade::checksum(&sums, &image).ok_or_else(|| format!("{image} is not in the checksums"))?.to_ascii_lowercase();
    let path = dir.join(&image);
    release::fetch(&feed.asset(version, &image)?, &path, feed.https, MAX_IMAGE)
        .await
        .map_err(|(_, e)| e)?
        .ok_or_else(|| format!("{image} is not published"))?;
    if release::sha256_file(&path).map_err(|e| e.to_string())? != expected {
        let _ = std::fs::remove_file(&path);
        return Err(format!("{image} does not match the release's {}", upgrade::CHECKSUMS));
    }
    Ok(path)
}

/// Mount `image`, check its Silo.app and copy it to `dest`.
async fn unpack(image: &Path, version: &str, running: &(String, Identity), dest: &Path) -> Result<(), String> {
    let mount = std::env::temp_dir().join(format!("silo-update.{}.mnt", std::process::id()));
    let _ = std::fs::create_dir_all(&mount);
    let attach = ["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"].map(OsStr::new);
    check("hdiutil", &[&attach[..], &[mount.as_os_str(), image.as_os_str()]].concat()).await?;
    let app = mount.join("Silo.app");
    let copied = async {
        verify(&app, version, running).await?;
        check("ditto", &[app.as_os_str(), dest.as_os_str()]).await.map(drop)
    }
    .await;
    if let Err(error) = check("hdiutil", &["detach".as_ref(), mount.as_os_str(), "-force".as_ref()]).await {
        tracing::warn!("update: {error}");
    }
    let _ = std::fs::remove_dir(&mount);
    copied
}

/// Download, check and stage release `version` next to `app`; the
/// staged app.
///
/// # Errors
/// [`Failure`].
pub async fn prepare(feed: &Feed, version: &str, app: &Path, running: &(String, Identity)) -> Result<PathBuf, Failure> {
    let stage = fresh_stage(app)?;
    let staged = stage.join("Silo.app");
    let result = async {
        let image = download(feed, version, &stage).await?;
        let unpacked = unpack(&image, version, running, &staged).await;
        let _ = std::fs::remove_file(&image);
        let _ = std::fs::remove_file(stage.join(upgrade::CHECKSUMS));
        unpacked?;
        verify(&staged, version, running).await
    }
    .await;
    match result {
        Ok(()) => Ok(staged),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&stage);
            Err(Failure::Retry(error))
        }
    }
}

/// Swap `staged` and `app` in one rename: `app` becomes the new app and
/// `staged` holds the old one.
///
/// # Errors
/// The rename fails (the folder is not writable, `staged` is gone).
pub fn install(staged: &Path, app: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
    let (from, to) = (c(staged)?, c(app)?);
    // SAFETY: both are valid NUL-terminated paths.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
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
/// left by processes that are gone.
pub fn clean_stale(app: &Path) {
    let Some(dir) = app.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        let pid = path.file_name().and_then(|n| n.to_str()?.strip_prefix(STAGE_PREFIX)?.parse::<i32>().ok());
        if pid.is_some_and(|pid| !alive(pid)) && std::fs::remove_dir_all(&path).is_ok() {
            tracing::info!("update: removed {}", path.display());
        }
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("silo-update-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
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
        for remote in ["https://example.com/feed", "http://10.0.0.1:80/x", "http://127.0.0.1.evil.com/x", "file://host/x"] {
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
        assert_eq!(
            requirement("FRUHQC68JC"),
            r#"anchor apple generic and identifier "xyz.nitish.silo" and certificate leaf[subject.OU] = "FRUHQC68JC""#
        );
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
        let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).unwrap();
        crate::conn::plist_string(&plist, "CFBundleShortVersionString").unwrap().to_owned()
    }

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
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unwritable_folder_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("unwritable");
        let app = dir.join("Silo.app");
        bundle(&app, "0.1.1");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let staged = fresh_stage(&app);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(staged, Err(Failure::Unwritable));
        std::fs::remove_dir_all(dir).unwrap();
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
        let path = temp("sha").join("bytes");
        std::fs::write(&path, bytes).unwrap();
        let sha = release::sha256_file(&path).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        sha
    }

    #[tokio::test]
    async fn downloads_a_checked_image() {
        let dir = temp("download");
        let sums = format!("{}  Silo-0.1.2.dmg\n{}  Silo-0.1.3.dmg\n", sha(b"image"), sha(b"other"));
        let feed =
            feed(&dir, &[("SHA256SUMS", sums.as_bytes()), ("Silo-0.1.2.dmg", b"image"), ("Silo-0.1.3.dmg", b"tampered"), ("Silo-0.1.4.dmg", b"x")]);
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();
        let image = download(&feed, "0.1.2", &out).await.unwrap();
        assert_eq!(std::fs::read(&image).unwrap(), b"image");
        assert!(download(&feed, "0.1.3", &out).await.unwrap_err().contains("does not match"));
        assert!(!out.join("Silo-0.1.3.dmg").exists(), "a mismatch is not kept");
        assert!(download(&feed, "0.1.4", &out).await.unwrap_err().contains("not in the checksums"));
        std::fs::write(dir.join("feed/latest.json"), br#"{"tag_name":"v0.1.2"}"#).unwrap();
        assert_eq!(check_latest(&feed, "0.1.1", &out).await.unwrap().as_deref(), Some("0.1.2"));
        assert_eq!(check_latest(&feed, "0.1.2", &out).await.unwrap(), None);
        std::fs::remove_file(dir.join("feed/latest.json")).unwrap();
        assert!(check_latest(&feed, "0.1.1", &out).await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The whole path on a real disk image of ad-hoc signed bundles.
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
        let staged = prepare(&feed, "0.1.2", &app, &running).await.unwrap();
        assert_eq!(staged, stage_dir(&app).join("Silo.app"));
        assert_eq!(version_of(&staged), "0.1.2");
        let left: Vec<_> = std::fs::read_dir(stage_dir(&app)).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["Silo.app"], "only the app stays staged");
        // The image of 0.1.3 holds 0.1.2: refused, nothing staged.
        let error = prepare(&feed, "0.1.3", &app, &running).await.unwrap_err();
        assert!(matches!(&error, Failure::Retry(e) if e.contains("not 0.1.3")), "{error:?}");
        assert!(!stage_dir(&app).exists());
        // A team-signed app never takes the ad-hoc one.
        let team = (
            crate::conn::BUNDLE_ID.to_owned(),
            Identity::Team { team: "FRUHQC68JC".into(), authority: "Developer ID Application: X (FRUHQC68JC)".into() },
        );
        let error = prepare(&feed, "0.1.2", &app, &team).await.unwrap_err();
        assert!(matches!(&error, Failure::Retry(e) if e.contains("not as the running app")), "{error:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
