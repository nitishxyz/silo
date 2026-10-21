//! The silo server: an always-on daemon owning sessions, PTYs and the
//! authoritative terminal state, serving clients over a unix socket (and,
//! later, iroh).

pub mod access;
pub mod app_bridge;
pub mod approval;
pub mod auth;
pub mod block;
pub mod browser;
pub mod bundle;
pub mod close_guard;
pub mod conn;
pub mod drops;
pub mod grants;
pub mod handoff;
pub mod layout_cmds;
mod names;
pub mod notify;
pub mod persist;
pub mod prompts;
pub mod registry;
pub mod remote;
pub mod shell;
pub mod status;
pub mod sysclip;
pub mod tunnel;
pub mod upgrade;

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
pub use silo_transport::RelayConfig;
use silo_transport::{Identity, Net, NetOptions};
use tokio::{net::UnixListener, sync::Notify};

use crate::registry::Registry;

pub struct Config {
    pub home: PathBuf,
    /// Exit after this long with no sessions and no clients.
    pub idle_exit: Option<Duration>,
    /// Never sit at zero sessions: start one when there are none (at start
    /// and when the last one closes). For servers that do not idle out.
    pub keep_session: bool,
    /// How often running blocks are checkpointed to `<home>/state`.
    pub checkpoint_interval: Duration,
    /// Relays used by the remote (iroh) endpoint.
    pub relay: RelayConfig,
    /// Advertise and discover on the LAN via mDNS.
    pub mdns: bool,
    /// Keep each remote connection on the path it was established on.
    pub pin_initial_path: bool,
    /// Fixed UDP port for the remote endpoint; `None` picks a random one.
    pub port: Option<u16>,
    /// The host's system clipboard remote pastes are pushed into and, where
    /// it can be watched, copied back from; `None` (the default) for none.
    pub system_clipboard: Option<Arc<dyn sysclip::SystemClipboard>>,
    /// Owner actions from clients that are not a Silo app wait for a human
    /// (Phase 7 §9). On in `silo server`; off by default for embedders.
    pub require_approval: bool,
    /// How often expired grants are removed and their connections closed.
    pub grant_sweep: Duration,
    /// Seconds the sweep adds to the clock when it looks for expired
    /// grants: a test hook to expire a grant once its connections exist.
    #[doc(hidden)]
    pub grant_sweep_ahead: Arc<std::sync::atomic::AtomicU64>,
    /// A fault in reading a successor's `READY` (a test hook).
    #[doc(hidden)]
    pub ready_fault: Option<handoff::ReadyFault>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            home: paths::home(),
            idle_exit: None,
            keep_session: false,
            checkpoint_interval: Duration::from_secs(5),
            relay: RelayConfig::Default,
            mdns: true,
            pin_initial_path: false,
            port: None,
            system_clipboard: None,
            require_approval: false,
            grant_sweep: GRANT_SWEEP,
            grant_sweep_ahead: Arc::default(),
            ready_fault: None,
        }
    }
}

/// Default [`Config::grant_sweep`].
pub const GRANT_SWEEP: Duration = Duration::from_secs(60);

impl Config {
    fn net_options(&self) -> NetOptions {
        NetOptions { relay: self.relay.clone(), mdns: self.mdns, pin_initial_path: self.pin_initial_path, port: self.port }
    }
}

pub struct Server {
    /// The server's state directory.
    pub home: PathBuf,
    pub registry: Mutex<Registry>,
    pub info: ServerInfo,
    pub shutdown: Notify,
    /// Set while blocks are being handed to a successor; mutations fail.
    pub upgrading: AtomicBool,
    pub auth: Mutex<auth::Auth>,
    /// The remote endpoint; taken when the server stops or hands over.
    pub net: tokio::sync::Mutex<Option<Net>>,
    /// Whether the endpoint uses relays (`Net::online` waits for one).
    pub relays: bool,
    last_activity: Mutex<Instant>,
    pub clipboard: HostClipboard,
    /// See [`Config::require_approval`].
    pub require_approval: bool,
    pub approvals: approval::Approvals,
    pub app_requests: app_bridge::Requests,
    /// See [`Config::grant_sweep_ahead`].
    grant_sweep_ahead: Arc<std::sync::atomic::AtomicU64>,
}

