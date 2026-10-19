//! `silo`: the server daemon and command-line client in one binary.

mod attach;

use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use silo_cli::{NetOptions, hosts::Hosts, parse_duration, parse_relay};
use silo_client::{Control, Events, transport::{Net, RelayConfig}};
use silo_proto::{
    control::{Capture, Created, Method, SpawnSpec},
    model::{BlockKind, SessionSummary, SessionView, TermSize},
    paths,
};

#[derive(Parser)]
#[command(name = "silo", version, about = "Block terminal multiplexer")]
struct Cli {
    /// Use a remote host: a stored host (name or id prefix) or a pairing
    /// ticket.
    #[arg(long, global = true)]
    host: Option<String>,
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
        /// Take every session, live, from the running server (of another
        /// build) instead of starting fresh; fails if that is not possible.
        #[arg(long)]
        takeover: bool,
        /// Relays for remote access: `default`, `disabled` or a relay URL.
        #[arg(long, value_parser = parse_relay, default_value = "default")]
        relay: RelayConfig,
        /// Do not advertise or discover on the LAN via mDNS.
        #[arg(long)]
        no_mdns: bool,
        /// Fixed UDP port for remote access (default: random), so paired
        /// devices can reach the server across restarts without relays or mDNS.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Create a pairing ticket for a new device.
    Pair {
        /// How long the ticket stays valid (`90s`, `10m`, `1h`).
        #[arg(long, value_parser = parse_duration, default_value = "10m")]
        ttl: Duration,
    },
    /// List paired devices.
    Devices {
        #[command(subcommand)]
        command: Option<DevicesCommand>,
    },
    /// Print the server's endpoint id.
    Id,
    /// Remote hosts this CLI can use with `--host`.
    Hosts {
        #[command(subcommand)]
        command: HostsCommand,
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
        /// Run a browser at URL instead of a program; prints the session id.
        #[arg(long, value_name = "URL", conflicts_with_all = ["command", "detached", "cwd"])]
        browser: Option<String>,
        /// Command to run instead of the login shell.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Drive a browser block.
    Browser {
        /// Block id, or a session whose focused block is used.
        #[arg(short, long, global = true)]
        target: Option<String>,
        #[command(subcommand)]
        command: BrowserCmd,
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
enum DevicesCommand {
    /// Revoke a device by id or id prefix.
    Revoke { id: String },
}

#[derive(Subcommand)]
enum HostsCommand {
    /// Pair with a host using its ticket and remember it.
    Add {
        ticket: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// List remembered hosts.
    Ls,
    /// Forget a host (by name or id prefix).
    Rm { host: String },
}

#[derive(Subcommand)]
enum ApiCommand {
    /// List methods.
    List,
    /// Call a method with JSON params.
    Call { method: String, params: Option<String> },
}

#[derive(Subcommand)]
enum BrowserCmd {
    /// Load a URL (bare hosts get https://, localhost and private IPs http://).
    Navigate { url: String },
    Back,
    Forward,
    Reload {
        #[arg(long)]
        ignore_cache: bool,
    },
    /// Set the zoom level (0 = 100%).
    Zoom {
        #[arg(allow_negative_numbers = true)]
        level: f32,
    },
}

impl BrowserCmd {
    fn method(self, block_id: String) -> Method {
        match self {
            Self::Navigate { url } => Method::BrowserNavigate { block_id, url },
            Self::Back => Method::BrowserBack { block_id },
            Self::Forward => Method::BrowserForward { block_id },
            Self::Reload { ignore_cache } => Method::BrowserReload { block_id, ignore_cache },
            Self::Zoom { level } => Method::BrowserZoom { block_id, zoom: level },
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Attach { target: None, watch: false });
    let status = match &command {
        Command::Server { daemon: true, takeover, .. } => daemonize(*takeover),
        _ => None,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let code = match runtime.block_on(run(command, cli.host, status)) {
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
/// survives the terminal or app that started it. With `report`, the caller
/// waits for one status byte (0 = success) on a pipe whose write end the
/// daemon gets, and exits with it; EOF without a byte is failure.
fn daemonize(report: bool) -> Option<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let home = paths::home();
    let _ = std::fs::create_dir_all(&home);
    // Clients redirect output to the log; do it ourselves when run by hand.
    let log = (std::io::stdout().is_terminal() || std::io::stderr().is_terminal())
        .then(|| std::fs::OpenOptions::new().create(true).append(true).open(paths::log(&home)).ok())
        .flatten();
    let mut pipe = [-1; 2];
    #[allow(unsafe_code)]
    // SAFETY: single-threaded at this point (before the tokio runtime);
    // only pipe/fork/setsid/dup2/read/close/_exit/waitpid are used.
    unsafe {
        if report && libc::pipe(pipe.as_mut_ptr()) != 0 {
            eprintln!("silo: pipe failed");
            std::process::exit(1);
        }
        match libc::fork() {
            -1 => {
                eprintln!("silo: fork failed");
                std::process::exit(1);
            }
            0 => {}
            child => {
                let mut status = 0;
                libc::waitpid(child, &raw mut status, 0);
                if !report {
                    libc::_exit(0);
                }
                libc::close(pipe[1]);
                let mut byte = 1u8;
                let n = libc::read(pipe[0], (&raw mut byte).cast(), 1);
                if n == 1 && byte == 0 {
                    libc::_exit(0);
                }
                eprintln!("silo: takeover failed; see the server log");
                libc::_exit(1);
            }
        }
        if report {
            libc::close(pipe[0]);
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
        report.then(|| std::fs::File::from_raw_fd(pipe[1]))
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("SILO_LOG").unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(std::io::stderr().is_terminal()).with_writer(std::io::stderr).try_init();
}

async fn run(command: Command, host: Option<String>, status: Option<std::fs::File>) -> anyhow::Result<i32> {
    let host = host.as_deref();
    if host.is_some() && matches!(command, Command::Server { .. } | Command::KillServer | Command::Pair { .. } | Command::Devices { .. } | Command::Id | Command::Hosts { .. }) {
        bail!("--host is not supported for this command");
    }
    match command {
        Command::Server { daemon, idle_exit, takeover, relay, no_mdns, port } => {
            init_tracing();
            let idle_exit = match idle_exit {
                Some(0) => None,
                Some(secs) => Some(Duration::from_secs(secs)),
                None => daemon.then(|| Duration::from_secs(10)),
            };
            #[cfg(target_os = "macos")]
            if daemon || takeover {
                start_menubar();
            }
            let config = silo_server::Config { home: paths::home(), idle_exit, relay, mdns: !no_mdns, port, ..Default::default() };
            if takeover {
                takeover_reporting(config, status).await?;
            } else {
                silo_server::run(config).await?;
            }
            Ok(0)
        }
        Command::Ls { json } => {
            let (control, _, _net) = match host {
                Some(host) => connect(Some(host)).await?,
                None => match connect_existing().await? {
                    (Some(control), Some(events)) => (control, events, None),
                    _ => {
                        if json {
                            println!("[]");
                        } else {
                            eprintln!("no server running");
                        }
                        return Ok(0);
                    }
                },
            };
            let sessions = silo_cli::sessions(&control).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                print!("{}", silo_cli::format_sessions(&sessions));
            }
            Ok(0)
        }
        Command::New { name, browser: Some(url), .. } => {
            let (control, _, _net) = connect(host).await?;
            let method = Method::SessionCreate { name, spawn: SpawnSpec::default(), kind: BlockKind::Browser, url: Some(url) };
            let created: Created = control.call(method).await?;
            println!("{}", created.session_id);
            Ok(0)
        }
        Command::New { name, detached, cwd, command, browser: None } => {
            let (control, _, _net) = connect(host).await?;
            let cwd = cwd.or_else(|| std::env::current_dir().ok()).map(|p| p.to_string_lossy().into_owned());
            let size = attach::host_size().map(|(c, r)| TermSize::new(c, r));
            let spawn = SpawnSpec { cwd, command, size };
            let created: Created = control.call(Method::SessionCreate { name, spawn, kind: BlockKind::Terminal, url: None }).await?;
            if detached || !std::io::stdin().is_terminal() {
                println!("{}", created.session_id);
                return Ok(0);
            }
            attach::run(&control, &created.block_id, false).await
        }
        Command::Browser { target, command } => {
            let (control, _, _net) = connect(host).await?;
            let block = resolve_block(&control, target.as_deref()).await?;
            control.call_value(command.method(block)).await?;
            Ok(0)
        }
        Command::Attach { target, watch } => {
            let (control, _, _net) = connect(host).await?;
            let block = resolve_block(&control, target.as_deref()).await?;
            if !std::io::stdin().is_terminal() {
                bail!("attach needs a terminal");
            }
            attach::run(&control, &block, watch).await
        }
        Command::Send { target, text, enter } => {
            let (control, _, _net) = connect(host).await?;
            let block = resolve_block(&control, Some(&target)).await?;
            let mut text = text.join(" ");
            if enter {
                text.push('\r');
            }
            control.call_value(Method::TerminalSend { block_id: block, text }).await?;
            Ok(0)
        }
        Command::Capture { target, history } => {
            let (control, _, _net) = connect(host).await?;
            let block = resolve_block(&control, target.as_deref()).await?;
            let capture: Capture = control.call(Method::TerminalCapture { block_id: block, history }).await?;
            let mut out = std::io::stdout().lock();
            writeln!(out, "{}", capture.text)?;
            Ok(0)
        }
        Command::Kill { target } => {
            let (control, _, _net) = connect(host).await?;
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
            let (control, mut events, _net) = connect(host).await?;
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
            let (control, _, _net) = connect(host).await?;
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
        Command::Pair { ttl } => {
            let control = local_server().await?;
            warn_remote_error(&control).await?;
            let invite = silo_cli::create_invite(&control, ttl).await?;
            if !std::io::stdout().is_terminal() {
                println!("{}", invite.ticket);
                return Ok(0);
            }
            println!("{}", silo_cli::qr(&invite.ticket)?);
            println!("{}\n", invite.ticket);
            println!("Expires in {}. On the other device: silo hosts add <ticket>", human(invite.expires_in_secs));
            Ok(0)
        }
        Command::Devices { command } => {
            let control = local_server().await?;
            match command {
                None => {
                    let devices = silo_cli::devices(&control).await?;
                    if devices.is_empty() {
                        eprintln!("no paired devices; pair one with `silo pair`");
                    }
                    for d in devices {
                        println!("{}  {}  paired {}, last seen {}", d.id, d.name, silo_cli::ago(d.paired_at), silo_cli::ago(d.last_seen));
                    }
                }
                Some(DevicesCommand::Revoke { id }) => {
                    let device = silo_cli::revoke(&control, &id).await?;
                    println!("revoked {} ({})", device.name, device.id);
                }
            }
            Ok(0)
        }
        Command::Id => {
            let control = local_server().await?;
            println!("{}", control.hello().server.endpoint_id);
            warn_remote_error(&control).await?;
            Ok(0)
        }
        Command::Hosts { command } => {
            let home = paths::home();
            match command {
                HostsCommand::Add { ticket, name } => {
                    let host = silo_cli::add_host(&home, &ticket, name, &NetOptions::default(), &device_name()).await?;
                    println!("paired with {} ({})", host.name, host.id);
                }
                HostsCommand::Ls => {
                    for h in Hosts::load(&home)?.hosts {
                        println!("{}  {}", h.name, h.id);
                    }
                }
                HostsCommand::Rm { host } => {
                    let mut hosts = Hosts::load(&home)?;
                    let removed = hosts.remove(&host)?;
                    hosts.save(&home)?;
                    println!("removed {} ({})", removed.name, removed.id);
                }
            }
            Ok(0)
        }
    }
}

fn client_name() -> String {
    format!("silo cli {} (pid {})", env!("CARGO_PKG_VERSION"), std::process::id())
}

/// How this device is named on hosts it pairs with.
fn device_name() -> String {
    let mut buf = [0u8; 256];
    #[allow(unsafe_code)]
    // SAFETY: the buffer is valid for its length.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let host = if ok { String::from_utf8_lossy(&buf[..len]).into_owned() } else { String::new() };
    if host.is_empty() { "silo cli".to_owned() } else { format!("silo cli on {host}") }
}

fn human(secs: u64) -> String {
    match secs {
        0..120 => format!("{secs}s"),
        120..7200 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// Say why the server cannot be reached remotely, if it cannot.
async fn warn_remote_error(control: &Control) -> anyhow::Result<()> {
    if let Some(error) = silo_cli::server_info(control).await?.remote_error {
        eprintln!("silo: remote access is unavailable on this server: {error}");
    }
    Ok(())
}

/// The running local server; pairing needs one that stays up.
async fn local_server() -> anyhow::Result<Control> {
    match connect_existing().await? {
        (Some(control), _) => Ok(control),
        _ => bail!("no server running; start one with `silo server`"),
    }
}

/// Run a takeover, reporting its outcome on `status` (see [`daemonize`]):
/// success once `runtime.json` names this process (written after COMMIT),
/// failure when the takeover returns an error first.
async fn takeover_reporting(config: silo_server::Config, status: Option<std::fs::File>) -> anyhow::Result<()> {
    let report = |status: Option<std::fs::File>, ok: bool| {
        if let Some(mut status) = status {
            let _ = status.write_all(&[u8::from(!ok)]);
        }
    };
    let runtime = paths::runtime(&config.home);
    let committed = async {
        loop {
            let ours = std::fs::read(&runtime).ok().and_then(|b| serde_json::from_slice::<paths::Runtime>(&b).ok()).is_some_and(|r| r.pid == std::process::id());
            if ours {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let takeover = silo_server::takeover(config, false);
    tokio::pin!(takeover);
    tokio::select! {
        result = &mut takeover => {
            report(status, result.is_ok());
            result
        }
        () = committed => {
            report(status, true);
            takeover.await
        }
    }
}

/// Launch the bundled menu-bar agent when this binary lives in Silo.app
/// (it exits at once if one is already running for this state dir).
#[cfg(target_os = "macos")]
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

/// The local server (spawned if needed) or, with `host`, a remote one
/// (never spawned). Keep the `Net` alive as long as the control.
async fn connect(host: Option<&str>) -> anyhow::Result<(Control, Events, Option<Net>)> {
    if let Some(host) = host {
        let remote = silo_cli::connect_remote(&paths::home(), host, &NetOptions::default(), &device_name()).await?;
        return Ok((remote.control, remote.events, Some(remote.net)));
    }
    let exe = std::env::current_exe().context("locate silo executable")?;
    let (control, events) = silo_client::connect_or_spawn(&paths::home(), &exe, &client_name()).await?;
    Ok((control, events, None))
}

async fn connect_existing() -> anyhow::Result<(Option<Control>, Option<Events>)> {
    match Control::connect(&paths::socket(&paths::home()), &client_name()).await {
        Ok((control, events)) => Ok((Some(control), Some(events))),
        Err(error) if silo_client::mismatch::is_mismatch(&error) => {
            Err(silo_client::mismatch::inspect(&paths::home()).await.into())
        }
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
    control.call(Method::SessionView { session_id: session.id.clone() }).await
}

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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn server_flags() {
        let Some(Command::Server { relay, no_mdns, .. }) = Cli::parse_from(["silo", "server"]).command else { panic!() };
        assert_eq!((relay, no_mdns), (RelayConfig::Default, false));
        let cli = Cli::parse_from(["silo", "server", "--relay", "disabled", "--no-mdns"]);
        let Some(Command::Server { relay, no_mdns, .. }) = cli.command else { panic!() };
        assert_eq!((relay, no_mdns), (RelayConfig::Disabled, true));
        let cli = Cli::parse_from(["silo", "server", "--relay", "https://relay.example.com"]);
        assert!(matches!(cli.command, Some(Command::Server { relay: RelayConfig::Custom(_), .. })));
        assert!(Cli::try_parse_from(["silo", "server", "--relay", "bogus"]).is_err());
    }

    #[test]
    fn pair_ttl_and_host() {
        let Some(Command::Pair { ttl }) = Cli::parse_from(["silo", "pair"]).command else { panic!() };
        assert_eq!(ttl, Duration::from_secs(600));
        let Some(Command::Pair { ttl }) = Cli::parse_from(["silo", "pair", "--ttl", "90s"]).command else { panic!() };
        assert_eq!(ttl, Duration::from_secs(90));
        assert!(Cli::try_parse_from(["silo", "pair", "--ttl", "soon"]).is_err());
        let cli = Cli::parse_from(["silo", "ls", "--host", "work"]);
        assert_eq!(cli.host.as_deref(), Some("work"));
        assert!(matches!(Cli::parse_from(["silo", "devices", "revoke", "ab12"]).command, Some(Command::Devices { command: Some(DevicesCommand::Revoke { .. }) })));
    }

    #[test]
    fn browser_commands() {
        let cli = Cli::parse_from(["silo", "new", "--browser", "localhost:3000", "-s", "work"]);
        let Some(Command::New { name, browser, .. }) = cli.command else { panic!() };
        assert_eq!((name.as_deref(), browser.as_deref()), (Some("work"), Some("localhost:3000")));
        assert!(Cli::try_parse_from(["silo", "new", "--browser", "x", "--", "vim"]).is_err());
        let cli = Cli::parse_from(["silo", "browser", "zoom", "-1.5", "-t", "b1"]);
        let Some(Command::Browser { target, command }) = cli.command else { panic!() };
        assert_eq!(target.as_deref(), Some("b1"));
        assert!(matches!(command.method("b1".into()), Method::BrowserZoom { zoom, .. } if zoom == -1.5));
        let Some(Command::Browser { command, .. }) = Cli::parse_from(["silo", "browser", "reload", "--ignore-cache"]).command else { panic!() };
        assert!(matches!(command.method("b".into()), Method::BrowserReload { ignore_cache: true, .. }));
        let Some(Command::Browser { command, .. }) = Cli::parse_from(["silo", "browser", "navigate", "example.com"]).command else { panic!() };
        assert!(matches!(command.method("b".into()), Method::BrowserNavigate { url, .. } if url == "example.com"));
    }
}
