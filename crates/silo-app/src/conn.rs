//! Host connections on the tokio runtime: connect through the host's
//! transport, forward events to the UI, and reconnect when the connection
//! drops.

use std::{path::PathBuf, time::Duration};

use silo_client::Control;
use silo_proto::control::Event;
use tokio::sync::mpsc;

use crate::hosts::Transport;

pub enum ConnEvent {
    Connected(Control),
    Event(Event),
    Disconnected(String),
}

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

pub fn start(rt: &tokio::runtime::Handle, transport: Transport) -> mpsc::UnboundedReceiver<ConnEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    rt.spawn(run(transport, tx));
    rx
}

async fn run(transport: Transport, tx: mpsc::UnboundedSender<ConnEvent>) {
    let name = format!("Silo {} (macOS, pid {})", env!("CARGO_PKG_VERSION"), std::process::id());
    let mut backoff = Duration::from_millis(100);
    loop {
        match transport.connect(&name).await {
            Ok((control, mut events)) => {
                backoff = Duration::from_millis(100);
                tracing::info!(pid = control.hello().server.pid, "connected to silo server");
                if tx.send(ConnEvent::Connected(control.clone())).is_err() {
                    return;
                }
                while let Some((_, event)) = events.recv().await {
                    if tx.send(ConnEvent::Event(event)).is_err() {
                        return;
                    }
                }
                if tx.send(ConnEvent::Disconnected("connection closed".into())).is_err() {
                    return;
                }
            }
            Err(error) => {
                tracing::debug!("connect failed: {error:#}");
                if tx.send(ConnEvent::Disconnected(format!("{error:#}"))).is_err() || !transport.retries() {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(3));
            }
        }
    }
}
