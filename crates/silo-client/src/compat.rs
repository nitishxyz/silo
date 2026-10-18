//! Servers this client cannot talk to: an older or newer silo server that
//! holds the state directory. Detected without spawning a second server,
//! described for the user, and stopped through a version-independent path.

use std::{
    os::fd::AsFd,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, bail, ensure};
use silo_proto::{PROTOCOL_VERSION, control::Method, paths};

use crate::{Control, control::ProtocolMismatch};

const NAME: &str = "silo (compat)";

/// A server of another protocol owns the state directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incompatible {
    /// Its protocol, when its rejection names it.
    pub protocol: Option<u32>,
    pub pid: Option<u32>,
    pub version: Option<String>,
    /// Its running sessions, when it could be asked.
    pub sessions: Option<usize>,
}

impl Incompatible {
    #[must_use]
    pub fn older(&self) -> bool {
        self.protocol.is_some_and(|p| p < PROTOCOL_VERSION)
    }

    #[must_use]
    pub fn newer(&self) -> bool {
        self.protocol.is_some_and(|p| p > PROTOCOL_VERSION)
    }

    /// "Older", "Newer" or "Incompatible".
    #[must_use]
    pub fn age(&self) -> &'static str {
        if self.older() {
            "Older"
        } else if self.newer() {
            "Newer"
        } else {
            "Incompatible"
        }
    }

    /// E.g. "An older Silo server is running (3 sessions)."
    #[must_use]
    pub fn headline(&self) -> String {
        let age = match self.age() {
            "Older" => "An older",
            "Newer" => "A newer",
            _ => "An incompatible",
        };
        match self.sessions {
            Some(1) => format!("{age} Silo server is running (1 session)."),
            Some(n) => format!("{age} Silo server is running ({n} sessions)."),
            None => format!("{age} Silo server is running."),
        }
    }

    /// The headline, what to do, and what restarting costs.
    #[must_use]
    pub fn message(&self) -> String {
        let cost = match self.sessions {
            Some(0) => "",
            Some(_) => " Restarting ends those sessions.",
            None => " Restarting ends its sessions.",
        };
        format!("{} Restart it to use this version.{cost}", self.headline())
    }
}

impl std::fmt::Display for Incompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.headline())
    }
}

impl std::error::Error for Incompatible {}

/// The protocol mismatch behind a connection error, if that is what it is.
#[must_use]
pub fn mismatch(error: &anyhow::Error) -> Option<ProtocolMismatch> {
    error.downcast_ref::<ProtocolMismatch>().copied()
}

/// Describe the server behind `mismatch`: pid and version from
/// `runtime.json`, its session count over its own protocol.
pub async fn inspect(home: &Path, mismatch: ProtocolMismatch) -> Incompatible {
    let runtime = read_runtime(home);
    let sessions = match mismatch.server {
        Some(protocol) => count_sessions(home, protocol).await,
        None => None,
    };
    Incompatible {
        protocol: mismatch.server,
        pid: runtime.as_ref().map(|r| r.pid),
        version: runtime.map(|r| r.version),
        sessions,
    }
}

async fn count_sessions(home: &Path, protocol: u32) -> Option<usize> {
    let (control, _) = Control::connect_protocol(&paths::socket(home), NAME, true, protocol).await.ok()?;
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
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(paths::server_lock(home)) else { return false };
    rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err()
}

