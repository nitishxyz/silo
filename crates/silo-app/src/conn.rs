//! Host connections on the tokio runtime: connect through the host's
//! transport, forward events to the UI, and reconnect when the connection
//! drops.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use silo_client::{Control, DifferentBuild};
use silo_proto::control::Event;
use tokio::sync::mpsc;

use crate::hosts::Transport;

#[allow(clippy::large_enum_variant, reason = "sent one at a time over a channel")]
pub enum ConnEvent {
    Connected(Control),
    Event(Event),
    Disconnected(String),
    /// A server of a different build holds this host; nothing was spawned.
    /// Sent again only when its description changes.
    DifferentBuild(DifferentBuild),
    /// The remote server reported a new route (already used for reconnects).
    Addr(silo_client::transport::EndpointAddr),
    /// The remote server refused this device; the loop stops.
    NotPaired,
    /// The remote server is another build; polled slowly.
    RemoteBuild,
    /// "Quit Silo" stopped the server on purpose: quit, don't reconnect.
    Quit,
}

/// How often to look again while an incompatible server holds the host.
const INCOMPATIBLE_POLL: Duration = Duration::from_secs(2);
/// Retry delay while a remote server hands over to a new build.
const UPGRADING_RETRY: Duration = Duration::from_millis(300);
/// How often a connected remote host's address is re-read.
const ADDR_REFRESH: Duration = Duration::from_secs(300);

/// Why a connect attempt failed, as far as the UI cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Unreachable,
    Upgrading,
    NotPaired,
    DifferentBuild,
}

#[must_use]
pub fn classify(error: &anyhow::Error) -> Failure {
    if error.downcast_ref::<silo_client::Upgrading>().is_some() {
        Failure::Upgrading
    } else if error.downcast_ref::<silo_client::Unauthorized>().is_some() {
        Failure::NotPaired
    } else if error.downcast_ref::<silo_client::BuildMismatch>().is_some() {
        Failure::DifferentBuild
    } else {
        Failure::Unreachable
    }
}

/// Delay before retry `attempt` (0-based): doubling from 100 ms locally or
/// 250 ms remotely, capped at 3 s / 15 s, scaled by `jitter` in [0, 1] to
/// 50–100 %.
#[must_use]
pub fn backoff(attempt: u32, remote: bool, jitter: f64) -> Duration {
    let (base, cap): (u64, u64) = if remote { (250, 15_000) } else { (100, 3_000) };
    let full = base.saturating_mul(1u64 << attempt.min(16)).min(cap);
    Duration::from_millis(full).mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0))
}

fn jitter() -> f64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    f64::from(nanos % 1000) / 999.0
}

/// The server binary: `silo-server` (app bundle) or `silo` (cargo target
/// dir) next to this executable, else `silo` on `PATH`.
#[must_use]
pub fn server_exe() -> PathBuf {
    server_exe_path()
}

/// How this app introduces itself to servers.
#[must_use]
pub fn client_name() -> String {
    format!("Silo {} (macOS, pid {})", env!("CARGO_PKG_VERSION"), std::process::id())
}

fn server_exe_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SILO_SERVER_EXE") {
        return path.into();
    }
    if let Some(path) = sibling(&["silo-server", "silo"]) {
        return path;
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("silo");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("silo")
}

/// A binary next to this executable (not this executable itself, which on a
/// case-insensitive volume may answer to another name).
#[must_use]
pub fn sibling(names: &[&str]) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let dir = exe.parent()?;
    names.iter().map(|name| dir.join(name)).find(|candidate| candidate.is_file() && candidate.canonicalize().ok().as_deref() != Some(exe.as_path()))
}

/// Start a host connection loop. While `paused` is set (Stop Server) the
/// loop neither connects nor spawns a server.
pub fn start(rt: &tokio::runtime::Handle, transport: Transport, paused: Arc<AtomicBool>) -> mpsc::UnboundedReceiver<ConnEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    rt.spawn(run(transport, tx, paused));
    rx
}

