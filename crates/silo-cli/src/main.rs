//! `silo`: the server daemon and command-line client in one binary.

mod attach;

use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use silo_client::{Control, Events};
use silo_proto::{
    control::{Capture, Created, Method, SpawnSpec},
    model::{SessionSummary, SessionView, TermSize},
    paths,
};

#[derive(Parser)]
#[command(name = "silo", version, about = "Block terminal multiplexer")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Server {
        /// Detach into the background and exit when idle (used by clients
        /// that auto-start the server).
        #[arg(long)]
        daemon: bool,
        /// Exit after this many seconds without sessions or clients
        /// (0 = never; `--daemon` defaults to 10).
        #[arg(long)]
        idle_exit: Option<u64>,
    },
    /// List sessions.
    #[command(alias = "list")]
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Create a session (and attach to it when run in a terminal).
    New {
        /// Session name.
        #[arg(short = 's', long = "session")]
        name: Option<String>,
        /// Do not attach.
        #[arg(short, long)]
        detached: bool,
        /// Working directory (default: current directory).
        #[arg(short = 'c', long)]
        cwd: Option<PathBuf>,
        /// Command to run instead of the login shell.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Attach to a session's focused block (or a block id). Detach with Ctrl-\.
    #[command(alias = "a")]
    Attach {
        target: Option<String>,
        /// Watch without taking size ownership.
        #[arg(long)]
        watch: bool,
    },
    /// Type text into a block.
    Send {
        target: String,
        text: Vec<String>,
        /// Append a newline (Enter).
        #[arg(short, long)]
        enter: bool,
    },
    /// Print a block's text.
    Capture {
        target: Option<String>,
        /// Include scrollback.
        #[arg(long)]
        history: bool,
    },
    /// Close a session and its processes.
    Kill { target: String },
    /// Stop the server and every process it runs.
    KillServer,
    /// Print server events as JSON lines.
    Events,
    /// Raw control API.
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
}

#[derive(Subcommand)]
enum ApiCommand {
    /// List methods.
    List,
    /// Call a method with JSON params.
    Call { method: String, params: Option<String> },
}

fn main() {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Attach { target: None, watch: false });
    if let Command::Server { daemon: true, .. } = command {
        daemonize();
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let code = match runtime.block_on(run(command)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("silo: {error:#}");
            1
        }
    };
    runtime.shutdown_timeout(Duration::from_millis(100));
    std::process::exit(code);
}

/// Detach from the caller: double fork with `setsid`, so the server
/// survives the terminal or app that started it.
fn daemonize() {
    use std::os::fd::AsRawFd;
    let home = paths::home();
    let _ = std::fs::create_dir_all(&home);
    // Clients redirect output to the log; do it ourselves when run by hand.
    let log = (std::io::stdout().is_terminal() || std::io::stderr().is_terminal())
        .then(|| std::fs::OpenOptions::new().create(true).append(true).open(paths::log(&home)).ok())
        .flatten();
    #[allow(unsafe_code)]
    // SAFETY: single-threaded at this point (before the tokio runtime);
    // only fork/setsid/dup2/_exit/waitpid are used.
    unsafe {
        match libc::fork() {
            -1 => {
                eprintln!("silo: fork failed");
                std::process::exit(1);
            }
            0 => {}
            child => {
                let mut status = 0;
                libc::waitpid(child, &raw mut status, 0);
                libc::_exit(0);
            }
        }
        libc::setsid();
        match libc::fork() {
            -1 => libc::_exit(1),
            0 => {}
            _ => libc::_exit(0),
        }
        libc::umask(0o077);
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, 0);
        }
        if let Some(log) = &log {
            libc::dup2(log.as_raw_fd(), 1);
            libc::dup2(log.as_raw_fd(), 2);
        }
        libc::chdir(c"/".as_ptr());
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("SILO_LOG").unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(std::io::stderr().is_terminal()).with_writer(std::io::stderr).try_init();
}

