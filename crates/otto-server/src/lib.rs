//! Local daemon hosting the shared durable runtime.
mod connection;
mod dispatch;

use otto_core::{ModelResolver, Runtime};
use otto_proto::{AuthMode, AuthStatus, LoginResult, ModelInfo, RpcError, ServerInfo, paths};
use std::{
    fs::{File, OpenOptions},
    future::Future,
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::UnixListener,
    sync::{Mutex, watch},
    task::{JoinHandle, JoinSet},
};

pub type AuthFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, RpcError>> + Send + 'a>>;

/// Implementations own credential storage and filter models by the active auth mode.
pub trait AuthService: Send + Sync {
    fn status(&self) -> AuthFuture<'_, Vec<AuthStatus>>;
    /// `base_url`, when set, is stored for local/custom providers before signing in. `paste` starts
    /// a browser sign-in whose code the user pastes (`login_complete`).
    fn login(
        &self,
        provider: String,
        mode: AuthMode,
        api_key: Option<String>,
        device: bool,
        base_url: Option<String>,
        paste: bool,
    ) -> AuthFuture<'_, LoginResult>;
    fn login_complete(&self, login_id: String, code: Option<String>) -> AuthFuture<'_, AuthStatus>;
    /// Drops a pending login, aborting an in-flight `login_complete` for it.
    fn login_cancel(&self, login_id: String) -> AuthFuture<'_, ()>;
    fn logout(&self, provider: String) -> AuthFuture<'_, ()>;
    fn list_models(&self, provider: Option<String>) -> AuthFuture<'_, Vec<ModelInfo>>;
}

pub struct ServerConfig {
    pub home: PathBuf,
    pub runtime: Arc<Runtime>,
    pub models: Arc<dyn ModelResolver>,
    pub auth: Arc<dyn AuthService>,
    pub idle_exit: Option<Duration>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Runtime(#[from] otto_core::Error),
    #[error("another otto server owns this home")]
    AlreadyRunning,
    #[error("server task: {0}")]
    Task(#[from] tokio::task::JoinError),
}

pub struct Server;
pub struct ServerHandle {
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<(), Error>>>,
    pub info: ServerInfo,
}
impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
impl ServerHandle {
    /// Request shutdown and await runtime and socket cleanup.
    /// # Errors
    /// Runtime shutdown or daemon task failure.
    pub async fn shutdown(mut self) -> Result<(), Error> {
        self.stop.send_replace(true);
        self.task.take().expect("server task").await?
    }
    /// Wait for RPC or idle shutdown.
    /// # Errors
    /// Runtime shutdown or daemon task failure.
    pub async fn serve_forever(mut self) -> Result<(), Error> {
        self.task.take().expect("server task").await?
    }
}

struct State {
    config: ServerConfig,
    info: ServerInfo,
    stop: watch::Sender<bool>,
    settings: Mutex<serde_json::Value>,
    requests: Arc<tokio::sync::Semaphore>,
}
struct Lease {
    home: PathBuf,
    _lock: File,
}
impl Drop for Lease {
    fn drop(&mut self) {
        for path in [paths::socket(&self.home), paths::runtime(&self.home)] {
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(%error, "daemon cleanup failed");
            }
        }
    }
}
impl Server {
    /// Bind the socket and publish readiness before returning.
    /// # Errors
    /// Home/lock/socket setup fails or another server holds the lease.
    pub async fn start(config: ServerConfig) -> Result<ServerHandle, Error> {
        prepare_home(&config.home)?;
        let lock = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(paths::server_lock(&config.home))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(Error::AlreadyRunning),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let lease = Lease { home: config.home.clone(), _lock: lock };
        remove_if_exists(&paths::socket(&config.home))?;
        let listener = UnixListener::bind(paths::socket(&config.home))?;
        std::fs::set_permissions(paths::socket(&config.home), std::fs::Permissions::from_mode(0o600))?;
        let info = ServerInfo {
            protocol: otto_proto::PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            server_id: uuid::Uuid::new_v4().to_string(),
            pid: std::process::id(),
            started_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().try_into().unwrap_or(i64::MAX),
        };
        let metadata = paths::Runtime {
            pid: info.pid,
            socket: paths::socket(&config.home),
            version: info.version.clone(),
            server_id: info.server_id.clone(),
            started_at: info.started_at,
        };
        let settings = match tokio::fs::read(config.home.join("config.json")).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::from)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
            Err(e) => return Err(e.into()),
        };
        config.runtime.set_config(dispatch::defaults(&settings).map_err(io::Error::other)?)?;
        let (stop, _) = watch::channel(false);
        let uid = current_uid();
        let state = Arc::new(State {
            config,
            info: info.clone(),
            stop: stop.clone(),
            settings: Mutex::new(settings),
            requests: Arc::new(tokio::sync::Semaphore::new(256)),
        });
        tokio::fs::write(paths::runtime(&state.config.home), serde_json::to_vec(&metadata).map_err(io::Error::from)?).await?;
        let task = tokio::spawn(run(state, listener, lease, uid));
        Ok(ServerHandle { stop, task: Some(task), info })
    }
    /// Start and wait until shutdown.
    /// # Errors
    /// Startup or shutdown fails.
    pub async fn serve_forever(config: ServerConfig) -> Result<(), Error> {
        Self::start(config).await?.serve_forever().await
    }
}

