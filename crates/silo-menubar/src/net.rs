//! Daemon monitor on a tokio thread: keeps a passive control connection,
//! mirrors server info and sessions into [`Shared`], and runs menu commands.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use silo_client::Control;
use silo_proto::{
    control::{Created, Event, Method, ServerInfo, SpawnSpec},
    model::SessionSummary,
    paths,
};
use tokio::sync::mpsc;

#[derive(Clone, Debug, Default)]
pub struct MenuState {
    pub server: Option<ServerInfo>,
    /// Oldest first.
    pub sessions: Vec<SessionSummary>,
    pub busy: Option<&'static str>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Shared {
    pub state: Mutex<MenuState>,
    /// Bumped on every state change; the menu rebuilds when it moves.
    pub generation: AtomicU64,
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
    OpenSession(String),
    NewSession,
    Start,
    Stop,
    Restart,
}

pub struct Config {
    pub home: PathBuf,
    pub server_exe: PathBuf,
    pub app: crate::AppLauncher,
}

const NAME: &str = "silo-menubar";

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
        if let Ok((control, mut events)) = Control::connect_with(&socket, NAME, true).await {
            let _ = control.subscribe().await;
            let sessions = list(&control).await;
            let info = control.hello().server.clone();
            shared.update(|s| {
                s.server = Some(info);
                s.sessions = sessions;
                s.error = None;
                s.busy = None;
            });
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Some((_, Event::SessionsChanged { mut sessions })) => {
                            sessions.sort_by_key(|s| s.created_at);
                            shared.update(|s| s.sessions = sessions);
                        }
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
                s.sessions.clear();
            });
        }
        // Disconnected: poll for a server, still accepting Start.
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
            cmd = cmds.recv() => match cmd {
                Some(cmd) => command(&config, &shared, None, cmd).await,
                None => return,
            },
        }
    }
}

async fn list(control: &Control) -> Vec<SessionSummary> {
    let mut sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await.unwrap_or_default();
    sessions.sort_by_key(|s| s.created_at);
    sessions
}

/// Adopt a running server, else start one that stays up (no idle exit).
async fn start(config: &Config) -> anyhow::Result<Control> {
    let socket = paths::socket(&config.home);
    if let Ok((control, _)) = Control::connect_with(&socket, NAME, true).await {
        return Ok(control);
    }
    std::fs::create_dir_all(&config.home)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths::log(&config.home))?;
    let exe = config.server_exe.clone();
    let home = config.home.clone();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(exe)
            .args(["server", "--daemon", "--idle-exit", "0"])
            .env("SILO_HOME", home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log.try_clone()?))
            .stderr(std::process::Stdio::from(log))
            .status()
    })
    .await??;
    anyhow::ensure!(status.success(), "server failed to start ({status})");
    for _ in 0..200 {
        if let Ok((control, _)) = Control::connect_with(&socket, NAME, true).await {
            return Ok(control);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("server did not come up; see {}", paths::log(&config.home).display())
}

async fn command(config: &Config, shared: &Shared, control: Option<&Control>, cmd: Cmd) {
    tracing::info!(?cmd, "menu command");
    let result: anyhow::Result<()> = async {
        match cmd {
            Cmd::OpenSession(id) => {
                if let Some(control) = control {
                    let _ = control.call_value(Method::UiOpenSession { session_id: id.clone() }).await;
                }
                config.app.open(Some(&id));
            }
            Cmd::NewSession => {
                let started;
                let control = match control {
                    Some(c) => c,
                    None => {
                        started = start(config).await?;
                        &started
                    }
                };
                let home = std::env::var("HOME").ok();
                let created: Created =
                    control.call(Method::SessionCreate { name: None, spawn: SpawnSpec { cwd: home, command: Vec::new(), size: None } }).await?;
                let _ = control.call_value(Method::UiOpenSession { session_id: created.session_id.clone() }).await;
                config.app.open(Some(&created.session_id));
            }
            Cmd::Start => {
                shared.update(|s| s.busy = Some("Starting…"));
                start(config).await?;
            }
            Cmd::Stop => {
                if let Some(control) = control {
                    shared.update(|s| s.busy = Some("Stopping…"));
                    control.call_value(Method::ServerShutdown {}).await?;
                }
            }
            Cmd::Restart => {
                shared.update(|s| s.busy = Some("Restarting…"));
                if let Some(control) = control {
                    let _ = control.call_value(Method::ServerShutdown {}).await;
                    control.closed().await;
                }
                // The old server releases its lock as it exits.
                let mut last = None;
                for _ in 0..50 {
                    match start(config).await {
                        Ok(_) => {
                            last = None;
                            break;
                        }
                        Err(e) => last = Some(e),
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if let Some(e) = last {
                    return Err(e);
                }
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