async fn run(command: Command) -> anyhow::Result<i32> {
    match command {
        Command::Server { daemon, idle_exit } => {
            init_tracing();
            let idle_exit = match idle_exit {
                Some(0) => None,
                Some(secs) => Some(Duration::from_secs(secs)),
                None => daemon.then(|| Duration::from_secs(10)),
            };
            if daemon {
                start_menubar();
            }
            silo_server::run(silo_server::Config { home: paths::home(), idle_exit }).await?;
            Ok(0)
        }
        Command::Ls { json } => {
            let (control, _) = connect_existing().await?;
            let Some(control) = control else {
                if json {
                    println!("[]");
                } else {
                    eprintln!("no server running");
                }
                return Ok(0);
            };
            let sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                for s in &sessions {
                    let attached = if s.attached_clients > 0 { format!(" ({} attached)", s.attached_clients) } else { String::new() };
                    println!("{}: {} windows, {} blocks{attached}  [{}]", s.name, s.windows, s.blocks, s.id);
                }
            }
            Ok(0)
        }
        Command::New { name, detached, cwd, command } => {
            let (control, _) = connect().await?;
            let cwd = cwd.or_else(|| std::env::current_dir().ok()).map(|p| p.to_string_lossy().into_owned());
            let size = attach::host_size().map(|(c, r)| TermSize::new(c, r));
            let spawn = SpawnSpec { cwd, command, size };
            let created: Created = control.call(Method::SessionCreate { name, spawn }).await?;
            if detached || !std::io::stdin().is_terminal() {
                println!("{}", created.session_id);
                return Ok(0);
            }
            attach::run(&control, &created.block_id, false).await
        }
        Command::Attach { target, watch } => {
            let (control, _) = connect().await?;
            let block = resolve_block(&control, target.as_deref()).await?;
            if !std::io::stdin().is_terminal() {
                bail!("attach needs a terminal");
            }
            attach::run(&control, &block, watch).await
        }
        Command::Send { target, text, enter } => {
            let (control, _) = connect().await?;
            let block = resolve_block(&control, Some(&target)).await?;
            let mut text = text.join(" ");
            if enter {
                text.push('\r');
            }
            control.call_value(Method::TerminalSend { block_id: block, text }).await?;
            Ok(0)
        }
        Command::Capture { target, history } => {
            let (control, _) = connect().await?;
            let block = resolve_block(&control, target.as_deref()).await?;
            let capture: Capture = control.call(Method::TerminalCapture { block_id: block, history }).await?;
            let mut out = std::io::stdout().lock();
            writeln!(out, "{}", capture.text)?;
            Ok(0)
        }
        Command::Kill { target } => {
            let (control, _) = connect().await?;
            let session = resolve_session(&control, Some(&target)).await?;
            control.call_value(Method::SessionClose { session_id: session.session.id }).await?;
            Ok(0)
        }
        Command::KillServer => {
            let (control, _) = connect_existing().await?;
            let Some(control) = control else {
                eprintln!("no server running");
                return Ok(0);
            };
            let pid = control.hello().server.pid;
            control.call_value(Method::ServerShutdown {}).await?;
            control.closed().await;
            // Wait for the process to release its lock.
            for _ in 0..100 {
                #[allow(unsafe_code)]
                // SAFETY: signal 0 only checks for existence.
                if unsafe { libc::kill(pid as i32, 0) } != 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(0)
        }
        Command::Events => {
            let (control, mut events) = connect().await?;
            control.subscribe().await?;
            let mut out = std::io::stdout();
            while let Some((seq, event)) = events.recv().await {
                let value = serde_json::json!({ "seq": seq, "event": event });
                writeln!(out, "{value}")?;
                out.flush()?;
            }
            Ok(0)
        }
        Command::Api { command } => {
            let (control, _) = connect().await?;
            let value = match command {
                ApiCommand::List => control.call_value(Method::ApiList {}).await?,
                ApiCommand::Call { method, params } => {
                    let params: serde_json::Value = match params {
                        Some(text) => serde_json::from_str(&text).context("params must be JSON")?,
                        None => serde_json::json!({}),
                    };
                    let method: Method = serde_json::from_value(serde_json::json!({ "method": method, "params": params }))
                        .context("unknown method or invalid params")?;
                    control.call_value(method).await?
                }
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(0)
        }
    }
}

fn client_name() -> String {
    format!("silo cli {} (pid {})", env!("CARGO_PKG_VERSION"), std::process::id())
}

/// Launch the bundled menu-bar agent when this binary lives in Silo.app
/// (it exits at once if one is already running for this state dir).
fn start_menubar() {
    if std::env::var_os("SILO_NO_MENUBAR").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.canonicalize().unwrap_or(exe);
    if !exe.to_string_lossy().contains(".app/Contents/MacOS/") {
        return;
    }
    let Some(agent) = exe.parent().map(|d| d.join("silo-menubar")).filter(|p| p.is_file()) else { return };
    let _ = std::process::Command::new(agent)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

async fn connect() -> anyhow::Result<(Control, Events)> {
    let exe = std::env::current_exe().context("locate silo executable")?;
    silo_client::connect_or_spawn(&paths::home(), &exe, &client_name()).await
}

async fn connect_existing() -> anyhow::Result<(Option<Control>, Option<Events>)> {
    match Control::connect(&paths::socket(&paths::home()), &client_name()).await {
        Ok((control, events)) => Ok((Some(control), Some(events))),
        Err(_) => Ok((None, None)),
    }
}

/// A session by id or name; `None` picks the most recently created.
async fn resolve_session(control: &Control, target: Option<&str>) -> anyhow::Result<SessionView> {
    let sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await?;
    let session = match target {
        Some(target) => sessions.iter().find(|s| s.id == target || s.name == target),
        None => sessions.iter().max_by_key(|s| s.created_at),
    };
    let Some(session) = session else {
        match target {
            Some(target) => bail!("no session {target:?}"),
            None => bail!("no sessions; create one with `silo new`"),
        }
    };
    control.call(Method::SessionView { session_id: session.id.clone() }).await}

/// A block id, or the focused block of a session's active window.
async fn resolve_block(control: &Control, target: Option<&str>) -> anyhow::Result<String> {
    if let Some(target) = target {
        let sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await?;
        if !sessions.iter().any(|s| s.id == target || s.name == target) {
            for s in &sessions {
                let view: SessionView = control.call(Method::SessionView { session_id: s.id.clone() }).await?;
                if view.blocks.iter().any(|b| b.id == target) {
                    return Ok(target.to_owned());
                }
            }
        }
    }
    let view = resolve_session(control, target).await?;
    let window = view
        .session
        .windows
        .iter()
        .find(|w| w.id == view.session.active_window)
        .or_else(|| view.session.windows.first())
        .context("session has no windows")?;
    Ok(window.focused.clone())
}