/// Open the private append-only tracing sink; the CLI installs its subscriber.
/// # Errors
/// The home or log cannot be created.
pub fn init_logging(home: &Path) -> io::Result<File> {
    prepare_home(home)?;
    let file = OpenOptions::new().create(true).append(true).mode(0o600).open(paths::log(home))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}
fn prepare_home(home: &Path) -> io::Result<()> {
    std::fs::create_dir_all(home)?;
    if std::fs::metadata(home)?.uid() != current_uid() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "home belongs to another user"));
    }
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))
}
fn current_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no arguments or failure mode on supported Unix targets.
    unsafe { geteuid() }
}
fn same_uid(own: u32, peer: u32) -> bool {
    own == peer
}
fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
async fn stopped(receiver: &mut watch::Receiver<bool>) {
    if *receiver.borrow() {
        return;
    }
    let _ = receiver.wait_for(|value| *value).await;
}
async fn run(state: Arc<State>, listener: UnixListener, _lease: Lease, uid: u32) -> Result<(), Error> {
    let mut stop = state.stop.subscribe();
    let mut clients = JoinSet::new();
    let mut idle_since = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut globals = state.config.runtime.events();
    let (index, _) = tokio::sync::broadcast::channel(256);
    let mut seq = 0i64;
    loop {
        tokio::select! {
            biased;
            () = stopped(&mut stop) => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) if stream.peer_cred().is_ok_and(|cred| same_uid(uid, cred.uid())) => {
                    idle_since = tokio::time::Instant::now();
                    clients.spawn(connection::serve(state.clone(), stream, index.subscribe()));
                },
                Ok(_) => tracing::warn!("rejected foreign peer uid"),
                Err(error) => { tracing::error!(%error, "accept failed"); state.stop.send_replace(true); break; },
            },
            Some(_) = clients.join_next() => { idle_since = tokio::time::Instant::now(); },
            event = globals.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => otto_proto::Event::SessionsChanged {},
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                seq += 1;
                let _ = index.send((seq, event));
            },
            _ = tick.tick(), if state.config.idle_exit.is_some() => {
                if !clients.is_empty() || state.config.runtime.busy() {
                    idle_since = tokio::time::Instant::now();
                } else if idle_since.elapsed() >= state.config.idle_exit.unwrap_or_default() {
                    state.stop.send_replace(true);
                    break;
                }
            }
        }
    }
    drop(listener);
    state.stop.send_replace(true);
    while clients.join_next().await.is_some() {}
    state.config.runtime.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