/// The host's system clipboard as remote clients see it.
#[derive(Default)]
pub struct HostClipboard {
    pub system: Option<Arc<dyn sysclip::SystemClipboard>>,
    /// The change count right after our last push, so it is not copied back.
    pushed: Mutex<Option<i64>>,
    /// The text of our last push and when it landed, until the next host
    /// change (Universal Clipboard echoing it from the client's Mac).
    pushed_text: Mutex<Option<(String, Instant)>>,
    /// Open remote data streams; changes are watched only while any is.
    pub remote_streams: std::sync::atomic::AtomicUsize,
    /// The block that last got input from a remote stream.
    pub last_input: Mutex<Option<String>>,
}

/// How often [`watch_clipboard`] looks for host clipboard changes.
pub const CLIPBOARD_POLL: Duration = Duration::from_millis(500);
/// How long after a push a host change to the same text reads as its echo.
const ECHO_WINDOW: Duration = Duration::from_secs(10);

impl HostClipboard {
    /// Put `content` (text or image within [`silo_proto::data::MAX_CLIP_BYTES`])
    /// on the system clipboard; done when it is there.
    pub async fn push(&self, content: silo_proto::data::Clip) {
        let Some(system) = self.system.clone() else { return };
        if content.size() > silo_proto::data::MAX_CLIP_BYTES {
            tracing::debug!(len = content.size(), "clipboard push too large; ignored");
            return;
        }
        let text = match &content {
            silo_proto::data::Clip::Text(text) => Some(text.clone()),
            _ => None,
        };
        let written = tokio::task::spawn_blocking(move || system.write(&content).map(|()| system.change_count())).await;
        match written {
            Ok(Ok(count)) => {
                *self.pushed.lock().unwrap() = count;
                *self.pushed_text.lock().unwrap() = text.map(|text| (text, Instant::now()));
            }
            Ok(Err(error)) => tracing::debug!("clipboard push: {error}"),
            Err(error) => tracing::warn!("clipboard push: {error}"),
        }
    }

    /// Whether a host change to `text` at `now` is our last push coming
    /// back. Only the first host change after a push, and only soon after
    /// it, can be: a later copy of the same text is a real copy.
    fn echo(&self, text: &str, now: Instant) -> bool {
        let pushed = self.pushed_text.lock().unwrap().take();
        pushed.is_some_and(|(pushed, at)| pushed == text && now.saturating_duration_since(at) < ECHO_WINDOW)
    }
}

/// While remote streams are open, send changes of the host's clipboard
/// that were not our own pushes to the remote clients of the block that
/// last got their input.
pub async fn watch_clipboard(server: Arc<Server>) {
    let Some(system) = server.clipboard.system.clone() else { return };
    let mut seen: Option<i64> = None;
    loop {
        tokio::time::sleep(CLIPBOARD_POLL).await;
        if server.clipboard.remote_streams.load(Ordering::Relaxed) == 0 {
            seen = None;
            continue;
        }
        let probe = system.clone();
        let Ok(Some(count)) = tokio::task::spawn_blocking(move || probe.change_count()).await else { return };
        let changed = seen.is_some_and(|seen| seen != count);
        seen = Some(count);
        if !changed || *server.clipboard.pushed.lock().unwrap() == Some(count) {
            continue;
        }
        let read = system.clone();
        let Ok(Some(text)) = tokio::task::spawn_blocking(move || read.read_text()).await else { continue };
        if server.clipboard.echo(&text, Instant::now()) {
            continue;
        }
        let Some(block_id) = server.clipboard.last_input.lock().unwrap().clone() else { continue };
        if let Ok(block) = server.registry.lock().unwrap().block(&block_id) {
            block.send(block::BlockMsg::HostClipboard { text });
        }
    }
}

impl Server {
    /// [`Self::info`] with the endpoint's current address.
    pub async fn info_now(&self) -> ServerInfo {
        let addr = self.net.lock().await.as_ref().map(Net::addr);
        let endpoint_addr = addr.and_then(|addr| serde_json::to_value(addr).inspect_err(|e| tracing::warn!("encode endpoint address: {e}")).ok());
        ServerInfo { endpoint_addr, ..self.info.clone() }
    }

    /// The grants and invites. A panic while holding them must not take
    /// remote access down with it, so poisoning is ignored.
    pub fn auth(&self) -> std::sync::MutexGuard<'_, auth::Auth> {
        self.auth.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn touch(&self) {
        if let Ok(mut last) = self.last_activity.lock() {
            *last = Instant::now();
        }
    }

    /// Close the connections of removed grants and tell the owner.
    pub fn revoked(&self, gone: &[silo_proto::access::Grant]) {
        if gone.is_empty() {
            return;
        }
        let ids: Vec<String> = gone.iter().map(|g| g.id.clone()).collect();
        tracing::info!(grants = ?ids, "grants removed; closing their connections");
        let grants = self.auth().grants();
        let mut reg = self.registry.lock().unwrap();
        reg.close_grant_clients(&ids);
        reg.emit_owner(silo_proto::control::Event::GrantsChanged { grants });
    }

