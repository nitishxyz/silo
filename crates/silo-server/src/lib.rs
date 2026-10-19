//! The silo server: an always-on daemon owning sessions, PTYs and the
//! authoritative terminal state, serving clients over a unix socket (and,
//! later, iroh).

pub mod block;
pub mod conn;
pub mod handoff;
mod names;
pub mod persist;
pub mod registry;

use std::{
    os::fd::AsFd,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use silo_proto::{control::ServerInfo, paths};
use tokio::{net::UnixListener, sync::Notify};

use crate::registry::Registry;

pub struct Config {
    pub home: PathBuf,
    /// Exit after this long with no sessions and no clients.
    pub idle_exit: Option<Duration>,
    /// How often running blocks are checkpointed to `<home>/state`.
    pub checkpoint_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self { home: paths::home(), idle_exit: None, checkpoint_interval: Duration::from_secs(5) }
    }
}

pub struct Server {
    pub registry: Mutex<Registry>,
    pub info: ServerInfo,
    pub shutdown: Notify,
    /// Set while blocks are being handed to a successor; mutations fail.
    pub upgrading: AtomicBool,
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

fn prepare_home(home: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    set_mode(home, 0o700);
    Ok(())
}

pub(crate) fn lock(home: &Path) -> anyhow::Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths::server_lock(home))?;
    rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| anyhow::anyhow!("another silo server is running for {}", home.display()))?;
    Ok(lock)
}

fn bind(home: &Path) -> anyhow::Result<UnixListener> {
    let socket = paths::socket(home);
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    set_mode(&socket, 0o600);
    Ok(listener)
}

/// Run the server until shutdown is requested.
///
/// # Errors
/// Another server already owns `config.home`, or the socket cannot be bound.
pub async fn run(config: Config) -> anyhow::Result<()> {
    prepare_home(&config.home)?;
    let lock = lock(&config.home)?;
    let listener = bind(&config.home)?;

    let (notes_tx, notes_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persist, restored) = persist::open(&config.home, &notes_tx)?;
    tracing::info!(sessions = restored.sessions.len(), blocks = restored.blocks.len(), "state restored");
    let start = Start { lock, listener, notes_tx, notes_rx, persist, sessions: restored.sessions, blocks: restored.blocks };
    serve(config, start).await
}

/// Become the server for `config.home` by taking every block, live, from
/// the running one (see [`handoff`]), then serve like [`run`].
/// `fail_before_ready` aborts the upgrade at its last step (a test hook).
///
/// # Errors
/// The upgrade failed (the old server keeps serving), or the lock or socket
/// could not be served after it committed.
pub async fn takeover(config: Config, fail_before_ready: bool) -> anyhow::Result<()> {
    prepare_home(&config.home)?;
    let (notes_tx, notes_rx) = tokio::sync::mpsc::unbounded_channel();
    let taken = handoff::take(&config.home, &notes_tx, fail_before_ready).await?;
    tracing::info!(sessions = taken.sessions.len(), blocks = taken.blocks.len(), "took over");
    let handoff::Taken { lock, listener, persist, sessions, blocks, .. } = taken;
    let start = Start { lock, listener, notes_tx, notes_rx, persist, sessions, blocks };
    serve(config, start).await
}

struct Start {
    lock: std::fs::File,
    listener: UnixListener,
    notes_tx: tokio::sync::mpsc::UnboundedSender<block::Note>,
    notes_rx: tokio::sync::mpsc::UnboundedReceiver<block::Note>,
    persist: persist::Persist,
    sessions: Vec<registry::Session>,
    blocks: std::collections::HashMap<silo_proto::model::BlockId, block::BlockHandle>,
}

