//! A server of a different build holds the state directory. It is
//! described for the user and can be stopped, never joined by a second
//! server. Requests to it are best-effort: request shapes are only
//! guaranteed between identical builds.

use std::{
    os::fd::AsFd,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, bail, ensure};
use silo_proto::{control::Method, paths};

use crate::{Control, control::IncompatibleProtocol};

const NAME: &str = "silo (different build)";

/// A server of a different build owns the state directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncompatibleServer {
    /// Its build, pid and version from `runtime.json`.
    pub build: Option<String>,
    pub pid: Option<u32>,
    pub version: Option<String>,
    /// Its running sessions, when it could be asked.
    pub sessions: Option<usize>,
}

impl IncompatibleServer {
    /// Short status, e.g. for a menu line.
    pub const LABEL: &str = "Incompatible server";

    /// E.g. "An incompatible Silo server is running (3 sessions)."
    #[must_use]
    pub fn headline(&self) -> String {
        match self.sessions {
            Some(1) => "An incompatible Silo server is running (1 session).".into(),
            Some(n) => format!("An incompatible Silo server is running ({n} sessions)."),
            None => "An incompatible Silo server is running.".into(),
        }
    }

    /// The headline and what to do.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{} Upgrade it to use this build.", self.headline())
    }

    /// What [`upgrade`] costs, in one line.
    pub const UPGRADE_NOTE: &str = "Sessions keep running; a very old server's sessions end.";
}

impl std::fmt::Display for IncompatibleServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.headline())
    }
}

impl std::error::Error for IncompatibleServer {}

/// Whether `error` is a build rejection.
#[must_use]
pub fn is_mismatch(error: &anyhow::Error) -> bool {
    error.downcast_ref::<IncompatibleProtocol>().is_some()
}

/// The server of another build for `home`, if `error` (from a local
/// connect) is a build rejection and a server holds the state directory.
pub async fn incompatible_server(home: &Path, error: &anyhow::Error) -> Option<IncompatibleServer> {
    if is_mismatch(error) && server_lock_held(home) { Some(inspect(home).await) } else { None }
}

/// Describe the server that rejected our build: `runtime.json`, plus a
/// best-effort session count.
pub async fn inspect(home: &Path) -> IncompatibleServer {
    let runtime = read_runtime(home);
    let sessions = match runtime.as_ref().and_then(|r| r.build.as_deref()) {
        Some(build) => count_sessions(home, build).await,
        None => None,
    };
    IncompatibleServer {
        build: runtime.as_ref().and_then(|r| r.build.clone()),
        pid: runtime.as_ref().map(|r| r.pid),
        version: runtime.map(|r| r.version),
        sessions,
    }
}

async fn connect(home: &Path, build: &str) -> Option<Control> {
    let socket = paths::socket(home);
    let connected = Control::connect_build(silo_transport::Conn::Unix(socket.clone()), NAME, true, build);
    Some(tokio::time::timeout(Duration::from_secs(2), connected).await.ok()?.ok()?.0)
}

async fn count_sessions(home: &Path, build: &str) -> Option<usize> {
    let control = connect(home, build).await?;
    let listed = tokio::time::timeout(Duration::from_secs(2), control.call_value(Method::SessionList {})).await;
    listed.ok()?.ok()?.as_array().map(Vec::len)
}

/// What describing and stopping another build's server needs from its
/// `runtime.json`. Builds before the build handshake wrote no `build`.
#[derive(serde::Deserialize)]
struct Holder {
    pid: u32,
    socket: std::path::PathBuf,
    version: String,
    build: Option<String>,
}

fn read_runtime(home: &Path) -> Option<Holder> {
    serde_json::from_slice(&std::fs::read(paths::runtime(home)).ok()?).ok()
}