    /// Remove the grants of closed sessions and blocks, and expired ones.
    pub fn settle_grants(&self) {
        let gone_scopes = std::mem::take(&mut self.registry.lock().unwrap().gone);
        let now = grants::unix_secs() + self.grant_sweep_ahead.load(Ordering::SeqCst);
        let removed = self.auth().remove_where(|g| g.expired(now) || gone_scopes.contains(&g.scope));
        match removed {
            Ok(gone) => self.revoked(&gone),
            Err(error) => tracing::warn!("remove grants: {error}"),
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

/// This machine's hostname; empty if it has none.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    #[allow(unsafe_code)]
    // SAFETY: the buffer is valid for its length.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if ok { String::from_utf8_lossy(&buf[..len]).into_owned() } else { String::new() }
}

fn prepare_home(home: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    set_mode(home, 0o700);
    Ok(())
}

/// The server lock (`flock` on `<home>/server.lock`). Dropping it unlocks,
/// not only closes: a child forked meanwhile by another thread (another
/// in-process server spawning its PTY host) shares the open file
/// description until it execs, and would hold the lock that long.
///
/// Once [`Self::share`]d with a successor (the same open file description,
/// so an unlock would release its lock too) dropping only closes, however
/// the upgrade or this server ends, until [`Self::keep`] (the upgrade aborted).
pub(crate) struct ServerLock {
    file: std::fs::File,
    shared: bool,
}

impl ServerLock {
    /// A lock taken over from the previous server, live.
    pub(crate) fn adopt(file: std::fs::File) -> Self {
        Self { file, shared: false }
    }

    /// A copy for a successor.
    fn share(&mut self) -> std::io::Result<std::os::fd::OwnedFd> {
        self.shared = true;
        self.file.as_fd().try_clone_to_owned()
    }

    /// The upgrade aborted: the lock is ours alone again.
    fn keep(&mut self) {
        self.shared = false;
    }
}

impl Drop for ServerLock {
    fn drop(&mut self) {
        if !self.shared {
            let _ = rustix::fs::flock(self.file.as_fd(), rustix::fs::FlockOperation::Unlock);
        }
    }
}

pub(crate) fn lock(home: &Path) -> anyhow::Result<ServerLock> {
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(home))?;
    rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| anyhow::anyhow!("another silo server is running for {}", home.display()))?;
    Ok(ServerLock::adopt(lock))
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
    let identity = Identity::load_or_create(paths::iroh_identity(&config.home)).context("load iroh identity")?;
    let auth = auth::Auth::open(&config.home)?;
    let net = Net::bind_with(&identity, config.net_options()).await?;
    let remote = Remote { identity, auth, net: Ok(net) };
    let start = Start { lock, listener, notes_tx, notes_rx, persist, sessions: restored.sessions, blocks: restored.blocks, remote };
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
    // Everything fallible is loaded before taking any program.
    let identity = Identity::load_or_create(paths::iroh_identity(&config.home)).context("load iroh identity")?;
    let auth = auth::Auth::open(&config.home)?;
    let (notes_tx, notes_rx) = tokio::sync::mpsc::unbounded_channel();
    let taken = handoff::take(&config.home, &notes_tx, fail_before_ready).await?;
    tracing::info!(sessions = taken.sessions.len(), blocks = taken.blocks.len(), "took over");
    let handoff::Taken { lock, listener, persist, sessions, blocks, .. } = taken;
    // The old server froze pairing when the upgrade began: re-read the
    // grants it may have recorded since they were first loaded.
    let auth = auth::Auth::open(&config.home).unwrap_or_else(|error| {
        tracing::error!("re-read paired devices: {error:#}");
        auth
    });
    // The old server closed its endpoint before COMMIT. Not fatal: the
    // programs we now hold must not end here.
    let net = Net::bind_with(&identity, config.net_options()).await.map_err(|error| format!("{error:#}"));
    let remote = Remote { identity, auth, net };
    let start = Start { lock: ServerLock::adopt(lock), listener, notes_tx, notes_rx, persist, sessions, blocks, remote };
    serve(config, start).await
}

struct Remote {
    identity: Identity,
    auth: auth::Auth,
    /// The endpoint, or why remote access is unavailable.
    net: Result<Net, String>,
}

