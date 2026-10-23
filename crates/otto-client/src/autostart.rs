use crate::{ClientError, Connection, Result};
use otto_proto::paths;
use std::{
    ffi::OsStr,
    fs::OpenOptions,
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

fn absent(error: &ClientError) -> bool {
    matches!(error, ClientError::Io { kind: std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused, .. })
}
/// Connect or launch `binary_path` with exactly `args`, in a new process group.
/// The start lock serializes launchers; the server lock prevents duplicate daemons.
/// # Errors
/// Connection, lock, spawn, or five-second readiness timeout.
pub async fn connect_or_start(home: &Path, binary_path: &Path, args: &[&str]) -> Result<Connection> {
    start(home, binary_path, args, &[]).await
}
/// Make sure a daemon serves `home`, launching `binary` with `args` and `envs` (plus
/// `OTTO_SILO_HOME`) in a new process group when none does.
/// # Errors
/// Connection, lock, spawn, or five-second readiness timeout.
pub async fn ensure_started(home: &Path, binary: &Path, args: &[&str], envs: &[(&str, &OsStr)]) -> Result<()> {
    start(home, binary, args, envs).await.map(drop)
}
async fn start(home: &Path, binary_path: &Path, args: &[&str], envs: &[(&str, &OsStr)]) -> Result<Connection> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    match Connection::connect(home).await {
        Ok(c) => return Ok(c),
        Err(e) if absent(&e) => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(home)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(paths::start_lock(home))?;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ClientError::Timeout);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    match Connection::connect(home).await {
        Ok(c) => return Ok(c),
        Err(e) if absent(&e) => {}
        Err(e) => return Err(e),
    }
    let server_lock = OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(home))?;
    let running = match server_lock.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    };
    drop(server_lock);
    if !running {
        let log = OpenOptions::new().create(true).append(true).open(paths::log(home))?;
        let mut child = Command::new(binary_path)
            .args(args)
            .envs(envs.iter().copied())
            .env(format!("{}_HOME", otto_proto::ENV_PREFIX), home)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    let mut delay = Duration::from_millis(20);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, Connection::connect(home)).await {
            Ok(Ok(c)) => return Ok(c),
            Ok(Err(e)) if absent(&e) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(ClientError::Timeout),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ClientError::Timeout);
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(200));
    }
}
