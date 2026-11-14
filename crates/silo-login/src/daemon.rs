//! `silo login-server run`: the daemon.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, bail};
use silo_transport::{Identity, Net, NetOptions, RelayConfig};

use crate::{
    Broker, admin,
    spawner::{AsUser, Spawner, Unprivileged},
    users::{self, System, Users},
};

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub accounts: PathBuf,
    pub state_dir: PathBuf,
    pub relay: RelayConfig,
    pub port: Option<u16>,
    pub mdns: bool,
    /// Not root: serve only this process's own account, started without
    /// changing user (`docs/LOGIN_SERVER.md` §8).
    pub unprivileged: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            accounts: crate::DEFAULT_ACCOUNTS.into(),
            state_dir: crate::DEFAULT_STATE_DIR.into(),
            relay: RelayConfig::Default,
            port: None,
            mdns: true,
            unprivileged: false,
        }
    }
}

/// The endpoint identity in `state_dir`.
///
/// # Errors
/// Unreadable or invalid key.
pub fn identity(state_dir: &std::path::Path) -> anyhow::Result<Identity> {
    Identity::load_or_create(state_dir.join("iroh"))
}

/// Run the login server until SIGTERM or SIGINT; SIGHUP re-reads the
/// accounts file.
///
/// # Errors
/// Not root without `--unprivileged`, or setup failed.
pub async fn run(options: RunOptions) -> anyhow::Result<()> {
    let root = rustix::process::geteuid().is_root();
    if !root && !options.unprivileged {
        bail!("the login server runs as root (or pass --unprivileged to serve only your own account)");
    }
    let exe = std::env::current_exe().context("this binary's path")?;
    let (spawner, only): (Arc<dyn Spawner>, Option<String>) = if options.unprivileged {
        let me = users::current().context("this process's account")?;
        (Arc::new(Unprivileged::new(exe, silo_proto::paths::home())), Some(me.name))
    } else {
        (Arc::new(AsUser::new(exe)), None)
    };
    let users: Arc<dyn Users> = Arc::new(System);
    let broker = Arc::new(Broker::new(options.accounts.clone(), users, spawner, only)?);
    // The admin socket first: it creates the state directory 0700.
    let listener = admin::bind(&options.state_dir)?;
    let identity = identity(&options.state_dir)?;
    let mut net =
        Net::bind_with(&identity, NetOptions { relay: options.relay.clone(), mdns: options.mdns, pin_initial_path: false, port: options.port })
            .await
            .context("bind the login server's endpoint")?;
    let incoming = net.serve_login(64)?;
    let net = Arc::new(net);
    tracing::info!(id = %net.id(), accounts = %options.accounts.display(), unprivileged = options.unprivileged, "silo login server listening");
    let addr: admin::Addr = {
        let net = net.clone();
        Arc::new(move || net.addr())
    };
    let admin_task = tokio::spawn(admin::serve(listener, broker.clone(), addr));
    let serve_task = tokio::spawn(broker.clone().serve(incoming));
    use tokio::signal::unix::{SignalKind, signal};
    let (mut hup, mut term, mut int) = (signal(SignalKind::hangup())?, signal(SignalKind::terminate())?, signal(SignalKind::interrupt())?);
    loop {
        tokio::select! {
            _ = hup.recv() => {
                if let Err(error) = broker.reload().await {
                    tracing::error!("reload: {error:#}; the accounts in force are kept");
                }
            }
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    tracing::info!("silo login server stopping");
    admin_task.abort();
    serve_task.abort();
    let _ = std::fs::remove_file(admin::socket(&options.state_dir));
    if let Ok(net) = Arc::try_unwrap(net) {
        let _ = net.close().await;
    }
    Ok(())
}