struct Start {
    lock: ServerLock,
    listener: UnixListener,
    notes_tx: tokio::sync::mpsc::UnboundedSender<block::Note>,
    notes_rx: tokio::sync::mpsc::UnboundedReceiver<block::Note>,
    persist: persist::Persist,
    sessions: Vec<registry::Session>,
    blocks: std::collections::HashMap<silo_proto::model::BlockId, browser::Block>,
    remote: Remote,
}

async fn serve(config: Config, start: Start) -> anyhow::Result<()> {
    let Start { mut lock, listener, notes_tx, mut notes_rx, persist, sessions, blocks, remote } = start;
    let Remote { identity, auth, net } = remote;
    let (net, incoming, remote_error) = match net.and_then(|mut net| net.serve(16).map(|incoming| (net, incoming)).map_err(|e| format!("{e:#}"))) {
        Ok((net, incoming)) => (Some(net), Some(incoming), None),
        Err(error) => {
            tracing::error!("remote access unavailable: {error}");
            (None, None, Some(error))
        }
    };
    let home = config.home;
    let socket = paths::socket(&home);
    let started_at = block::now_ms();
    let info = ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        server_id: persist.server_id().to_owned(),
        started_at,
        endpoint_id: identity.node_id(),
        hostname: hostname(),
        remote_error,
        endpoint_addr: None,
        idle_exit_secs: config.idle_exit.map_or(0, |d| d.as_secs().max(1)),
    };
    let runtime =
        paths::Runtime { pid: info.pid, socket: socket.clone(), version: info.version.clone(), build: silo_proto::BUILD.to_owned(), started_at };
    // Not fatal: a server that took over must not end its programs here.
    let written = serde_json::to_vec_pretty(&runtime).map_err(std::io::Error::from).and_then(|j| std::fs::write(paths::runtime(&home), j));
    if let Err(error) = written {
        tracing::error!("write runtime.json: {error}");
    }
    if let Err(error) = bundle::install_shims(&home) {
        tracing::error!("install clipboard shims: {error}");
    }
    let (canonical, lead) = bundle::path_lead();
    if let Err(error) = shell::install(canonical, lead) {
        tracing::error!("install shell integration: {error}");
    }
    let _drops_gc = AbortOnDrop(tokio::spawn(drops::gc_daily(home.clone())));

    let server = Arc::new(Server {
        home: home.clone(),
        registry: Mutex::new(Registry::restored(notes_tx, sessions, blocks)),
        info,
        shutdown: Notify::new(),
        upgrading: AtomicBool::new(false),
        auth: Mutex::new(auth),
        net: tokio::sync::Mutex::new(net),
        relays: config.relay != RelayConfig::Disabled,
        last_activity: Mutex::new(Instant::now()),
        clipboard: HostClipboard { system: config.system_clipboard.clone(), ..HostClipboard::default() },
        require_approval: config.require_approval,
        approvals: approval::Approvals::default(),
        app_requests: app_bridge::Requests::default(),
        grant_sweep_ahead: config.grant_sweep_ahead.clone(),
    });
    if config.keep_session {
        server.registry.lock().unwrap().keep_session();
    }
    let _clipboard_task = AbortOnDrop(tokio::spawn(watch_clipboard(server.clone())));
    let sweep_server = server.clone();
    let sweep = config.grant_sweep;
    let _sweep_task = AbortOnDrop(tokio::spawn(async move {
        loop {
            tokio::time::sleep(sweep).await;
            if !sweep_server.upgrading.load(Ordering::SeqCst) {
                sweep_server.settle_grants();
            }
        }
    }));
    tracing::info!(socket = %socket.display(), pid = server.info.pid, "silo server listening");
    let (incoming, tunnels) = incoming.unzip();
    let _remote_task = incoming.map(|incoming| AbortOnDrop(tokio::spawn(remote::accept_loop(server.clone(), incoming))));
    let _tunnel_task = tunnels.map(|tunnels| AbortOnDrop(tokio::spawn(tunnel::accept_loop(server.clone(), tunnels))));
    notify::notify("READY=1");

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
                    Ok(cred) => {
                        tracing::warn!(uid = cred.uid(), "rejected connection from another user");
                        continue;
                    }
                    // A peer that already closed (`server_running` probes
                    // connect and hang up) has no credentials left to read.
                    Err(error) => {
                        tracing::debug!("connection closed before its credentials were read: {error}");
                        continue;
                    }
                }
                tokio::spawn(conn::accept(server.clone(), stream, upgrade_tx.clone()));
            }
            Some(sock) = upgrade_rx.recv(), if worker.is_some() => {
                let own = match (lock.share(), listener.as_fd().try_clone_to_owned()) {
                    (Ok(lock), Ok(listener)) => [lock, listener],
                    (Err(error), _) | (_, Err(error)) => {
                        tracing::warn!("upgrade refused: {error}");
                        lock.keep();
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
                    lock.keep();
                    continue;
                };
                let server_id = persist.server_id().to_owned();
                // SIGTERM during a handoff comes from a service manager
                // restarting the job after a takeover: honour it only if the
                // handoff commits (we exit then anyway).
                let give = handoff::give(&server, sock, server_id, own, config.ready_fault);
                tokio::pin!(give);
                let mut deferred = false;
                let given = loop {
                    tokio::select! {
                        given = &mut give => break given,
                        Some(()) = terminate.recv() => {
                            if !std::mem::replace(&mut deferred, true) {
                                tracing::info!("SIGTERM during an upgrade; deferred until it commits");
                            }
                        }
                    }
                };
                match given {
                    Ok(given) => {
                        committed = Some(given);
                        break;
                    }
                    Err(error) => {
                        tracing::warn!("upgrade aborted: {error:#}");
                        if deferred {
                            tracing::warn!("ignoring the SIGTERM received during the aborted upgrade; still serving");
                        }
                        worker = Some(persist::Worker::spawn(persist, server.clone(), kick.clone(), interval));
                        server.upgrading.store(false, Ordering::SeqCst);
                        lock.keep();
                    }
                }
            }
            () = server.shutdown.notified() => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }
    if let Some(given) = committed {
        return commit(&server, listener, lock, given).await;
    }
    tracing::info!("shutting down");
    // Commit the final state before teardown empties the tree.
    if let Some(worker) = worker {
        worker.finish().await;
    }
    drop(notes_task);
    close_net(&server).await;
    server.auth().flush();
    server.registry.lock().unwrap().shutdown();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(paths::runtime(&home));
    // Give block actors a moment to hang up their children.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(lock);
    Ok(())
}

