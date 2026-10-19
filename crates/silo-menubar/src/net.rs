//! Daemon monitor on a tokio thread: keeps a passive control connection,
//! mirrors server info and the session count into [`Shared`], notices a
//! server of a different build, and runs Quit Silo.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use silo_client::{Control, DifferentBuild, Events, PING_TIMEOUT_LOCAL, mismatch};
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
    /// A server of a different build holds the state directory.
    pub incompatible: Option<DifferentBuild>,
    pub busy: Option<&'static str>,
    pub error: Option<String>,
    /// A server holds the socket but stopped answering pings.
    pub unresponsive: bool,
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
            (None, _) if self.unresponsive => "Silo — Not responding".to_owned(),
            (None, Some(_)) => format!("Silo — {}", DifferentBuild::LABEL),
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
        // A hung server accepts but never answers the hello.
        let connect = tokio::time::timeout(PING_TIMEOUT_LOCAL, Control::connect_local(&socket, NAME, true));
        let Ok(connected) = connect.await else {
            if shared.state.lock().is_ok_and(|s| !s.unresponsive) {
                shared.update(|s| s.unresponsive = true);
            }
            if wait(&config, &shared, &mut cmds).await {
                return;
            }
            continue;
        };
        match connected {
            Ok((control, events)) => {
                let unresponsive = tokio::select! {
                    () = control.clone().heartbeat() => true,
                    done = serve(&config, &shared, &control, events, &mut cmds) => {
                        if done {
                            return;
                        }
                        false
                    }
                };
                shared.update(|s| {
                    s.server = None;
                    s.sessions = 0;
                    s.unresponsive = unresponsive;
                });
            }
            Err(error) => {
                // Another build: describe it, never start a second server.
                let incompatible = mismatch::different_build(&config.home, &error).await;
                let changed = shared.state.lock().is_ok_and(|s| s.incompatible != incompatible || s.unresponsive);
                if changed {
                    shared.update(|s| {
                        s.incompatible = incompatible;
                        s.unresponsive = false;
                    });
                }
            }
        }
        if wait(&config, &shared, &mut cmds).await {
            return;
        }
    }
}

/// Disconnected: poll, still accepting commands. True once the menu is gone.
async fn wait(config: &Config, shared: &Shared, cmds: &mut mpsc::UnboundedReceiver<Cmd>) -> bool {
    tokio::select! {
        () = tokio::time::sleep(POLL) => false,
        cmd = cmds.recv() => match cmd {
            Some(cmd) => {
                command(config, shared, None, cmd).await;
                false
            }
            None => true,
        },
    }
}

/// Connected: mirror events and run commands until the connection drops.
/// True once the menu is gone.
async fn serve(config: &Config, shared: &Shared, control: &Control, mut events: Events, cmds: &mut mpsc::UnboundedReceiver<Cmd>) -> bool {
    let _ = control.subscribe().await;
    let sessions = list(control).await.len();
    let info = control.hello().server.clone();
    shared.update(|s| {
        s.server = Some(info);
        s.sessions = sessions;
        s.incompatible = None;
        s.error = None;
        s.busy = None;
        s.unresponsive = false;
    });
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some((_, Event::SessionsChanged { sessions })) => shared.update(|s| s.sessions = sessions.len()),
                Some(_) => {}
                None => return false,
            },
            cmd = cmds.recv() => match cmd {
                Some(cmd) => command(config, shared, Some(control), cmd).await,
                None => return true,
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
                    control.call_value(Method::ServerShutdown {}).await.inspect_err(|_| paths::clear_quit(&config.home, None))?;
                    let _ = tokio::time::timeout(Duration::from_secs(5), control.closed()).await;
                } else if let Some(server) = incompatible {
                    shared.update(|s| s.busy = Some("Quitting…"));
                    if let Some(pid) = server.pid {
                        paths::mark_quit(&config.home, pid)?;
                    }
                    mismatch::stop(&config.home, &server).await.inspect_err(|_| paths::clear_quit(&config.home, None))?;
                }
                shared.quit.store(true, Ordering::Release);
            }
        }
        Ok(())
    }
    .await;
    shared.update(|s| {
        s.busy = None;
        s.error = result.err().map(|e| {
            tracing::warn!("menu command: {e:#}");
            "Could not quit Silo.".to_owned()
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn other(sessions: Option<usize>) -> DifferentBuild {
        DifferentBuild { build: Some("0123456789abcdef".into()), pid: Some(1), version: None, sessions }
    }

    #[test]
    fn status_line() {
        let mut state = MenuState::default();
        assert_eq!(state.status(), "Silo — Not running");
        assert_eq!(state.running_sessions(), 0);
        state.incompatible = Some(other(Some(2)));
        assert_eq!(state.status(), "Silo — Different server build");
        assert_eq!(state.running_sessions(), 2);
        state.unresponsive = true;
        assert_eq!(state.status(), "Silo — Not responding");
        state.server = Some(ServerInfo {
            version: "x".into(),
            pid: 2,
            server_id: String::new(),
            endpoint_id: String::new(),
            remote_error: None,
            endpoint_addr: None,
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