/// Whether some server process holds `home`'s server lock (running,
/// starting, or not answering). Spawning another one would only fail.
#[must_use]
pub fn server_lock_held(home: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(paths::server_lock(home)) else {
        return false;
    };
    if rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        return true;
    }
    // Unlock, not only close: a child forked meanwhile by another thread
    // shares this open file description until it execs, and would hold
    // the lock that long (a server starting then finds it taken).
    let _ = rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::Unlock);
    false
}

/// Stop the different-build server: best-effort `server.shutdown`, else
/// SIGTERM to the pid in `runtime.json` once that is confirmed to be a silo
/// server for `home`. Never SIGKILL. Returns once the server lock is free.
/// Ends its sessions; used to quit. [`upgrade`] keeps them.
///
/// # Errors
/// The server could not be asked or signalled, or did not exit in time.
pub async fn stop(home: &Path, server: &IncompatibleServer) -> anyhow::Result<()> {
    let asked = match &server.build {
        Some(build) => ask_shutdown(home, build).await,
        None => false,
    };
    if !asked {
        terminate(home, server.pid)?;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while server_lock_held(home) {
        if Instant::now() >= deadline {
            bail!("the other Silo server build did not stop");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Replace the different-build server with this build's `server_exe`:
/// live where it can (`server --takeover` takes every session, programs
/// included), else, for a server from before live upgrade, [`stop`] it
/// (its sessions end); the caller then starts this build's. On failure
/// the old server keeps serving.
///
/// # Errors
/// The successor could not take over, the old server could not be
/// stopped, or it is of the other channel ([`silo_proto::OtherChannel`]:
/// neither taken over nor stopped).
pub async fn upgrade(home: &Path, server_exe: &Path, server: &IncompatibleServer) -> anyhow::Result<Upgraded> {
    paths::check_runtime_channel(home)?;
    match take_over(home, server_exe, server).await {
        Ok(()) => Ok(Upgraded::Live),
        Err(error) if error.is::<NotUpgradable>() => {
            tracing::info!(pid = ?server.pid, "the server predates live upgrade; stopping it");
            stop(home, server).await?;
            Ok(Upgraded::Stopped)
        }
        Err(error) => Err(error),
    }
}

/// How [`upgrade`] replaced the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upgraded {
    /// Taken over live; every session kept running.
    Live,
    /// It predated live upgrade: stopped (its sessions ended).
    Stopped,
}

/// The live takeover; [`NotUpgradable`] if the server predates it.
async fn take_over(home: &Path, server_exe: &Path, server: &IncompatibleServer) -> anyhow::Result<()> {
    let mut child = crate::spawn::spawn_server(home, server_exe, &["--takeover", "--idle-exit", "0"])?;
    let socket = paths::socket(home);
    let deadline = Instant::now() + UPGRADE_TIMEOUT;
    let failed = |why: String| {
        tracing::warn!("upgrade {server}: {why}; see {}", paths::log(home).display());
        anyhow::anyhow!("the new Silo server could not take over")
    };
    loop {
        // Connections wait in the shared backlog during the handoff.
        let connected = tokio::time::timeout(Duration::from_secs(1), Control::connect_local(&socket, NAME, true)).await;
        if let Ok(Ok(_)) = connected {
            // The successor outlives us; reap it whenever it exits.
            std::thread::spawn(move || child.wait());
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            if status.code() == Some(silo_proto::EXIT_NOT_UPGRADABLE) {
                return Err(NotUpgradable.into());
            }
            return Err(failed(format!("successor exited ({status})")));
        }
        if Instant::now() >= deadline {
            std::thread::spawn(move || child.wait());
            return Err(failed("timed out".into()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The running server predates live upgrade: it closed on the upgrade
/// request and keeps serving. Only a restart ([`stop`]) replaces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NotUpgradable;

impl std::fmt::Display for NotUpgradable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the server predates live upgrade")
    }
}

impl std::error::Error for NotUpgradable {}

/// Generous: the successor restores every block's snapshot first.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(60);

async fn ask_shutdown(home: &Path, build: &str) -> bool {
    let Some(control) = connect(home, build).await else {
        return false;
    };
    // Stopping was decided already: without `force`, a server enforcing
    // the close guard for this process would only be sent SIGTERM next.
    let asked = tokio::time::timeout(Duration::from_secs(2), control.call_value(Method::ServerShutdown { force: true })).await;
    if !matches!(asked, Ok(Ok(_))) {
        return false;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), control.closed()).await;
    true
}

fn terminate(home: &Path, expected: Option<u32>) -> anyhow::Result<()> {
    let runtime = read_runtime(home).context("the running server left no runtime.json")?;
    ensure!(expected.is_none_or(|pid| pid == runtime.pid), "the server changed while stopping it");
    ensure!(runtime.socket == paths::socket(home), "runtime.json belongs to another state directory");
    ensure!(server_lock_held(home), "no server holds the state directory");
    ensure!(is_silo_process(runtime.pid), "pid {} is not a silo server", runtime.pid);
    let pid = i32::try_from(runtime.pid).ok().and_then(rustix::process::Pid::from_raw).context("bad pid")?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).context("signal the other server")?;
    Ok(())
}

fn is_silo_process(pid: u32) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .is_ok_and(|o| o.status.success() && is_silo_command(String::from_utf8_lossy(&o.stdout).trim()))
}

/// `ps -o comm=` output of a silo server: `silo` (CLI) or `silo-server`
/// (app bundle), possibly with a path.
fn is_silo_command(comm: &str) -> bool {
    Path::new(comm).file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "silo" || n == "silo-server")
}

#[cfg(test)]
mod tests {
    use super::*;

    const OTHER: &str = "0123456789abcdef";

    fn server(sessions: Option<usize>) -> IncompatibleServer {
        IncompatibleServer { build: Some(OTHER.into()), pid: Some(1), version: Some("0.1.0".into()), sessions }
    }

    #[test]
    fn describes_server() {
        assert_eq!(server(Some(3)).message(), "An incompatible Silo server is running (3 sessions). Upgrade it to use this build.");
        assert_eq!(server(Some(1)).headline(), "An incompatible Silo server is running (1 session).");
        assert_eq!(server(None).headline(), "An incompatible Silo server is running.");
    }

    #[test]
    fn build_is_set() {
        assert_eq!(silo_proto::BUILD.len(), 16);
        assert!(silo_proto::BUILD.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(silo_proto::BUILD, OTHER);
    }

    #[test]
    fn recognises_silo_processes() {
        assert!(is_silo_command("/Applications/Silo.app/Contents/MacOS/silo-server"));
        assert!(is_silo_command("silo"));
        assert!(!is_silo_command("/bin/zsh"));
        assert!(!is_silo_command("silo-menubar"));
    }

    /// A server of build [`OTHER`]: rejects other hellos the way real
    /// servers do, answers `session.list` with two sessions and
    /// `server.shutdown` by exiting (releasing its lock).
    async fn other_server(home: &Path) -> (std::sync::Arc<std::sync::atomic::AtomicUsize>, tokio::task::JoinHandle<()>) {
        fake_server(home, OTHER, silo_proto::CHANNEL).await
    }

    /// A server of `build` and `channel`, otherwise as [`other_server`].
    async fn fake_server(
        home: &Path,
        build: &'static str,
        channel: silo_proto::Channel,
    ) -> (std::sync::Arc<std::sync::atomic::AtomicUsize>, tokio::task::JoinHandle<()>) {
        use silo_proto::{
            Frame,
            control::{ClientHello, Request, RpcError, ServerHello, ServerInfo, ServerMessage},
            kind, read_frame, write_frame,
        };
        std::fs::create_dir_all(home).unwrap();
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(home)).unwrap();
        rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        let listener = tokio::net::UnixListener::bind(paths::socket(home)).unwrap();
        let runtime = paths::Runtime { pid: 4242, socket: paths::socket(home), version: "0.0.1".into(), build: build.into(), started_at: 0, channel };
        std::fs::write(paths::runtime(home), serde_json::to_vec(&runtime).unwrap()).unwrap();
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connections.clone();
        let task = tokio::spawn(async move {
            let _lock = lock;
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (mut r, mut w) = stream.into_split();
                let hello: ClientHello = read_frame(&mut r).await.unwrap().unwrap().parse().unwrap();
                if hello.build != build {
                    // A server from before protocol numbers rejected other builds.
                    let error = RpcError::new("different_build", format!("different build: server {build}, client {}", hello.build));
                    let reply = ServerMessage::Response { id: 0, result: None, error: Some(error) };
                    write_frame(&mut w, &Frame::json(kind::CONTROL, &reply).unwrap()).await.unwrap();
                    continue;
                }
                let info = ServerInfo {
                    version: "0.0.1".into(),
                    pid: 4242,
                    server_id: "other".into(),
                    started_at: 0,
                    endpoint_id: String::new(),
                    hostname: String::new(),
                    remote_error: None,
                    endpoint_addr: None,
                    idle_exit_secs: 0,
                };
                let ours = build == silo_proto::BUILD;
                let hello = ServerHello {
                    protocol: if ours { silo_proto::PROTOCOL } else { 0 },
                    vt: if ours { silo_proto::VT.into() } else { String::new() },
                    features: Vec::new(),
                    build: build.into(),
                    client_id: 1,
                    server: info,
                    granted: None,
                    channel,
                };
                write_frame(&mut w, &Frame::json(kind::CONTROL_HELLO, &hello).unwrap()).await.unwrap();
                while let Ok(Some(frame)) = read_frame(&mut r).await {
                    let request: Request = frame.parse().unwrap();
                    let shutdown = matches!(request.method, Method::ServerShutdown { .. });
                    let result = if shutdown { serde_json::json!({}) } else { serde_json::json!([{}, {}]) };
                    let reply = ServerMessage::Response { id: request.id, result: Some(result), error: None };
                    write_frame(&mut w, &Frame::json(kind::CONTROL, &reply).unwrap()).await.unwrap();
                    if shutdown {
                        return;
                    }
                }
            }
        });
        (connections, task)
    }

    #[tokio::test]
    async fn different_build_is_reported_not_respawned_and_stopped() {
        let home = std::env::temp_dir().join(format!("silo-mismatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (connections, task) = other_server(&home).await;
        // A missing server binary: any spawn attempt would fail differently.
        let error = crate::connect_or_spawn(&home, Path::new("/nonexistent/silo"), "test").await.err().expect("a different build must not connect");
        let found = error.downcast_ref::<IncompatibleServer>().expect("different build, not a spawn failure");
        assert_eq!(found, &IncompatibleServer { build: Some(OTHER.into()), pid: Some(4242), version: Some("0.0.1".into()), sessions: Some(2) });
        // One rejected hello plus one best-effort look at its sessions.
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 2);
        stop(&home, found).await.unwrap();
        assert!(!server_lock_held(&home));
        task.await.unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A server from before the build handshake: holds the lock, accepts
    /// the socket and answers the hello as connection `n % 4` says (hang
    /// up as the real one does, an unknown error code, its old hello
    /// shape, garbage). Its `runtime.json` has no build; its pid is a
    /// process named `silo`, and the lock goes once that exits.
    fn old_server(home: &Path) -> (std::sync::Arc<std::sync::atomic::AtomicUsize>, u32, tokio::task::JoinHandle<()>) {
        use silo_proto::{Frame, kind, read_frame, write_frame};
        std::fs::create_dir_all(home.join("bin")).unwrap();
        // Linux names a script's process after the script; macOS after its interpreter, but its
        // sleep runs under any name (uutils' multicall sleep on Ubuntu 26.04 refuses another name).
        if cfg!(target_os = "linux") {
            std::fs::write(home.join("bin/silo"), "#!/bin/sh\nsleep 60\n").unwrap();
            std::fs::set_permissions(home.join("bin/silo"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        } else {
            std::os::unix::fs::symlink("/bin/sleep", home.join("bin/silo")).unwrap();
        }
        // A fork elsewhere in the test process may briefly hold the fresh script open for writing.
        let mut child = (0..50)
            .find_map(|_| match std::process::Command::new(home.join("bin/silo")).arg("60").spawn() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    None
                }
                spawned => Some(spawned.unwrap()),
            })
            .expect("spawn the old server");
        let pid = child.id();
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(home)).unwrap();
        rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        std::thread::spawn(move || drop((child.wait(), lock)));
        let mut runtime = serde_json::json!({"pid": pid, "socket": paths::socket(home), "version": "0.1.0", "ghostty_rev": "x", "started_at": 0});
        // Without a channel it is a release server, which a dev build never
        // upgrades or stops (`the_other_channels_server_is_left_alone`):
        // pretend it was a dev one when this build is.
        if silo_proto::CHANNEL == silo_proto::Channel::Dev {
            runtime["channel"] = "dev".into();
        }
        std::fs::write(paths::runtime(home), runtime.to_string()).unwrap();
        let listener = tokio::net::UnixListener::bind(paths::socket(home)).unwrap();
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connections.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                read_frame(&mut stream).await.unwrap();
                let reply = match n % 4 {
                    0 => continue,
                    1 => {
                        let error = silo_proto::control::RpcError::new("protocol", "server speaks protocol 1");
                        Frame::json(kind::CONTROL, &silo_proto::control::ServerMessage::Response { id: 0, result: None, error: Some(error) })
                    }
                    2 => Frame::json(kind::CONTROL_HELLO, &serde_json::json!({"protocol": 1, "client_id": 1, "server": {"version": "0.1.0"}})),
                    _ => Ok(Frame::new(kind::CONTROL_HELLO, b"\xff garbage".to_vec())),
                };
                write_frame(&mut stream, &reply.unwrap()).await.unwrap();
            }
        });
        (connections, pid, task)
    }

    #[tokio::test]
    async fn old_server_is_a_different_build_not_respawned_and_stopped() {
        let home = std::env::temp_dir().join(format!("silo-mismatch-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (connections, pid, task) = old_server(&home);
        let expected = IncompatibleServer { build: None, pid: Some(pid), version: Some("0.1.0".into()), sessions: None };
        for n in 1..=4 {
            let error = crate::connect_or_spawn(&home, Path::new("/nonexistent/silo"), "test").await.err().expect("an old server must not connect");
            assert_eq!(error.downcast_ref::<IncompatibleServer>(), Some(&expected), "reply {}: {error:#}", n - 1);
            assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), n, "one hello each, no session lookup");
        }
        // It closes on the upgrade request, so the takeover exits 3: one
        // upgrade stops it.
        let successor = home.join("bin/successor");
        let script = format!("#!/bin/sh\n[ \"$2\" = --takeover ] && exit {}\n", silo_proto::EXIT_NOT_UPGRADABLE);
        std::fs::write(&successor, script).unwrap();
        std::fs::set_permissions(&successor, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(upgrade(&home, &successor, &expected).await.unwrap(), Upgraded::Stopped);
        assert!(!server_lock_held(&home), "the old server was stopped");
        task.abort();
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The live path needs a real successor binary (see `silo-server`'s
    /// `tests/upgrade.rs`); here: a missing one fails at once and leaves
    /// the other server serving.
    #[tokio::test]
    async fn upgrade_without_successor_fails_and_keeps_server() {
        let home = std::env::temp_dir().join(format!("silo-mismatch-upgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (_, task) = other_server(&home).await;
        let found = inspect(&home).await;
        let error = upgrade(&home, Path::new("/nonexistent/silo"), &found).await.expect_err("no successor");
        assert_eq!(format!("{error:#}"), "could not start the Silo server");
        assert!(server_lock_held(&home));
        stop(&home, &found).await.unwrap();
        task.await.unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    struct NothingRuns;

    impl crate::service::Runner for NothingRuns {
        fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<crate::service::Output> {
            panic!("ran {program} {args:?} against another channel's server");
        }
    }

    /// A server of the other channel (a release server for this dev build,
    /// or the reverse) is never taken over, adopted, upgraded or stopped:
    /// `up` and `upgrade` refuse before doing anything, whether it answers
    /// the hello (this build) or rejects it (another build: its
    /// `runtime.json` names the channel).
    #[tokio::test]
    async fn the_other_channels_server_is_left_alone() {
        use crate::service::{Manager, Server, Service};
        let other = silo_proto::CHANNEL.other();
        let refused = Some(silo_proto::OtherChannel { server: other });
        for (name, build) in [("same", silo_proto::BUILD), ("other", OTHER)] {
            let home = std::env::temp_dir().join(format!("silo-mismatch-channel-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            let (_, task) = fake_server(&home, build, other).await;
            assert_eq!(crate::service::probe_server(&home, "test").await.0, Server::OtherChannel(other), "{name} build");
            let svc = Service::new(Manager::None, "/nonexistent/silo".into(), home.clone(), home.clone(), 501, "bat".into()).unwrap();
            let error = crate::service::up(&svc, &NothingRuns, "test").await.err().expect("refused");
            assert_eq!(error.downcast_ref::<silo_proto::OtherChannel>().copied(), refused, "{name} build: {error:#}");
            let error = upgrade(&home, Path::new("/nonexistent/silo"), &inspect(&home).await).await.expect_err("refused");
            assert_eq!(error.downcast_ref::<silo_proto::OtherChannel>().copied(), refused, "{name} build: {error:#}");
            assert!(server_lock_held(&home), "{name} build: still serving");
            task.abort();
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[tokio::test]
    async fn locked_home_is_not_respawned() {
        let home = std::env::temp_dir().join(format!("silo-mismatch-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(&home)).unwrap();
        rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        let error = crate::connect_or_spawn(&home, Path::new("/nonexistent/silo"), "test").await.err().unwrap();
        assert!(format!("{error:#}").contains("not responding"), "{error:#}");
        drop(lock);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A real server: this build connects, `runtime.json` records the
    /// build, and speaking as another build (inspecting an incompatible
    /// server) expects that build's reply, which this server is not.
    #[tokio::test]
    async fn handshake_and_inspection() {
        let home = std::env::temp_dir().join(format!("silo-mismatch-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let config =
            silo_server::Config { home: home.clone(), idle_exit: None, relay: silo_server::RelayConfig::Disabled, mdns: false, ..Default::default() };
        let server = tokio::spawn(silo_server::run(config));
        let socket = paths::socket(&home);
        let mut connected = None;
        for _ in 0..200 {
            if let Ok(c) = Control::connect(&socket, "test").await {
                connected = Some(c.0);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let control = connected.expect("same build connects");
        assert_eq!(control.hello().build, silo_proto::BUILD);
        assert_eq!(read_runtime(&home).unwrap().build.as_deref(), Some(silo_proto::BUILD));

        let error = Control::connect_build(silo_transport::Conn::Unix(socket.clone()), "inspect", false, OTHER).await.err().expect("not that build");
        assert!(error.downcast_ref::<IncompatibleProtocol>().is_some(), "{error:#}");

        control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn lock_probe() {
        // A flock follows its open file into children forked (by parallel
        // tests) until they exec, so a lock may stay held a moment longer.
        let eventually = |what: &str, check: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !check() {
                assert!(std::time::Instant::now() < deadline, "{what}");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let home = std::env::temp_dir().join(format!("silo-mismatch-lock-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        assert!(!server_lock_held(&home), "no lock file");
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(&home)).unwrap();
        eventually("unlocked", &|| !server_lock_held(&home));
        eventually("lock", &|| rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok());
        assert!(server_lock_held(&home), "locked");
        drop(file);
        eventually("released", &|| !server_lock_held(&home));
        let _ = std::fs::remove_dir_all(&home);
    }
}
