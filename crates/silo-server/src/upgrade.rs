//! Upgrade streams (see [`silo_proto::upgrade`]): the owner or a paired
//! device replaces this server's executable with another build, which then
//! takes over live through its `silo up`.
//!
//! The new binary is renamed over the executable before the handoff (the
//! service and `silo up` run that path), with the previous one kept as a
//! hard link. If its `silo up` fails while this server still runs, the
//! takeover did not commit: the previous executable is put back and the
//! client told at once. If it commits, this server exits and the client
//! sees the new build come up.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

use sha2::{Digest, Sha256};
use silo_proto::{
    BUILD, Frame, kind, spawn_reader,
    upgrade::{self, Decision, HostBuild, MAX_BINARY, UpgradeBinary, UpgradeRejected, UpgradeStarted},
    write_frame,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{Server, auth::Peer};

/// Longest wait for the next frame of a binary.
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest `<new binary> --version`.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// Most `--version` output read.
const VERSION_MAX: u64 = 4096;
/// Prefix of staged binaries and kept previous executables:
/// `.silo.upgrade.<pid>.<suffix>` next to the executable.
const PREFIX: &str = ".silo.upgrade.";

type Refusal = (&'static str, String);

/// Serve an upgrade stream whose `UPGRADE_HELLO` was read. Remote peers
/// must be paired devices: they already run any program here, so replacing
/// the server grants nothing more.
pub async fn serve<R, W>(server: Arc<Server>, reader: R, mut writer: W, peer: Peer) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    if let Peer::Remote { endpoint_id, .. } = &peer
        && !server.auth().is_paired(endpoint_id)
    {
        return reject(&mut writer, ("unauthorized", "this device is not paired with the host".into())).await;
    }
    let status = HostBuild {
        version: server.info.version.clone(),
        build: BUILD.to_owned(),
        platform: upgrade::platform(),
        sessions: server.registry.lock().unwrap().summaries().into_iter().map(|s| s.id).collect(),
    };
    send(&mut writer, &Frame::json(kind::HOST_BUILD, &status)?).await?;
    let mut frames = spawn_reader(reader, 16);
    // Closing here was a check.
    let Some(Ok(frame)) = frames.recv().await else { return Ok(()) };
    if frame.kind != kind::UPGRADE_BINARY {
        return reject(&mut writer, ("bad_request", format!("unexpected frame {:#x}", frame.kind))).await;
    }
    let binary = match frame.parse::<UpgradeBinary>() {
        Ok(binary) => binary,
        Err(error) => return reject(&mut writer, ("bad_request", error.to_string())).await,
    };
    let target = match target_exe(&server.home) {
        Ok(target) => target,
        Err(refusal) => return reject(&mut writer, refusal).await,
    };
    let Some(_busy) = Busy::take(&target).filter(|_| !server.upgrading.load(Ordering::SeqCst)) else {
        return reject(&mut writer, (upgrade::BUSY, "an upgrade is already in progress".into())).await;
    };
    let installed = match install(&status, &binary, &target, &mut frames).await {
        Ok(installed) => installed,
        Err(refusal) => return reject(&mut writer, refusal).await,
    };
    tracing::info!(version = installed.started.version, build = installed.started.build, "upgrade: installed; handing over");
    send(&mut writer, &Frame::json(kind::UPGRADE_STARTED, &installed.started)?).await?;
    match hand_over(&server.home, &target).await {
        Ok(()) => writer.shutdown().await,
        Err(why) => {
            let restored = std::fs::rename(&installed.previous, &target);
            let reason = match restored {
                Ok(()) => format!("the new build could not take over ({why}); still serving the previous one, see the server log"),
                Err(error) => format!("the new build could not take over ({why}), and restoring {} failed: {error}", target.display()),
            };
            reject(&mut writer, (upgrade::HANDOFF, reason)).await
        }
    }
}

async fn send<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> std::io::Result<()> {
    write_frame(writer, frame).await?;
    writer.flush().await
}

async fn reject<W: AsyncWrite + Unpin>(writer: &mut W, (code, reason): Refusal) -> std::io::Result<()> {
    tracing::info!(code, reason, "upgrade refused");
    send(writer, &Frame::json(kind::UPGRADE_REJECTED, &UpgradeRejected { code: code.into(), reason })?).await?;
    writer.shutdown().await
}

/// Executables being replaced, held from receiving through the handoff.
static BUSY: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

struct Busy(PathBuf);

impl Busy {
    fn take(target: &Path) -> Option<Self> {
        let mut busy = BUSY.lock().unwrap();
        busy.get_or_insert_with(HashSet::new).insert(target.to_owned()).then(|| Self(target.to_owned()))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(busy) = BUSY.lock().unwrap().as_mut() {
            busy.remove(&self.0);
        }
    }
}

/// The executable to replace: the file the service runs (`silo up`
/// records the canonical path, so a symlinked install upgrades its target).
fn target_exe(home: &Path) -> Result<PathBuf, Refusal> {
    let exe = std::env::current_exe().map_err(|e| ("internal", format!("locate the server executable: {e}")))?;
    // Linux names an executable replaced since it started "<path> (deleted)".
    let path = exe.to_str().and_then(|p| p.strip_suffix(" (deleted)")).map_or(exe.clone(), PathBuf::from);
    let target = path
        .canonicalize()
        .map_err(|_| ("install", format!("the server's executable {} was removed since it started; reinstall silo there", path.display())))?;
    let home = home.canonicalize().unwrap_or_else(|_| home.to_owned());
    if crate::bundle::runtime_dir(&target, &home, BUILD).is_some() {
        return Err(("managed", "this server comes with Silo.app; update the app on that machine".into()));
    }
    Ok(target)
}

/// Removes a file on drop unless disarmed.
struct Temp(Option<PathBuf>);

impl Drop for Temp {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct Installed {
    started: UpgradeStarted,
    /// The previous executable, hard-linked aside.
    previous: PathBuf,
}

async fn install(
    status: &HostBuild,
    binary: &UpgradeBinary,
    target: &Path,
    frames: &mut tokio::sync::mpsc::Receiver<std::io::Result<Frame>>,
) -> Result<Installed, Refusal> {
    if binary.size > MAX_BINARY {
        return Err(("too_large", format!("the binary is larger than {MAX_BINARY} bytes")));
    }
    let dir = target.parent().ok_or_else(|| ("internal", "the server executable has no directory".to_owned()))?;
    let pid = std::process::id();
    let mut staged = Temp(Some(dir.join(format!("{PREFIX}{pid}.{:x}", rand::random::<u32>()))));
    let path = staged.0.clone().expect("staged");
    match &binary.release {
        Some(version) => download(&path, std::env::var("SILO_DOWNLOAD_BASE").ok().as_deref(), version, &status.platform).await?,
        None => receive(&path, binary, frames).await?,
    }
    let (version, build) = version_of(&path).await?;
    match upgrade::decide((&status.version, Some(&status.build)), (&version, &build), binary.force) {
        Decision::Upgrade => {}
        Decision::UpToDate => return Err((upgrade::UP_TO_DATE, format!("already runs {}", upgrade::version_text(&version, &build)))),
        Decision::Newer => return Err((upgrade::NEWER, format!("runs {}, newer than {version}", status.version))),
    }
    let cannot = |e: std::io::Error| ("install", format!("cannot replace {}: {e}", target.display()));
    let previous = dir.join(format!("{PREFIX}{pid}.previous"));
    let _ = std::fs::remove_file(&previous);
    std::fs::hard_link(target, &previous).map_err(cannot)?;
    let mut kept = Temp(Some(previous.clone()));
    std::fs::rename(&path, target).map_err(cannot)?;
    staged.0 = None;
    kept.0 = None;
    Ok(Installed { started: UpgradeStarted { version, build }, previous })
}

/// Write exactly `binary.size` bytes from `UPGRADE_CHUNK` frames to `path`
/// (mode 0755) and check their hash.
async fn receive(path: &Path, binary: &UpgradeBinary, frames: &mut tokio::sync::mpsc::Receiver<std::io::Result<Frame>>) -> Result<(), Refusal> {
    let io = |error: std::io::Error| ("internal", error.to_string());
    let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).mode(0o755).open(path).await.map_err(io)?;
    let mut hash = Sha256::new();
    let mut received = 0u64;
    while received < binary.size {
        let frame = match tokio::time::timeout(FRAME_TIMEOUT, frames.recv()).await {
            Ok(Some(Ok(frame))) if frame.kind == kind::UPGRADE_CHUNK => frame,
            Ok(Some(Ok(frame))) => return Err(("bad_request", format!("unexpected frame {:#x}", frame.kind))),
            Ok(_) => return Err(("truncated", "the stream ended before the whole binary arrived".into())),
            Err(_) => return Err(("timeout", "the binary stopped arriving".into())),
        };
        received += frame.payload.len() as u64;
        if received > binary.size {
            return Err(("bad_request", "more bytes than announced".into()));
        }
        hash.update(&frame.payload);
        file.write_all(&frame.payload).await.map_err(io)?;
    }
    file.sync_all().await.map_err(io)?;
    // The umask may have narrowed the mode; installed binaries are 0755.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).map_err(io)?;
    let actual: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != binary.sha256 {
        return Err(("checksum", "the binary arrived corrupted (sha256 mismatch)".into()));
    }
    Ok(())
}

/// Longest download of one release file.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Fetch release `version` for `platform` into `path` (mode 0755): its
/// asset, checked against the release's checksums, unpacked. Uses `curl`
/// and `tar` like install.sh; `base` (`SILO_DOWNLOAD_BASE`) overrides the
/// location.
async fn download(path: &Path, base: Option<&str>, version: &str, platform: &str) -> Result<(), Refusal> {
    let url = |file: &str| upgrade::release_url(base, version, file).ok_or_else(|| ("bad_request", format!("{version:?} is not a release version")));
    let (sums_url, asset) = (url(upgrade::CHECKSUMS)?, upgrade::asset(platform));
    let asset_url = url(&asset)?;
    // Only the default (GitHub) location must be https; an override may be
    // a mirror or a test server.
    let https = base.is_none_or(str::is_empty);
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| ("internal", "bad staging path".to_owned()))?;
    let unpack = path.with_file_name(format!("{name}.d"));
    std::fs::create_dir(&unpack).map_err(|e| ("internal", e.to_string()))?;
    let _unpack = TempDir(unpack.clone());
    let not_published = || (upgrade::NOT_PUBLISHED, upgrade::not_published(version, platform));
    let sums = unpack.join(upgrade::CHECKSUMS);
    fetch(&sums_url, &sums, https).await?.ok_or_else(not_published)?;
    let sums = std::fs::read_to_string(&sums).map_err(|e| ("internal", e.to_string()))?;
    let expected = upgrade::checksum(&sums, &asset).ok_or_else(not_published)?.to_ascii_lowercase();
    let archive = unpack.join(&asset);
    fetch(&asset_url, &archive, https).await?.ok_or_else(not_published)?;
    let actual = sha256_file(&archive).map_err(|e| ("internal", e.to_string()))?;
    if actual != expected {
        return Err(("checksum", format!("{asset} does not match the release's {}", upgrade::CHECKSUMS)));
    }
    let tar = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&unpack)
        .arg("silo")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| ("download", format!("run tar: {e}")))?;
    if !tar.status.success() {
        return Err(("download", format!("unpack {asset}: {}", String::from_utf8_lossy(&tar.stderr).trim())));
    }
    let member = unpack.join("silo");
    // A plain file of its own: a symlink or hard link member could point
    // anywhere (set_permissions and rename would act on its target).
    let plain = std::fs::symlink_metadata(&member).is_ok_and(|m| m.file_type().is_file() && std::os::unix::fs::MetadataExt::nlink(&m) == 1);
    if !plain {
        return Err(("invalid", format!("{asset} does not hold a plain `silo` file")));
    }
    std::fs::rename(&member, path).map_err(|e| ("internal", e.to_string()))?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).map_err(|e| ("internal", e.to_string()))
}

