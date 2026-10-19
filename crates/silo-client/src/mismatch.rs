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

use crate::{Control, control::BuildMismatch};

const NAME: &str = "silo (different build)";

/// A server of a different build owns the state directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DifferentBuild {
    /// Its build, pid and version from `runtime.json`.
    pub build: Option<String>,
    pub pid: Option<u32>,
    pub version: Option<String>,
    /// Its running sessions, when it could be asked.
    pub sessions: Option<usize>,
}

impl DifferentBuild {
    /// Short status, e.g. for a menu line.
    pub const LABEL: &str = "Different server build";

    /// E.g. "A different Silo server build is running (3 sessions)."
    #[must_use]
    pub fn headline(&self) -> String {
        match self.sessions {
            Some(1) => "A different Silo server build is running (1 session).".into(),
            Some(n) => format!("A different Silo server build is running ({n} sessions)."),
            None => "A different Silo server build is running.".into(),
        }
    }

    /// The headline and what to do.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{} Upgrade it to use this build; sessions keep running.", self.headline())
    }
}

impl std::fmt::Display for DifferentBuild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.headline())
    }
}

impl std::error::Error for DifferentBuild {}

/// Whether `error` is a build rejection.
#[must_use]
pub fn is_mismatch(error: &anyhow::Error) -> bool {
    error.downcast_ref::<BuildMismatch>().is_some()
}

/// Describe the server that rejected our build: `runtime.json`, plus a
/// best-effort session count.
pub async fn inspect(home: &Path) -> DifferentBuild {
    let runtime = read_runtime(home);
    let sessions = match &runtime {
        Some(r) => count_sessions(home, &r.build).await,
        None => None,
    };
    DifferentBuild {
        build: runtime.as_ref().map(|r| r.build.clone()),
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

fn read_runtime(home: &Path) -> Option<paths::Runtime> {
    serde_json::from_slice(&std::fs::read(paths::runtime(home)).ok()?).ok()
}

/// Whether some server process holds `home`'s server lock (running,
/// starting, or not answering). Spawning another one would only fail.
#[must_use]
pub fn server_lock_held(home: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(paths::server_lock(home)) else {
        return false;
    };
    rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err()
}

/// Stop the different-build server: best-effort `server.shutdown`, else
/// SIGTERM to the pid in `runtime.json` once that is confirmed to be a silo
/// server for `home`. Never SIGKILL. Returns once the server lock is free.
/// Ends its sessions; used to quit. [`upgrade`] keeps them.
///
/// # Errors
/// The server could not be asked or signalled, or did not exit in time.
pub async fn stop(home: &Path, server: &DifferentBuild) -> anyhow::Result<()> {
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

/// Replace the different-build server with this build's `server_exe`,
/// live: `server --takeover --daemon` takes every session, programs
/// included, from it. Returns once this build answers on the socket;
/// clients then reconnect. On failure the old server keeps serving.
///
/// # Errors
/// The successor could not be started or did not take over in time.
pub async fn upgrade(home: &Path, server_exe: &Path, server: &DifferentBuild) -> anyhow::Result<()> {
    let mut child = crate::spawn::spawn_server(home, server_exe, &["--takeover", "--idle-exit", "10"])?;
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
            return Err(failed(format!("successor exited ({status})")));
        }
        if Instant::now() >= deadline {
            std::thread::spawn(move || child.wait());
            return Err(failed("timed out".into()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Generous: the successor restores every block's snapshot first.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(60);

async fn ask_shutdown(home: &Path, build: &str) -> bool {
    let Some(control) = connect(home, build).await else {
        return false;
    };
    let asked = tokio::time::timeout(Duration::from_secs(2), control.call_value(Method::ServerShutdown {})).await;
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

    fn server(sessions: Option<usize>) -> DifferentBuild {
        DifferentBuild { build: Some(OTHER.into()), pid: Some(1), version: Some("0.1.0".into()), sessions }
    }

    #[test]
    fn describes_server() {
        assert_eq!(
            server(Some(3)).message(),
            "A different Silo server build is running (3 sessions). Upgrade it to use this build; sessions keep running."
        );
        assert_eq!(server(Some(1)).headline(), "A different Silo server build is running (1 session).");
        assert_eq!(server(None).headline(), "A different Silo server build is running.");
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
        use silo_proto::{
            Frame,
            control::{ClientHello, Request, RpcError, ServerHello, ServerInfo, ServerMessage},
            kind, read_frame, write_frame,
        };
        std::fs::create_dir_all(home).unwrap();
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(home)).unwrap();
        rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        let listener = tokio::net::UnixListener::bind(paths::socket(home)).unwrap();
        let runtime = paths::Runtime { pid: 4242, socket: paths::socket(home), version: "0.0.1".into(), build: OTHER.into(), started_at: 0 };
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
                if hello.build != OTHER {
                    let error = RpcError::different_build(OTHER, &hello.build);
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
                    remote_error: None,
                    endpoint_addr: None,
                };
                let hello = ServerHello { build: OTHER.into(), client_id: 1, server: info };
                write_frame(&mut w, &Frame::json(kind::CONTROL_HELLO, &hello).unwrap()).await.unwrap();
                while let Ok(Some(frame)) = read_frame(&mut r).await {
                    let request: Request = frame.parse().unwrap();
                    let shutdown = matches!(request.method, Method::ServerShutdown {});
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
        let found = error.downcast_ref::<DifferentBuild>().expect("different build, not a spawn failure");
        assert_eq!(found, &DifferentBuild { build: Some(OTHER.into()), pid: Some(4242), version: Some("0.0.1".into()), sessions: Some(2) });
        // One rejected hello plus one best-effort look at its sessions.
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 2);
        stop(&home, found).await.unwrap();
        assert!(!server_lock_held(&home));
        task.await.unwrap();
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

    /// A real server: the same build connects, a forged build is rejected
    /// with both builds named, and `runtime.json` records the build.
    #[tokio::test]
    async fn handshake_requires_same_build() {
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
        assert_eq!(read_runtime(&home).unwrap().build, silo_proto::BUILD);

        let error =
            Control::connect_build(silo_transport::Conn::Unix(socket.clone()), "forged", false, OTHER).await.err().expect("forged build rejected");
        let rejected = error.downcast_ref::<BuildMismatch>().expect("a build mismatch");
        assert!(rejected.message.contains(silo_proto::BUILD) && rejected.message.contains(OTHER), "{rejected}");

        control.call_value(Method::ServerShutdown {}).await.unwrap();
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn lock_probe() {
        let home = std::env::temp_dir().join(format!("silo-mismatch-lock-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        assert!(!server_lock_held(&home), "no lock file");
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(&home)).unwrap();
        assert!(!server_lock_held(&home), "unlocked");
        rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        assert!(server_lock_held(&home), "locked");
        drop(file);
        assert!(!server_lock_held(&home), "released");
        let _ = std::fs::remove_dir_all(&home);
    }
}
