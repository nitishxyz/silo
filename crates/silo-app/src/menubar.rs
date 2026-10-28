//! The menu-bar agent, kept up while the app runs. Started at launch; when
//! it exits (crashed, killed) and no other agent holds `menubar.lock` (one
//! of an earlier launch, or the `silo` CLI's, may), a new one starts.
//! Never while Silo is quitting (the agent's Quit Silo marks it), and with
//! growing pauses when it keeps exiting right away. How each one ended goes
//! to the app log; the agent's own output to `menubar.log`.

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// How often a free lock is looked for.
const POLL: Duration = Duration::from_secs(5);
/// An agent up this long was not crash-looping.
const STEADY: Duration = Duration::from_secs(30);
const MAX_PAUSE: Duration = Duration::from_secs(300);

pub fn start(agent: PathBuf, home: PathBuf) {
    if std::env::var_os("SILO_NO_MENUBAR").is_some() {
        return;
    }
    let spawned = std::thread::Builder::new().name("menubar-agent".into()).spawn(move || supervise(&agent, &home));
    if let Err(error) = spawned {
        tracing::warn!("menu-bar agent: {error}");
    }
}

fn supervise(agent: &Path, home: &Path) {
    let mut quick = 0;
    loop {
        let started = Instant::now();
        match spawn(agent, home) {
            // Waited for, so it never lingers as a zombie.
            Ok(mut child) => match child.wait() {
                Ok(status) => tracing::info!(%status, secs = started.elapsed().as_secs(), "menu-bar agent exited"),
                Err(error) => return tracing::warn!("menu-bar agent: {error}"),
            },
            Err(error) => tracing::warn!("menu-bar agent {}: {error}", agent.display()),
        }
        quick = if started.elapsed() < STEADY { quick + 1 } else { 0 };
        let mut wait = pause(quick);
        loop {
            std::thread::sleep(wait);
            wait = POLL;
            if !quitting(home) && agent.is_file() && !held(home) {
                break;
            }
        }
    }
}

fn spawn(agent: &Path, home: &Path) -> std::io::Result<Child> {
    let mut command = Command::new(agent);
    // Not as this app: the agent would register as Silo.app and take its
    // `silo://` URLs (the menu's Settings… and Permissions… did nothing).
    for var in silo_proto::APP_IDENTITY_ENV {
        command.env_remove(var);
    }
    command.env("SILO_HOME", home).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Outlives the app; exits at once when another agent holds the lock.
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    command.spawn()
}

/// The pause after an agent exits: [`POLL`], doubling with each exit in a
/// row that came soon after its start, up to [`MAX_PAUSE`].
fn pause(quick: u32) -> Duration {
    POLL.saturating_mul(1 << quick.min(6)).min(MAX_PAUSE)
}

/// Quit Silo is under way (cleared by the app on its next server).
fn quitting(home: &Path) -> bool {
    silo_proto::paths::quit_marker(home).exists()
}

/// Whether an agent holds `menubar.lock` (its exclusive `flock`).
fn held(home: &Path) -> bool {
    let Ok(file) = std::fs::File::open(home.join("menubar.lock")) else { return false };
    matches!(file.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pauses_grow_while_the_agent_keeps_exiting_at_once() {
        assert_eq!(pause(0), POLL);
        assert_eq!(pause(1), POLL * 2);
        assert_eq!(pause(3), POLL * 8);
        assert_eq!(pause(30), MAX_PAUSE);
    }

    #[test]
    fn an_agent_holding_the_lock_is_seen() {
        let home = std::env::temp_dir().join(format!("silo-menubar-lock-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        assert!(!held(&home), "no lock file: nobody");
        let lock = std::fs::File::create(home.join("menubar.lock")).unwrap();
        assert!(!held(&home), "unlocked");
        lock.lock().unwrap();
        assert!(held(&home));
        lock.unlock().unwrap();
        assert!(!held(&home));
        assert!(!quitting(&home));
        std::fs::write(silo_proto::paths::quit_marker(&home), "1").unwrap();
        assert!(quitting(&home));
        let _ = std::fs::remove_dir_all(home);
    }
}