/// Download `url` to `dest` (at most [`MAX_BINARY`] bytes; over https only
/// when `https`, redirects included); `None` when the server has no such
/// file.
async fn fetch(url: &str, dest: &Path, https: bool) -> Result<Option<()>, Refusal> {
    tracing::info!(url, "upgrade: downloading");
    let mut curl = tokio::process::Command::new("curl");
    curl.args(["-fsSL", "--retry", "3", "--max-filesize", &MAX_BINARY.to_string()]);
    if https {
        curl.args(["--proto", "=https", "--proto-redir", "=https"]);
    }
    let run = curl.arg("-o").arg(dest).arg(url).stdin(std::process::Stdio::null()).kill_on_drop(true).output();
    let out = match tokio::time::timeout(DOWNLOAD_TIMEOUT, run).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(("download", "the host needs curl to download releases".into())),
        Ok(Err(e)) => return Err(("download", format!("run curl: {e}"))),
        Err(_) => return Err(("download", format!("downloading {url} timed out"))),
    };
    match out.status.code() {
        Some(0) => Ok(Some(())),
        // HTTP error (404 for a missing asset or release).
        Some(22) => Ok(None),
        Some(63) => Err(("too_large", format!("{url} is larger than {MAX_BINARY} bytes"))),
        _ => Err(("download", format!("download {url}: {}", String::from_utf8_lossy(&out.stderr).trim()))),
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    std::io::copy(&mut std::fs::File::open(path)?, &mut hash)?;
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Removes a directory tree on drop.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Version and build reported by `path --version`: proves it runs here.
/// Runs in its own process group, killed whole if it overstays; at most
/// [`VERSION_MAX`] bytes of output are read.
async fn version_of(path: &Path) -> Result<(String, String), Refusal> {
    let not_silo = || ("invalid", "the binary is not a silo for this host's platform".to_owned());
    let mut child = tokio::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| not_silo())?;
    let group = child.id().and_then(|id| rustix::process::Pid::from_raw(id.cast_signed()));
    let stdout = child.stdout.take().expect("piped");
    let run = async {
        let mut out = Vec::new();
        stdout.take(VERSION_MAX).read_to_end(&mut out).await?;
        std::io::Result::Ok((child.wait().await?, out))
    };
    let (status, out) = match tokio::time::timeout(VERSION_TIMEOUT, run).await {
        Ok(Ok(done)) => done,
        Ok(Err(_)) => return Err(not_silo()),
        Err(_) => {
            if let Some(group) = group {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
            return Err(("invalid", "the binary's `--version` did not finish".into()));
        }
    };
    match upgrade::parse_version(&String::from_utf8_lossy(&out)) {
        Some((version, Some(build))) if status.success() => Ok((version, build)),
        Some((_, None)) => Err(("invalid", "the binary predates remote upgrade (it reports no build)".into())),
        _ => Err(not_silo()),
    }
}

/// Run `exe up --no-offer`: it takes over from this server live (and
/// updates its service) like a local `silo up`, and wait for it.
/// In its own process group so a service manager stopping this server's
/// job spares it.
async fn hand_over(home: &Path, exe: &Path) -> Result<(), String> {
    let log = std::fs::OpenOptions::new().create(true).append(true).open(silo_proto::paths::log(home)).map_err(|e| e.to_string())?;
    let status = tokio::process::Command::new(exe)
        .args(["up", "--no-offer"])
        .env("SILO_HOME", home)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .process_group(0)
        .status()
        .await
        .map_err(|e| format!("start `silo up`: {e}"))?;
    // Success means this build's server answers: the takeover committed (this
    // server is exiting) or, reinstalling this same build, none was needed.
    if status.success() { Ok(()) } else { Err(format!("`silo up` failed: {status}")) }
}

/// Remove staged binaries and kept executables left next to this server's
/// executable by processes that are gone.
pub fn clean_stale_beside_exe() {
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()).and_then(|e| e.parent().map(Path::to_owned)) {
        clean_stale(&dir);
    }
}

