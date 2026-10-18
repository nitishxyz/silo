//! Daemon monitor on a tokio thread: keeps a passive control connection,
//! mirrors server info and the session count into [`Shared`], notices a
//! server of another version, and runs Quit Silo.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use silo_client::{Control, Incompatible, compat};
use silo_proto::{
    control::{Event, Method, ServerInfo},
    model::SessionSummary,
    paths,
};
use tokio::sync::mpsc;

#[derive(Clone, Debug, Default)]
pub struct MenuState {
    pub server: Option<ServerInfo>,
    pub sessions: usize,
    /// A server of another version holds the state directory.
    pub incompatible: Option<Incompatible>,
    pub busy: Option<&'static str>,
    pub error: Option<String>,
}

impl MenuState {
    /// The menu's status line.
    pub fn status(&self) -> String {
        if let Some(busy) = self.busy {
            return format!("Silo — {busy}");
        }
        match (&self.server, &self.incompatible) {
            (Some(_), _) => match self.sessions {
                1 => "Silo — Running · 1 session".to_owned(),
                n => format!("Silo — Running · {n} sessions"),
            },
            (None, Some(server)) => format!("Silo — {} server running", server.age()),
            (None, None) => "Silo — Not running".to_owned(),
        }
    }

    /// Sessions Quit Silo would end.
    pub fn running_sessions(&self) -> usize {
        match (&self.server, &self.incompatible) {
            (Some(_), _) => self.sessions,
            (None, Some(server)) => server.sessions.unwrap_or(0),
            (None, None) => 0,
        }
    }
}

#[derive(Default)]
pub struct Shared {
    pub state: Mutex<MenuState>,
    /// Bumped on every state change; the menu rebuilds when it moves.
    pub generation: AtomicU64,
    /// Set once "Quit Silo" has stopped the server; the agent then exits.
    pub quit: AtomicBool,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut MenuState)) {
        if let Ok(mut state) = self.state.lock() {
            f(&mut state);
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub enum Cmd {
    /// Stop the server (marked as a quit so the app quits too), then exit.
    Quit,
}

pub struct Config {
    pub home: PathBuf,
}

const NAME: &str = "silo-menubar";
/// How often to look for a server while none is connected.
const POLL: Duration = Duration::from_secs(2);

pub fn spawn(config: Config, shared: Arc<Shared>) -> mpsc::UnboundedSender<Cmd> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("silo-menubar-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio");
            rt.block_on(run(config, shared, rx));
        })
        .expect("spawn net thread");
    tx
}

async fn run(config: Config, shared: Arc<Shared>, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
    let socket = paths::socket(&config.home);
    loop {
        // Connected: mirror events, run commands until the connection drops.
        match Control::connect_with(&socket, NAME, true).await {
            Ok((control, mut events)) => {
                let _ = control.subscribe().await;
                let sessions = list(&control).await.len();
                let info = control.hello().server.clone();
                shared.update(|s| {
                    s.server = Some(info);
                    s.sessions = sessions;
                    s.incompatible = None;
                    s.error = None;
                    s.busy = None;
                });
                loop {
                    tokio::select! {
                        event = events.recv() => match event {
                            Some((_, Event::SessionsChanged { sessions })) => shared.update(|s| s.sessions = sessions.len()),
                            Some(_) => {}
                            None => break,
                        },
                        cmd = cmds.recv() => match cmd {
                            Some(cmd) => command(&config, &shared, Some(&control), cmd).await,
                            None => return,
                        },
                    }
                }
                shared.update(|s| {
                    s.server = None;
                    s.sessions = 0;
                });
            }
            Err(error) => {
                // Another version: describe it, never start a second server.
                let incompatible = match compat::mismatch(&error) {
                    Some(mismatch) => Some(compat::inspect(&config.home, mismatch).await),
                    None => None,
                };
                let changed = shared.state.lock().is_ok_and(|s| s.incompatible != incompatible);
                if changed {
                    shared.update(|s| s.incompatible = incompatible);
                }
            }
        }
        // Disconnected: poll, still accepting commands.
        tokio::select! {
            () = tokio::time::sleep(POLL) => {}
            cmd = cmds.recv() => match cmd {
                Some(cmd) => command(&config, &shared, None, cmd).await,
                None => return,
            },
        }
    }
}

async fn list(control: &Control) -> Vec<SessionSummary> {
    control.call(Method::SessionList {}).await.unwrap_or_default()
}

async fn command(config: &Config, shared: &Shared, control: Option<&Control>, cmd: Cmd) {
    tracing::info!(?cmd, "menu command");
    let result: anyhow::Result<()> = async {
        match cmd {
            Cmd::Quit => {
                let incompatible = shared.state.lock().ok().and_then(|s| s.incompatible.clone());
                if let Some(control) = control {
                    shared.update(|s| s.busy = Some("Quitting…"));
                    // The app reads this marker when its connection drops
                    // and quits instead of reconnecting.
                    paths::mark_quit(&config.home, control.hello().server.pid)?;
                    control
                        .call_value(Method::ServerShutdown {})
                        .await
                        .inspect_err(|_| paths::clear_quit(&config.home, None))?;
                    let _ = tokio::time::timeout(Duration::from_secs(5), control.closed()).await;
                } else if let Some(server) = incompatible {
                    shared.update(|s| s.busy = Some("Quitting…"));
                    if let Some(pid) = server.pid {
                        paths::mark_quit(&config.home, pid)?;
                    }
                    compat::stop(&config.home, &server).await.inspect_err(|_| paths::clear_quit(&config.home, None))?;
                }
                shared.quit.store(true, Ordering::Release);
            }
        }
        Ok(())
    }
    .await;
    shared.update(|s| {
        s.busy = None;
        s.error = result.err().map(|e| format!("{e:#}"));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn older(sessions: Option<usize>) -> Incompatible {
        Incompatible { protocol: Some(1), pid: Some(1), version: None, sessions }
    }

    #[test]
    fn status_line() {
        let mut state = MenuState::default();
        assert_eq!(state.status(), "Silo — Not running");
        assert_eq!(state.running_sessions(), 0);
        state.incompatible = Some(older(Some(2)));
        assert_eq!(state.status(), "Silo — Older server running");
        assert_eq!(state.running_sessions(), 2);
        state.server = Some(ServerInfo {
            version: "x".into(),
            ghostty_rev: String::new(),
            snapshot_version: 1,
            pid: 2,
            server_id: String::new(),
            started_at: 0,
        });
        state.sessions = 1;
        assert_eq!(state.status(), "Silo — Running · 1 session");
        state.sessions = 3;
        assert_eq!(state.status(), "Silo — Running · 3 sessions");
        assert_eq!(state.running_sessions(), 3);
        state.busy = Some("Quitting…");
        assert_eq!(state.status(), "Silo — Quitting…");
    }
}
