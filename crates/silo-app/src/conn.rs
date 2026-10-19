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

pub enum ConnEvent {
    Connected(Control),
    Event(Event),
    Disconnected(String),
    /// A server of a different build holds this host; nothing was spawned.
    /// Sent again only when its description changes.
    DifferentBuild(DifferentBuild),
    /// "Quit Silo" stopped the server on purpose: quit, don't reconnect.
    Quit,
}

/// How often to look again while an incompatible server holds the host.
const INCOMPATIBLE_POLL: Duration = Duration::from_secs(2);

/// The server binary: `silo-server` (app bundle) or `silo` (cargo target
/// dir) next to this executable, else `silo` on `PATH`.
#[must_use]
pub fn server_exe() -> PathBuf {
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
    names.iter().map(|name| dir.join(name)).find(|candidate| {
        candidate.is_file() && candidate.canonicalize().ok().as_deref() != Some(exe.as_path())
    })
}

/// Start a host connection loop. While `paused` is set (Stop Server) the
/// loop neither connects nor spawns a server.
pub fn start(rt: &tokio::runtime::Handle, transport: Transport, paused: Arc<AtomicBool>) -> mpsc::UnboundedReceiver<ConnEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    rt.spawn(run(transport, tx, paused));
    rx
}

async fn run(transport: Transport, tx: mpsc::UnboundedSender<ConnEvent>, paused: Arc<AtomicBool>) {
    let name = format!("Silo {} (macOS, pid {})", env!("CARGO_PKG_VERSION"), std::process::id());
    let mut backoff = Duration::from_millis(100);
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
        match transport.connect(&name).await {
            Ok((control, mut events)) => {
                backoff = Duration::from_millis(100);
                incompatible = None;
                let pid = control.hello().server.pid;
                tracing::info!(pid, "connected to silo server");
                if let Transport::Local { home, .. } = &transport {
                    silo_proto::paths::clear_quit(home, Some(pid));
                }
                if tx.send(ConnEvent::Connected(control.clone())).is_err() {
                    return;
                }
                while let Some((_, event)) = events.recv().await {
                    if tx.send(ConnEvent::Event(event)).is_err() {
                        return;
                    }
                }
                if quit_requested(&transport, pid) {
                    tracing::info!(pid, "server stopped by Quit Silo");
                    let _ = tx.send(ConnEvent::Quit);
                    return;
                }
                let reason = if paused.load(Ordering::Acquire) { "server stopped" } else { "connection closed" };
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
                // The full error (paths and all) stays in the log.
                if tx.send(ConnEvent::Disconnected("connection failed".into())).is_err() || !transport.retries() {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(3));
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
        assert!(!quit_requested(&Transport::Iroh { address: "x".into() }, 42));
    }
}
