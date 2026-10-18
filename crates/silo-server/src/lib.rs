//! The silo server: an always-on daemon owning sessions, PTYs and the
//! authoritative terminal state, serving clients over a unix socket (and,
//! later, iroh).

pub mod block;
pub mod conn;
mod names;
pub mod registry;

use std::{
    os::fd::AsFd,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use silo_proto::{control::ServerInfo, paths};
use tokio::{net::UnixListener, sync::Notify};

use crate::registry::Registry;

pub const SNAPSHOT_VERSION: u16 = 1;

pub struct Config {
    pub home: PathBuf,
    /// Exit after this long with no sessions and no clients.
    pub idle_exit: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self { home: paths::home(), idle_exit: None }
    }
}

pub struct Server {
    pub registry: Mutex<Registry>,
    pub info: ServerInfo,
    pub shutdown: Notify,
    last_activity: Mutex<Instant>,
}

impl Server {
    pub fn touch(&self) {
        if let Ok(mut last) = self.last_activity.lock() {
            *last = Instant::now();
        }
    }

    fn idle_for(&self) -> Duration {
        let busy = {
            let reg = self.registry.lock().unwrap();
            !reg.sessions.is_empty() || reg.clients.values().any(|c| !c.passive)
        };
        if busy {
            self.touch();
            return Duration::ZERO;
        }
        self.last_activity.lock().map_or(Duration::ZERO, |l| l.elapsed())
    }
}

/// Run the server until shutdown is requested.
///
/// # Errors
/// Another server already owns `config.home`, or the socket cannot be bound.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let home = config.home;
    std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
    set_mode(&home, 0o700);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths::server_lock(&home))?;
    rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| anyhow::anyhow!("another silo server is running for {}", home.display()))?;

    let socket = paths::socket(&home);
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    set_mode(&socket, 0o600);

    let started_at = block::now_ms();
    let info = ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        ghostty_rev: silo_vt::GHOSTTY_REV.to_owned(),
        snapshot_version: SNAPSHOT_VERSION,
        pid: std::process::id(),
        server_id: format!("{:016x}", rand::random::<u64>()),
        started_at,
    };
    let runtime = paths::Runtime {
        pid: info.pid,
        socket: socket.clone(),
        version: info.version.clone(),
        ghostty_rev: info.ghostty_rev.clone(),
        started_at,
    };
    std::fs::write(paths::runtime(&home), serde_json::to_vec_pretty(&runtime)?)?;

    let (notes_tx, mut notes_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = Arc::new(Server {
        registry: Mutex::new(Registry::new(notes_tx)),
        info,
        shutdown: Notify::new(),
        last_activity: Mutex::new(Instant::now()),
    });
    tracing::info!(socket = %socket.display(), pid = server.info.pid, "silo server listening");

    let notes_server = server.clone();
    tokio::spawn(async move {
        while let Some(note) = notes_rx.recv().await {
            notes_server.registry.lock().unwrap().handle_note(note);
        }
    });

    if let Some(idle) = config.idle_exit {
        let idle_server = server.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if idle_server.idle_for() >= idle {
                    tracing::info!("idle; exiting");
                    idle_server.shutdown.notify_one();
                    return;
                }
            }
        });
    }

    let uid = rustix::process::getuid();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // PTY children get their own sessions, but ignore SIGHUP/SIGPIPE in case
    // the daemon was started from a terminal that later goes away.
    unsafe_ignore_signals();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == uid.as_raw() => {}
                    _ => {
                        tracing::warn!("rejected connection from another user");
                        continue;
                    }
                }
                tokio::spawn(conn::serve(server.clone(), stream));
            }
            () = server.shutdown.notified() => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }
    tracing::info!("shutting down");
    server.registry.lock().unwrap().shutdown();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(paths::runtime(&home));
    // Give block actors a moment to hang up their children.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(lock);
    Ok(())
}

fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[allow(unsafe_code)]
fn unsafe_ignore_signals() {
    // SAFETY: setting dispositions to SIG_IGN has no handler to race with.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}