/// Stop an incompatible server: `server.shutdown` in its own protocol (the
/// request has the same shape in every protocol so far), else SIGTERM to
/// the pid in `runtime.json` once that is confirmed to be a silo server for
/// `home`. Never SIGKILL. Returns once the server lock is free.
///
/// # Errors
/// The server could not be asked or signalled, or did not exit in time.
pub async fn stop(home: &Path, server: &Incompatible) -> anyhow::Result<()> {
    let asked = match server.protocol {
        Some(protocol) => ask_shutdown(home, protocol).await,
        None => false,
    };
    if !asked {
        terminate(home, server.pid)?;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while server_lock_held(home) {
        if Instant::now() >= deadline {
            bail!("the old Silo server did not stop");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn ask_shutdown(home: &Path, protocol: u32) -> bool {
    let Ok((control, _)) = Control::connect_protocol(&paths::socket(home), NAME, true, protocol).await else {
        return false;
    };
    if control.call_value(Method::ServerShutdown {}).await.is_err() {
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
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).context("signal the old server")?;
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

    fn server(protocol: Option<u32>, sessions: Option<usize>) -> Incompatible {
        Incompatible { protocol, pid: Some(1), version: Some("0.1.0".into()), sessions }
    }

    #[test]
    fn describes_older_server() {
        let s = server(Some(PROTOCOL_VERSION - 1), Some(3));
        assert!(s.older());
        assert_eq!(s.age(), "Older");
        assert_eq!(
            s.message(),
            "An older Silo server is running (3 sessions). Restart it to use this version. Restarting ends those sessions."
        );
        assert_eq!(server(Some(PROTOCOL_VERSION - 1), Some(0)).message(), "An older Silo server is running (0 sessions). Restart it to use this version.");
        assert_eq!(server(Some(PROTOCOL_VERSION + 1), Some(1)).headline(), "A newer Silo server is running (1 session).");
        assert_eq!(server(None, None).headline(), "An incompatible Silo server is running.");
    }

    #[test]
    fn parses_rejection() {
        assert_eq!(crate::control::speaks("server speaks protocol 1"), Some(1));
        assert_eq!(crate::control::speaks("protocol version mismatch"), None);
    }

    #[test]
    fn recognises_silo_processes() {
        assert!(is_silo_command("/Applications/Silo.app/Contents/MacOS/silo-server"));
        assert!(is_silo_command("silo"));
        assert!(!is_silo_command("/bin/zsh"));
        assert!(!is_silo_command("silo-menubar"));
    }

    /// A protocol-1 server: rejects other hellos the way real servers do,
    /// answers `session.list` with two sessions and `server.shutdown` by
    /// exiting (releasing its lock).
    async fn old_server(home: &Path) -> (std::sync::Arc<std::sync::atomic::AtomicUsize>, tokio::task::JoinHandle<()>) {
        use silo_proto::{
            Frame,
            control::{ClientHello, Request, RpcError, ServerHello, ServerInfo, ServerMessage},
            kind, read_frame, write_frame,
        };
        std::fs::create_dir_all(home).unwrap();
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(home)).unwrap();
        rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        let listener = tokio::net::UnixListener::bind(paths::socket(home)).unwrap();
        let runtime = paths::Runtime {
            pid: 4242,
            socket: paths::socket(home),
            version: "0.0.1".into(),
            ghostty_rev: String::new(),
            started_at: 0,
        };
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
                if hello.protocol != 1 {
                    let error = RpcError::new("protocol", "server speaks protocol 1");
                    let reply = ServerMessage::Response { id: 0, result: None, error: Some(error) };
                    write_frame(&mut w, &Frame::json(kind::CONTROL, &reply).unwrap()).await.unwrap();
                    continue;
                }
                let info = ServerInfo {
                    version: "0.0.1".into(),
                    ghostty_rev: String::new(),
                    snapshot_version: 1,
                    pid: 4242,
                    server_id: "old".into(),
                    started_at: 0,
                };
                let hello = ServerHello { protocol: 1, client_id: 1, server: info };
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
    async fn older_server_is_reported_not_respawned_and_stopped() {
        let home = std::env::temp_dir().join(format!("silo-compat-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (connections, task) = old_server(&home).await;
        // A missing server binary: any spawn attempt would fail differently.
        let error = crate::connect_or_spawn(&home, Path::new("/nonexistent/silo"), "test")
            .await
            .err()
            .expect("an incompatible server must not connect");
        let found = error.downcast_ref::<Incompatible>().expect("incompatible, not a spawn failure");
        assert_eq!(
            found,
            &Incompatible { protocol: Some(1), pid: Some(4242), version: Some("0.0.1".into()), sessions: Some(2) }
        );
        // One rejected hello plus one protocol-1 look at its sessions.
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 2);
        stop(&home, found).await.unwrap();
        assert!(!server_lock_held(&home));
        task.await.unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn locked_home_is_not_respawned() {
        let home = std::env::temp_dir().join(format!("silo-compat-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(&home)).unwrap();
        rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        let error = crate::connect_or_spawn(&home, Path::new("/nonexistent/silo"), "test").await.err().unwrap();
        assert!(format!("{error:#}").contains("not responding"), "{error:#}");
        drop(lock);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn lock_probe() {
        let home = std::env::temp_dir().join(format!("silo-compat-lock-{}", std::process::id()));
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