async fn serve(config: Config, start: Start) -> anyhow::Result<()> {
    let Start { lock, listener, notes_tx, mut notes_rx, persist, sessions, blocks } = start;
    let home = config.home;
    let socket = paths::socket(&home);
    let started_at = block::now_ms();
    let info = ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        server_id: persist.server_id().to_owned(),
        started_at,
    };
    let runtime = paths::Runtime {
        pid: info.pid,
        socket: socket.clone(),
        version: info.version.clone(),
        build: silo_proto::BUILD.to_owned(),
        started_at,
    };
    // Not fatal: a server that took over must not end its programs here.
    let written = serde_json::to_vec_pretty(&runtime).map_err(std::io::Error::from).and_then(|j| std::fs::write(paths::runtime(&home), j));
    if let Err(error) = written {
        tracing::error!("write runtime.json: {error}");
    }

    let server = Arc::new(Server {
        registry: Mutex::new(Registry::restored(notes_tx, sessions, blocks)),
        info,
        shutdown: Notify::new(),
        upgrading: AtomicBool::new(false),
        last_activity: Mutex::new(Instant::now()),
    });
    tracing::info!(socket = %socket.display(), pid = server.info.pid, "silo server listening");

    let notes_server = server.clone();
    let notes_task = AbortOnDrop(tokio::spawn(async move {
        while let Some(note) = notes_rx.recv().await {
            notes_server.registry.lock().unwrap().handle_note(note);
        }
    }));
    let kick = server.registry.lock().unwrap().persist_kick.clone();
    let interval = config.checkpoint_interval;
    let mut worker = Some(persist::Worker::spawn(persist, server.clone(), kick.clone(), interval));
    let (upgrade_tx, mut upgrade_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut committed = None;

    let _idle_task = config.idle_exit.map(|idle| {
        let idle_server = server.clone();
        AbortOnDrop(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if idle_server.idle_for() >= idle {
                    tracing::info!("idle; exiting");
                    idle_server.shutdown.notify_one();
                    return;
                }
            }
        }))
    });

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
                tokio::spawn(conn::accept(server.clone(), stream, upgrade_tx.clone()));
            }
            Some(sock) = upgrade_rx.recv(), if worker.is_some() => {
                let own = match (lock.as_fd().try_clone_to_owned(), listener.as_fd().try_clone_to_owned()) {
                    (Ok(lock), Ok(listener)) => [lock, listener],
                    (Err(error), _) | (_, Err(error)) => {
                        tracing::warn!("upgrade refused: {error}");
                        continue;
                    }
                };
                server.upgrading.store(true, Ordering::SeqCst);
                // Commit the final state; nothing is persisted after this
                // unless the upgrade aborts.
                let finished = match worker.take() {
                    Some(worker) => worker.finish().await,
                    None => None,
                };
                let Some(persist) = finished else {
                    // Keep serving (unpersisted) rather than end the programs.
                    tracing::error!("persistence worker failed; upgrade refused");
                    server.upgrading.store(false, Ordering::SeqCst);
                    continue;
                };
                let server_id = persist.server_id().to_owned();
                match handoff::give(&server, sock, server_id, own).await {
                    Ok(sock) => {
                        committed = Some(sock);
                        break;
                    }
                    Err(error) => {
                        tracing::warn!("upgrade aborted: {error:#}");
                        worker = Some(persist::Worker::spawn(persist, server.clone(), kick.clone(), interval));
                        server.upgrading.store(false, Ordering::SeqCst);
                    }
                }
            }
            () = server.shutdown.notified() => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }
    if let Some(sock) = committed {
        return commit(&server, listener, lock, sock).await;
    }
    tracing::info!("shutting down");
    // Commit the final state before teardown empties the tree.
    if let Some(worker) = worker {
        worker.finish().await;
    }
    drop(notes_task);
    server.registry.lock().unwrap().shutdown();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(paths::runtime(&home));
    // Give block actors a moment to hang up their children.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(lock);
    Ok(())
}

/// The successor is ready and holds the lock and listening socket: close
/// our copies and connections and tell it so. The socket path stays bound
/// (it is the same socket); the successor rewrites the runtime file.
async fn commit(server: &Server, listener: UnixListener, lock: std::fs::File, mut sock: std::os::unix::net::UnixStream) -> anyhow::Result<()> {
    drop(listener);
    {
        let mut reg = server.registry.lock().unwrap();
        for client in reg.clients.values_mut() {
            client.events = None;
            if let Some(close) = client.close.take() {
                let _ = close.send(());
            }
        }
        reg.sessions.clear();
    }
    drop(lock);
    tokio::task::spawn_blocking(move || std::io::Write::write_all(&mut sock, handoff::COMMIT)).await??;
    tracing::info!("upgrade committed; exiting");
    Ok(())
}

/// Aborts a background task when the server future is dropped (a crash in
/// tests), so nothing keeps running against the old state.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
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