fn clean_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        let removed = || if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        if stale_pid(&path).is_some_and(|pid| !alive(pid)) && removed().is_ok() {
            tracing::info!("upgrade: removed {}", path.display());
        }
    }
}

/// The pid in `.silo.upgrade.<pid>.<suffix>`.
fn stale_pid(path: &Path) -> Option<i32> {
    path.file_name()?.to_str()?.strip_prefix(PREFIX)?.split('.').next()?.parse().ok()
}

fn alive(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid).is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_files() {
        assert_eq!(stale_pid(Path::new("/x/.silo.upgrade.123.previous")), Some(123));
        assert_eq!(stale_pid(Path::new("/x/.silo.upgrade.9.1f")), Some(9));
        assert_eq!(stale_pid(Path::new("/x/silo")), None);
        let dir = std::env::temp_dir().join(format!("silo-upgrade-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ours = dir.join(format!("{PREFIX}{}.previous", std::process::id()));
        let gone = dir.join(format!("{PREFIX}{}.1", i32::MAX));
        std::fs::write(&ours, "").unwrap();
        std::fs::write(&gone, "").unwrap();
        clean_stale(&dir);
        assert!(ours.exists());
        assert!(!gone.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn version_probe() {
        let dir = std::env::temp_dir().join(format!("silo-upgrade-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            path
        };
        let ok = script("ok", "echo 'silo 1.2.3 (abc)'");
        assert_eq!(version_of(&ok).await.unwrap(), ("1.2.3".into(), "abc".into()));
        let old = script("old", "echo 'silo 1.2.3'");
        assert!(version_of(&old).await.unwrap_err().1.contains("predates"));
        let failing = script("failing", "echo 'silo 1.2.3 (abc)'; exit 1");
        assert_eq!(version_of(&failing).await.unwrap_err().0, "invalid");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A minimal HTTP/1.0 server for `files`; anything else is a 404.
    async fn serve_files(files: std::collections::HashMap<String, Vec<u8>>) -> String {
        use tokio::io::AsyncReadExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let files = files.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let path = String::from_utf8_lossy(&request).split_whitespace().nth(1).unwrap_or("").to_owned();
                    let reply = match files.get(path.trim_start_matches("/r/")) {
                        Some(body) => [format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes(), body.clone()].concat(),
                        None => b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    };
                    let _ = socket.write_all(&reply).await;
                });
            }
        });
        format!("http://{addr}/r")
    }

    #[tokio::test]
    async fn downloads_a_checked_release() {
        let dir = std::env::temp_dir().join(format!("silo-upgrade-download-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pack")).unwrap();
        std::fs::write(dir.join("pack/silo"), "#!/bin/sh\necho 'silo 9.9.9 (rel)'\n").unwrap();
        let archive = dir.join("a.tar.gz");
        let packed = std::process::Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(dir.join("pack")).arg("silo").status().unwrap();
        assert!(packed.success());
        let tarball = std::fs::read(&archive).unwrap();
        let sha = |bytes: &[u8]| -> String { Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect() };
        // `silo` as a symlink to a file outside the unpack directory.
        std::fs::create_dir_all(dir.join("evil")).unwrap();
        std::os::unix::fs::symlink(dir.join("pack/silo"), dir.join("evil/silo")).unwrap();
        let evil_archive = dir.join("evil.tar.gz");
        let packed = std::process::Command::new("tar").arg("-czf").arg(&evil_archive).arg("-C").arg(dir.join("evil")).arg("silo").status().unwrap();
        assert!(packed.success());
        let evil = std::fs::read(&evil_archive).unwrap();
        let asset = upgrade::asset("linux-x86_64");
        let sums = format!("{}  {asset}\n{}  silo-macos-aarch64.tar.gz\n{}  silo-linux-riscv64.tar.gz\n", sha(&tarball), "0".repeat(64), sha(&evil));
        let files = std::collections::HashMap::from([
            (upgrade::CHECKSUMS.to_owned(), sums.into_bytes()),
            (asset.clone(), tarball),
            ("silo-macos-aarch64.tar.gz".to_owned(), b"corrupt".to_vec()),
            ("silo-linux-riscv64.tar.gz".to_owned(), evil),
        ]);
        let base = serve_files(files).await;
        let path = dir.join(format!("{PREFIX}1.aa"));
        download(&path, Some(&base), "9.9.9", "linux-x86_64").await.unwrap();
        assert_eq!(version_of(&path).await.unwrap(), ("9.9.9".into(), "rel".into()));
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions()) & 0o777, 0o755);
        std::fs::remove_file(&path).unwrap();
        // Not in the checksums: not published.
        let error = download(&path, Some(&base), "9.9.9", "linux-aarch64").await.unwrap_err();
        assert_eq!(error, (upgrade::NOT_PUBLISHED, "silo 9.9.9 isn't published for linux-aarch64; use --local or --binary".into()));
        // A missing release (404 for SHA256SUMS): not published either.
        let error = download(&path, Some(&format!("{base}/nope")), "9.9.9", "linux-x86_64").await.unwrap_err();
        assert_eq!(error.0, upgrade::NOT_PUBLISHED);
        let error = download(&path, Some(&base), "9.9.9", "macos-aarch64").await.unwrap_err();
        assert_eq!(error.0, "checksum");
        // A symlink member is refused and its target left alone.
        let mode = |p: &Path| std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(p).unwrap().permissions()) & 0o777;
        let before = mode(&dir.join("pack/silo"));
        let error = download(&path, Some(&base), "9.9.9", "linux-riscv64").await.unwrap_err();
        assert_eq!(error.0, "invalid", "{error:?}");
        assert_eq!(mode(&dir.join("pack/silo")), before);
        // A version that is no release version never reaches a URL.
        let error = download(&path, None, "0.1.0/../../../../x/y/releases/download/v1", "linux-x86_64").await.unwrap_err();
        assert_eq!(error.0, "bad_request");
        assert!(!path.exists());
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left.len(), 4, "only the fixtures remain: {left:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_upgrade_per_executable() {
        let a = Busy::take(Path::new("/x/silo")).unwrap();
        assert!(Busy::take(Path::new("/x/silo")).is_none());
        assert!(Busy::take(Path::new("/y/silo")).is_some());
        drop(a);
        assert!(Busy::take(Path::new("/x/silo")).is_some());
    }
}