/// The remote server's current route from `server.info`, if it has one.
async fn server_addr(control: &Control) -> Option<silo_client::transport::EndpointAddr> {
    let info: silo_proto::control::ServerInfo =
        control.call(silo_proto::control::Method::ServerInfo {}).await.inspect_err(|e| tracing::debug!("server.info: {e:#}")).ok()?;
    serde_json::from_value(info.endpoint_addr?).inspect_err(|e| tracing::debug!("server address: {e}")).ok()
}

async fn run(mut transport: Transport, tx: mpsc::UnboundedSender<ConnEvent>, paused: Arc<AtomicBool>) {
    let name = client_name();
    let remote = transport.is_remote();
    let mut attempt = 0u32;
    let mut remote_build = false;
    let mut incompatible: Option<DifferentBuild> = None;
    loop {
        if paused.load(Ordering::Acquire) {
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        // Quit Silo stopped the incompatible server: don't start ours.
        if let Some(pid) = incompatible.as_ref().and_then(|s| s.pid)
            && quit_requested(&transport, pid)
        {
            tracing::info!(pid, "incompatible server stopped by Quit Silo");
            let _ = tx.send(ConnEvent::Quit);
            return;
        }
        // A stopped server accepts the socket but never answers the hello.
        let limit = if remote { silo_client::PING_TIMEOUT_IROH } else { silo_client::PING_TIMEOUT_LOCAL };
        let connected = tokio::time::timeout(limit, transport.connect(&name)).await.unwrap_or_else(|_| Err(anyhow::anyhow!("server not responding")));
        match connected {
            Ok((control, mut events)) => {
                attempt = 0;
                remote_build = false;
                incompatible = None;
                let pid = control.hello().server.pid;
                tracing::info!(pid, "connected to silo server");
                if let Transport::Local { home, .. } = &transport {
                    silo_proto::paths::clear_quit(home, Some(pid));
                }
                if tx.send(ConnEvent::Connected(control.clone())).is_err() {
                    return;
                }
                // Remote: read the server's current route now and every few
                // minutes, so a restart or port change stays reachable.
                let mut info = tokio::time::interval(ADDR_REFRESH);
                let heartbeat = control.clone().heartbeat();
                tokio::pin!(heartbeat);
                let mut hung = false;
                loop {
                    tokio::select! {
                        event = events.recv() => {
                            let Some((_, event)) = event else { break };
                            if tx.send(ConnEvent::Event(event)).is_err() {
                                return;
                            }
                        }
                        () = &mut heartbeat => {
                            tracing::warn!(pid, "server not responding");
                            hung = true;
                            break;
                        }
                        _ = info.tick(), if remote => {
                            if let Some(addr) = server_addr(&control).await
                                && let Transport::Iroh { addr: current, .. } = &mut transport
                                && *current != addr
                                && addr.id == current.id
                            {
                                current.clone_from(&addr);
                                if tx.send(ConnEvent::Addr(addr)).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                if quit_requested(&transport, pid) {
                    tracing::info!(pid, "server stopped by Quit Silo");
                    let _ = tx.send(ConnEvent::Quit);
                    return;
                }
                let reason = if hung {
                    "server not responding"
                } else if paused.load(Ordering::Acquire) {
                    "server stopped"
                } else {
                    "connection closed"
                };
                if tx.send(ConnEvent::Disconnected(reason.into())).is_err() {
                    return;
                }
            }
            Err(error) => {
                if let Some(server) = error.downcast_ref::<DifferentBuild>() {
                    // Slow, quiet polling: nothing is spawned meanwhile.
                    if incompatible.as_ref() != Some(server) {
                        tracing::info!(?server, "incompatible server");
                        incompatible = Some(server.clone());
                        if tx.send(ConnEvent::DifferentBuild(server.clone())).is_err() {
                            return;
                        }
                    }
                    if tx.is_closed() {
                        return;
                    }
                    tokio::time::sleep(INCOMPATIBLE_POLL).await;
                    continue;
                }
                incompatible = None;
                tracing::debug!("connect failed: {error:#}");
                let delay = match classify(&error) {
                    Failure::Upgrading => UPGRADING_RETRY,
                    Failure::NotPaired => {
                        let _ = tx.send(ConnEvent::NotPaired);
                        return;
                    }
                    Failure::DifferentBuild => {
                        if !std::mem::replace(&mut remote_build, true) && tx.send(ConnEvent::RemoteBuild).is_err() {
                            return;
                        }
                        backoff(u32::MAX, remote, jitter())
                    }
                    Failure::Unreachable => {
                        remote_build = false;
                        // The full error (paths and all) stays in the log.
                        if tx.send(ConnEvent::Disconnected("connection failed".into())).is_err() {
                            return;
                        }
                        backoff(attempt, remote, jitter())
                    }
                };
                if tx.is_closed() {
                    return;
                }
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Whether the server with `pid` went away because of "Quit Silo" (as
/// opposed to a crash, idle exit, Stop Server or Restart Server).
fn quit_requested(transport: &Transport, pid: u32) -> bool {
    match transport {
        Transport::Local { home, .. } => silo_proto::paths::quit_marked(home, pid),
        Transport::Iroh { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(name: &str) -> (PathBuf, Transport) {
        let home = std::env::temp_dir().join(format!("silo-quit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        (home.clone(), Transport::Local { home, server_exe: PathBuf::from("silo") })
    }

    #[test]
    fn crash_or_restart_reconnects() {
        let (home, transport) = local("crash");
        assert!(!quit_requested(&transport, 42));
        // A marker for an older server (e.g. before a restart) is ignored.
        silo_proto::paths::mark_quit(&home, 7).unwrap();
        assert!(!quit_requested(&transport, 42));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn quit_silo_quits() {
        let (home, transport) = local("quit");
        silo_proto::paths::mark_quit(&home, 42).unwrap();
        assert!(quit_requested(&transport, 42));
        // Connecting to another server clears the stale marker.
        silo_proto::paths::clear_quit(&home, Some(43));
        assert!(!quit_requested(&transport, 42));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn remote_never_quits() {
        let home = crate::hosts::tests::temp("never-quits");
        let addr = crate::hosts::tests::addr(&home.join("key"));
        silo_proto::paths::mark_quit(&home, 42).unwrap();
        assert!(!quit_requested(&Transport::Iroh { home: home.clone(), addr }, 42));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn backoff_schedule() {
        let ms = |attempt, remote, jitter| backoff(attempt, remote, jitter).as_millis();
        assert_eq!([0, 1, 2, 3, 5].map(|a| ms(a, false, 1.0)), [100, 200, 400, 800, 3000]);
        assert_eq!([0, 1, 5, 6, 40].map(|a| ms(a, true, 1.0)), [250, 500, 8000, 15000, 15000]);
        // Jitter scales to 50–100 % and never goes past the cap.
        assert_eq!(ms(0, true, 0.0), 125);
        assert_eq!(ms(u32::MAX, true, 0.0), 7500);
        assert_eq!(ms(3, true, 7.0), 2000);
    }

    #[test]
    fn errors_map_to_failures() {
        let message = String::from("why");
        assert_eq!(classify(&silo_client::Upgrading { message: message.clone() }.into()), Failure::Upgrading);
        assert_eq!(classify(&silo_client::Unauthorized { message: message.clone() }.into()), Failure::NotPaired);
        assert_eq!(classify(&silo_client::BuildMismatch { message }.into()), Failure::DifferentBuild);
        assert_eq!(classify(&anyhow::anyhow!("dial server")), Failure::Unreachable);
        // Context added on the way up keeps the type.
        let wrapped = anyhow::Error::from(silo_client::Unauthorized { message: "x".into() }).context("hello");
        assert_eq!(classify(&wrapped), Failure::NotPaired);
    }
}