/// The successor is ready (or may be: [`handoff::Given::Unknown`]) and holds
/// the lock and listening socket: close our copies and connections and
/// tell it so. The socket path stays bound (it is the same socket); the
/// successor rewrites the runtime file.
async fn commit(server: &Server, listener: UnixListener, lock: ServerLock, given: handoff::Given) -> anyhow::Result<()> {
    let (mut sock, unknown) = match given {
        handoff::Given::Committed(sock) => (sock, false),
        handoff::Given::Unknown(sock, error) => {
            tracing::error!("upgrade outcome unknown ({error}): the successor may serve; exiting without serving again");
            (sock, true)
        }
    };
    drop(listener);
    // Remote clients reconnect to the successor, which binds the same
    // identity once this endpoint is gone.
    close_net(server).await;
    // Wait out any hello that took the auth lock before `upgrading` was
    // set, and leave the successor's re-read the latest `last_seen`.
    server.auth().flush();
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
    // Shared: closes our copy only.
    drop(lock);
    let written = tokio::task::spawn_blocking(move || std::io::Write::write_all(&mut sock, handoff::COMMIT)).await?;
    if unknown {
        // The successor may have given up already; nothing else to do.
        if let Err(error) = written {
            tracing::warn!("send COMMIT: {error}");
        }
        tracing::info!("upgrade outcome unknown; exiting");
        return Ok(());
    }
    written?;
    tracing::info!("upgrade committed; exiting");
    Ok(())
}

/// Close the remote endpoint and every remote connection.
async fn close_net(server: &Server) {
    let net = server.net.lock().await.take();
    let Some(net) = net else { return };
    match tokio::time::timeout(Duration::from_secs(3), net.close()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!("close remote endpoint: {error:#}"),
        Err(_) => tracing::warn!("closing the remote endpoint timed out"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_next_host_change_soon_after_a_push_is_its_echo() {
        let clipboard = HostClipboard::default();
        let at = Instant::now();
        let push = |text: &str| *clipboard.pushed_text.lock().unwrap() = Some((text.to_owned(), at));
        push("pasted");
        assert!(clipboard.echo("pasted", at + Duration::from_secs(1)));
        assert!(!clipboard.echo("pasted", at + Duration::from_secs(2)), "a second copy is real");
        push("pasted");
        assert!(!clipboard.echo("other", at), "a real copy");
        assert!(!clipboard.echo("pasted", at), "after another copy, the same text is real too");
        push("pasted");
        assert!(!clipboard.echo("pasted", at + ECHO_WINDOW), "long after the push");
    }
}
