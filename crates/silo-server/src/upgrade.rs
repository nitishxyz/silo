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
    BUILD, Frame, kind,
    release::{download, version_of},
    spawn_reader,
    upgrade::{self, Decision, HostBuild, MAX_BINARY, UpgradeBinary, UpgradeRejected, UpgradeStarted},
    write_frame,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::{Server, auth::Peer};

/// Longest wait for the next frame of a binary.
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);
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

    #[test]
    fn one_upgrade_per_executable() {
        let a = Busy::take(Path::new("/x/silo")).unwrap();
        assert!(Busy::take(Path::new("/x/silo")).is_none());
        assert!(Busy::take(Path::new("/y/silo")).is_some());
        drop(a);
        assert!(Busy::take(Path::new("/x/silo")).is_some());
    }
}
