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

use silo_client::{Control, Events, IncompatibleServer, PING_TIMEOUT_LOCAL, mismatch};
use silo_proto::{
    control::{ClientRole, Event, Method, ServerInfo},
    model::SessionSummary,
    paths,
};
use tokio::sync::mpsc;

#[derive(Clone, Debug, Default)]
pub struct MenuState {
    pub server: Option<ServerInfo>,
    pub sessions: usize,
    /// A server of a different build holds the state directory.
    pub incompatible: Option<IncompatibleServer>,
    pub busy: Option<&'static str>,
    pub error: Option<String>,
    /// A server holds the socket but stopped answering pings.
    pub unresponsive: bool,
}

impl MenuState {
    /// The menu's status line.
    pub fn status(&self) -> String {
        let name = silo_proto::CHANNEL.app_name();
        if let Some(busy) = self.busy {
            return format!("{name} — {busy}");
        }
        match (&self.server, &self.incompatible) {
            (Some(_), _) => match self.sessions {
                1 => format!("{name} — Running · 1 session"),
                n => format!("{name} — Running · {n} sessions"),
            },
            (None, _) if self.unresponsive => format!("{name} — Not responding"),
            (None, Some(_)) => format!("{name} — {}", IncompatibleServer::LABEL),
            (None, None) => format!("{name} — Not running"),
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
                let incompatible = mismatch::incompatible_server(&config.home, &error).await;
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

/// Stop the server: the menu's Quit is the user's, like the Silo app's
/// own calls, so it goes over an `app` connection that needs no approval.
/// That connection never subscribes: this agent shows no approval cards
/// and runs no forwarded requests.
async fn shutdown(config: &Config) -> anyhow::Result<()> {
    silo_client::set_role(ClientRole::App);
    let connected = Control::connect_local(&paths::socket(&config.home), NAME, true).await;
    silo_client::set_role(ClientRole::Cli);
    let (control, _events) = connected?;
    control.call_value(Method::ServerShutdown { force: false }).await?;
    Ok(())
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
                    shutdown(config).await.inspect_err(|_| paths::clear_quit(&config.home, None))?;
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
            format!("Could not quit {}.", silo_proto::CHANNEL.app_name())
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn other(sessions: Option<usize>) -> IncompatibleServer {
        IncompatibleServer { build: Some("0123456789abcdef".into()), pid: Some(1), version: None, sessions }
    }

    #[test]
    fn status_line() {
        let mut state = MenuState::default();
        assert_eq!(state.status(), format!("{} — Not running", silo_proto::CHANNEL.app_name()));
        assert_eq!(state.running_sessions(), 0);
        state.incompatible = Some(other(Some(2)));
        assert_eq!(state.status(), format!("{} — Incompatible server", silo_proto::CHANNEL.app_name()));
        assert_eq!(state.running_sessions(), 2);
        state.unresponsive = true;
        assert_eq!(state.status(), format!("{} — Not responding", silo_proto::CHANNEL.app_name()));
        state.server = Some(ServerInfo {
            version: "x".into(),
            pid: 2,
            server_id: String::new(),
            endpoint_id: String::new(),
            hostname: String::new(),
            remote_error: None,
            endpoint_addr: None,
            idle_exit_secs: 0,
            started_at: 0,
        });
        state.sessions = 1;
        assert_eq!(state.status(), format!("{} — Running · 1 session", silo_proto::CHANNEL.app_name()));
        state.sessions = 3;
        assert_eq!(state.status(), format!("{} — Running · 3 sessions", silo_proto::CHANNEL.app_name()));
        assert_eq!(state.running_sessions(), 3);
        state.busy = Some("Quitting…");
        assert_eq!(state.status(), format!("{} — Quitting…", silo_proto::CHANNEL.app_name()));
    }
}
