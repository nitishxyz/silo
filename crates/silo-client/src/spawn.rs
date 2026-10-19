//! Find the running server or start a detached one.

use std::{
    os::fd::AsFd,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use silo_proto::paths;

use crate::{Control, Events, mismatch};

/// True if a server accepts connections for `home`.
pub async fn server_running(home: &Path) -> bool {
    tokio::net::UnixStream::connect(paths::socket(home)).await.is_ok()
}

/// Connect to the server for `home`, starting `server_exe server --daemon`
/// if none is running. Concurrent callers serialise on `start.lock`.
///
/// Never starts a second server while one holds the server lock: an
/// server of another build fails with [`mismatch::DifferentBuild`] at once, one that
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
    if mismatch::server_lock_held(home) {
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
    start_daemon(home, server_exe, &[])?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Control::connect(&socket, client_name).await {
            Ok(connected) => {
                drop(lock);
                return Ok(connected);
            }
            Err(e) if Instant::now() >= deadline => {
                incompatible(home, &e).await?;
                tracing::warn!("server did not start: {e:#}; see {}", paths::log(home).display());
                return Err(e.context("the Silo server did not start"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
}

/// Run `server_exe server --daemon <extra>` for `home`, logging to its log
/// file; returns once the daemon detached.
pub(crate) fn start_daemon(home: &Path, server_exe: &Path, extra: &[&str]) -> anyhow::Result<()> {
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths::log(home))?;
    let status = Command::new(server_exe)
        .args(["server", "--daemon"])
        .args(extra)
        .env("SILO_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .status()
        .map_err(|error| {
            tracing::warn!("start {}: {error}", server_exe.display());
            anyhow::anyhow!("could not start the Silo server")
        })?;
    if !status.success() {
        tracing::warn!("{} server --daemon failed ({status})", server_exe.display());
        bail!("could not start the Silo server");
    }
    Ok(())
}

/// Run `server_exe server <args>` for `home` as our child in its own
/// process group (no double fork), logging to its log file, so the caller
/// can see it exit.
pub(crate) fn spawn_server(home: &Path, server_exe: &Path, args: &[&str]) -> anyhow::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths::log(home))?;
    Command::new(server_exe)
        .arg("server")
        .args(args)
        .env("SILO_HOME", home)
        .current_dir("/")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|error| {
            tracing::warn!("start {}: {error}", server_exe.display());
            anyhow::anyhow!("could not start the Silo server")
        })
}

/// Turn a build rejection by the server holding `home` into
/// [`mismatch::DifferentBuild`]; other errors pass.
async fn incompatible(home: &Path, error: &anyhow::Error) -> anyhow::Result<()> {
    match mismatch::different_build(home, error).await {
        Some(server) => Err(server.into()),
        None => Ok(()),
    }
}
