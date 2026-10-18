//! Find the running server or start a detached one.

use std::{
    os::fd::AsFd,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use silo_proto::paths;

use crate::{Control, Events, compat};

/// True if a server accepts connections for `home`.
pub async fn server_running(home: &Path) -> bool {
    tokio::net::UnixStream::connect(paths::socket(home)).await.is_ok()
}

/// Connect to the server for `home`, starting `server_exe server --daemon`
/// if none is running. Concurrent callers serialise on `start.lock`.
///
/// Never starts a second server while one holds the server lock: an
/// incompatible server fails with [`compat::Incompatible`] at once, one that
/// does not answer fails after a short wait.
///
/// # Errors
/// The server could not be started or did not come up in time.
pub async fn connect_or_spawn(home: &Path, server_exe: &Path, client_name: &str) -> anyhow::Result<(Control, Events)> {
    let socket = paths::socket(home);
    match Control::connect(&socket, client_name).await {
        Ok(connected) => return Ok(connected),
        Err(e) => incompatible(home, &e).await?,
    }
    std::fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    let lock_path = paths::start_lock(home);
    let lock = tokio::task::spawn_blocking(move || -> anyhow::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_path)?;
        rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::LockExclusive)?;
        Ok(file)
    })
    .await??;
    match Control::connect(&socket, client_name).await {
        Ok(connected) => return Ok(connected),
        Err(e) => incompatible(home, &e).await?,
    }
    if compat::server_lock_held(home) {
        // Starting up, or alive without a socket: wait, don't spawn.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match Control::connect(&socket, client_name).await {
                Ok(connected) => return Ok(connected),
                Err(e) => incompatible(home, &e).await?,
            }
            if Instant::now() >= deadline {
                bail!("the Silo server is running but not responding");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths::log(home))?;
    let status = Command::new(server_exe)
        .args(["server", "--daemon"])
        .env("SILO_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .status()
        .with_context(|| format!("start {}", server_exe.display()))?;
    if !status.success() {
        bail!("{} server --daemon failed ({status})", server_exe.display());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Control::connect(&socket, client_name).await {
            Ok(connected) => {
                drop(lock);
                return Ok(connected);
            }
            Err(e) if Instant::now() >= deadline => {
                incompatible(home, &e).await?;
                return Err(e.context(format!("server did not start; see {}", paths::log(home).display())));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
}

/// Turn a protocol rejection into [`compat::Incompatible`]; other errors
/// pass.
async fn incompatible(home: &Path, error: &anyhow::Error) -> anyhow::Result<()> {
    match compat::mismatch(error) {
        Some(mismatch) => Err(compat::inspect(home, mismatch).await.into()),
        None => Ok(()),
    }
}
