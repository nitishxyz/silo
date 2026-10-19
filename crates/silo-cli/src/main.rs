//! `silo`: the server daemon and command-line client in one binary.

mod attach;

use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use silo_cli::{NetOptions, hosts::Hosts, link, parse_duration, parse_relay};
use silo_client::service;
use silo_client::{
    Control, Events,
    transport::{Net, RelayConfig},
};
use silo_proto::{
    control::{Capture, Created, InvitePurpose, Method, SpawnSpec},
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
        /// Take over the running server (of any build) if there is one,
        /// else start fresh; how the service manager runs the server.
        #[arg(long, conflicts_with = "takeover")]
        adopt: bool,
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
    /// Run the server as a user service that starts at login (systemd or
    /// launchd), upgrading a running server live.
    Up {
        /// Do not offer this machine to the Silo terminal running over SSH.
        #[arg(long)]
        no_offer: bool,
    },
    /// Start the server (as `up`) and offer it to the machine whose join
    /// ticket this is; it is added there.
    Join { ticket: String },
    /// Set up other machines over SSH.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Stop the server service and remove it; ends every session.
    Down {
        /// Do not ask before ending sessions.
        #[arg(long)]
        yes: bool,
    },
    /// Show the server and its service.
    Status,
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
    /// Float a block (or a session's focused block) over its window.
    Float { target: String },
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
enum RemoteCommand {
    /// Install silo on `target` (an ssh destination), start it and add it
    /// as a host here.
    Install {
        target: String,
        /// Upload this silo binary instead of downloading the release.
        #[arg(long)]
        binary: Option<PathBuf>,
    },
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
    /// Describe a method: params and result JSON Schema and an example.
    Describe { method: String },
    /// Call a method with JSON params.
    Call { method: String, params: Option<String> },
}

#[derive(Subcommand)]
enum BrowserCmd {
    /// Load a URL (bare hosts get https://, localhost and private IPs http://).
    Navigate {
        url: String,
    },
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
    // Never hold the caller's terminal or pipes open.
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths::log(&home)).ok();
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
    if host.is_some()
        && matches!(
            command,
            Command::Server { .. }
                | Command::Up { .. }
                | Command::Join { .. }
                | Command::Remote { .. }
                | Command::Down { .. }
                | Command::Status
                | Command::KillServer
                | Command::Pair { .. }
                | Command::Devices { .. }
                | Command::Id
                | Command::Hosts { .. }
        )
    {
        bail!("--host is not supported for this command");
    }
    match command {
        Command::Server { daemon, idle_exit, takeover, adopt, relay, no_mdns, port } => {
            init_tracing();
            let home = paths::home();
            let takeover = takeover || (adopt && silo_client::mismatch::server_lock_held(&home));
            tracing::info!(daemon, takeover, adopt, ?idle_exit, pid = std::process::id(), "silo server starting");
            let idle_exit = match idle_exit {
                Some(0) => None,
                Some(secs) => Some(Duration::from_secs(secs)),
                None => daemon.then(|| Duration::from_secs(10)),
            };
            #[cfg(target_os = "macos")]
            if daemon || takeover || adopt {
                start_menubar();
            }
            let config = silo_server::Config { home, idle_exit, relay, mdns: !no_mdns, port, ..Default::default() };
            if takeover {
                if let Err(error) = takeover_reporting(config, status).await {
                    if error.downcast_ref::<silo_server::handoff::NotUpgradable>().is_some() {
                        // A clean exit so the service manager does not retry
                        // (the unit and plist do not restart exit 0).
                        if adopt {
                            tracing::error!("adopt: {error:#}; stop it with `silo down`, then run `silo up`");
                            return Ok(0);
                        }
                        tracing::warn!("takeover: {error:#}");
                        return Ok(silo_proto::EXIT_NOT_UPGRADABLE);
                    }
                    return Err(error);
                }
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
        Command::Float { target } => {
            let (control, _, _net) = connect(host).await?;
            let block = resolve_block(&control, Some(&target)).await?;
            control.call_value(Method::BlockFloat { block_id: block, rect: None }).await?;
            Ok(0)
        }
        Command::Kill { target } => {
            let (control, _, _net) = connect(host).await?;
            let session = resolve_session(&control, Some(&target)).await?;
            control.call_value(Method::SessionClose { session_id: session.session.id }).await?;
            Ok(0)
        }
        Command::Up { no_offer } => {
            let control = up().await?;
            let tty = std::io::stdout().is_terminal();
            let lc_terminal = std::env::var("LC_TERMINAL").ok();
            let ssh = std::env::var("SSH_CONNECTION").ok();
            if link::ssh_offer_wanted(lc_terminal.as_deref(), ssh.as_deref(), tty, no_offer) {
                offer_over_ssh(&control).await?;
            }
            Ok(0)
        }
        Command::Join { ticket } => join(&ticket).await,
        Command::Remote { command: RemoteCommand::Install { target, binary } } => remote_install(&target, binary.as_deref()).await,
        Command::Down { yes } => down(yes).await,
        Command::Status => show_status().await,
        Command::KillServer => {
            let (control, _) = connect_existing().await?;
            let Some(control) = control else {
                eprintln!("no server running");
                return Ok(0);
            };
            shutdown(control).await?;
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
                ApiCommand::Describe { method } => control.call_value(Method::ApiDescribe { method }).await?,
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
                    let removed = Hosts::update(&home, |hosts| hosts.remove(&host))?;
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

/// This machine's hostname; empty if it has none.
fn raw_hostname() -> String {
    let mut buf = [0u8; 256];
    #[allow(unsafe_code)]
    // SAFETY: the buffer is valid for its length.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if ok { String::from_utf8_lossy(&buf[..len]).into_owned() } else { String::new() }
}

/// How this machine's server is named when offered to another.
fn hostname() -> String {
    let host = raw_hostname();
    if host.is_empty() { "silo host".to_owned() } else { host }
}

/// How this device is named on hosts it pairs with.
fn device_name() -> String {
    let host = raw_hostname();
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

/// Stop the server behind `control` and wait for it to exit.
async fn shutdown(control: Control) -> anyhow::Result<()> {
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
    Ok(())
}

fn local_service(runner: &dyn service::Runner) -> anyhow::Result<service::Service> {
    let exe = std::env::current_exe().and_then(|e| e.canonicalize()).context("locate silo executable")?;
    service::Service::local(service::detect(runner), exe, paths::home())
}

/// The local server, if one of this build answers, and how it runs.
async fn probe_server() -> (service::Server, Option<Control>) {
    service::probe_server(&paths::home(), &client_name()).await
}

fn manager_name(manager: service::Manager) -> &'static str {
    match manager {
        service::Manager::Systemd => "systemd",
        service::Manager::Launchd => "launchd",
        service::Manager::None => "none",
    }
}

/// Install or update the server service and start it; returns once this
/// build's server answers.
async fn up() -> anyhow::Result<Control> {
    let runner = service::System;
    let svc = local_service(&runner)?;
    let service::Up { control, actions, notes } = service::up(&svc, &runner, &client_name()).await.map_err(|error| {
        if error.is::<service::NotUpgraded>() {
            anyhow::anyhow!("{error}; stop it with `silo down` (ends its sessions), then run `silo up`")
        } else {
            error
        }
    })?;
    for note in notes {
        eprintln!("silo: {note}");
    }
    let pid = control.hello().server.pid;
    match svc.manager {
        service::Manager::None => {
            println!("silo server running (pid {pid})");
            eprintln!("silo: no service manager; the server will not survive a reboot");
        }
        manager if actions.is_empty() => println!("silo server already up (pid {pid}, {})", manager_name(manager)),
        manager => println!("silo server up (pid {pid}, {})", manager_name(manager)),
    }
    Ok(control)
}

/// Offer this machine to the Silo app whose terminal we run in over SSH.
async fn offer_over_ssh(control: &Control) -> anyhow::Result<()> {
    if silo_cli::server_info(control).await?.remote_error.is_some() {
        return Ok(());
    }
    if std::env::var_os("TMUX").is_some() {
        eprintln!("silo: inside tmux; run `silo up` outside it to offer this machine to your Silo");
        return Ok(());
    }
    let invite = link::invite(control, link::SSH_OFFER_TTL, InvitePurpose::Device).await?;
    let mut out = std::io::stdout().lock();
    out.write_all(silo_proto::offer::osc(&invite.ticket, &hostname()).as_bytes())?;
    out.flush()?;
    println!("Offered this machine to your Silo.");
    Ok(())
}

async fn join(ticket: &str) -> anyhow::Result<i32> {
    silo_client::transport::PairTicket::decode(ticket.trim()).context("invalid join ticket")?;
    let control = up().await?;
    let name = hostname();
    link::join(&paths::home(), &control, ticket, &name, &NetOptions::default()).await?;
    println!("Offered {name} to the other machine.");
    Ok(0)
}

/// Run `argv`, sharing our terminal.
fn run_interactive(argv: &[String]) -> anyhow::Result<()> {
    let status = std::process::Command::new(&argv[0]).args(&argv[1..]).status().with_context(|| format!("run {}", argv[0]))?;
    if !status.success() {
        bail!("{} failed ({status})", argv[0]);
    }
    Ok(())
}

async fn remote_install(target: &str, binary: Option<&std::path::Path>) -> anyhow::Result<i32> {
    let install = link::Install::new(target)?;
    let (control, mut events, _net) = connect(None).await?;
    link::require_remote(&control).await?;
    let detect = install.detect();
    let output = std::process::Command::new(&detect[0]).args(&detect[1..]).stderr(std::process::Stdio::inherit()).output().context("run ssh")?;
    if !output.status.success() {
        bail!("could not reach {target} over ssh ({})", output.status);
    }
    let platform = link::Platform::from_uname(&String::from_utf8_lossy(&output.stdout))?;
    if let Some(binary) = binary {
        let mut header = [0u8; 64];
        let mut file = std::fs::File::open(binary).with_context(|| format!("open {}", binary.display()))?;
        let n = std::io::Read::read(&mut file, &mut header)?;
        if link::Platform::of_binary(&header[..n]) != Some(platform) {
            bail!("{} is not a binary for {target} (needs {})", binary.display(), platform.asset());
        }
    }
    control.subscribe().await?;
    let invite = link::invite(&control, link::INSTALL_JOIN_TTL, InvitePurpose::Join).await?;
    match binary {
        Some(binary) => {
            eprintln!("silo: uploading {} to {target}", binary.display());
            run_interactive(&install.upload(binary))?;
            run_interactive(&install.join_uploaded(&invite.ticket))?;
        }
        None => {
            eprintln!("silo: installing {} on {target}", platform.asset());
            run_interactive(&install.join_download(&invite.ticket))?;
        }
    }
    let offer = link::wait_offer(&mut events, &invite.invite_id, Duration::from_secs(30)).await?;
    let host = silo_cli::add_host(&paths::home(), &offer.ticket, Some(offer.name), &NetOptions::default(), &device_name()).await?;
    println!("Added {}.", host.name);
    Ok(0)
}

async fn down(yes: bool) -> anyhow::Result<i32> {
    let runner = service::System;
    let svc = local_service(&runner)?;
    let (server, control) = probe_server().await;
    let sessions = match (&control, server) {
        (Some(control), _) => Some(silo_cli::sessions(control).await?.len()),
        (None, service::Server::Other) => silo_client::mismatch::inspect(&paths::home()).await.sessions,
        (None, service::Server::Unresponsive) => None,
        (None, _) => Some(0),
    };
    if sessions != Some(0) && !yes {
        let prompt = match sessions {
            Some(1) => "Stopping the server ends 1 session.".to_owned(),
            Some(n) => format!("Stopping the server ends {n} sessions."),
            None => "Stopping the server ends every session it runs.".to_owned(),
        };
        if !std::io::stdin().is_terminal() {
            bail!("{prompt} Pass --yes to confirm.");
        }
        eprint!("{prompt} Continue? [y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Ok(1);
        }
    }
    drop(control);
    let state = svc.probe(&runner, server)?;
    svc.apply(&runner, &service::plan_down(&state))?;
    match probe_server().await {
        (_, Some(control)) => shutdown(control).await?,
        (service::Server::Other | service::Server::Unresponsive, None) => {
            let home = paths::home();
            silo_client::mismatch::stop(&home, &silo_client::mismatch::inspect(&home).await).await?;
        }
        _ => {}
    }
    println!("silo server down");
    Ok(0)
}

async fn show_status() -> anyhow::Result<i32> {
    let runner = service::System;
    let svc = local_service(&runner)?;
    let (server, control) = probe_server().await;
    let state = svc.probe(&runner, server)?;
    let managed = if state.running { manager_name(svc.manager) } else { "not managed" };
    let autostart = if state.loaded { "yes" } else { "no" };
    match (server, control) {
        (_, Some(control)) => {
            let hello = control.hello();
            let sessions = silo_cli::sessions(&control).await?.len();
            let devices = silo_cli::devices(&control).await?.len();
            println!("running:        yes");
            println!("pid:            {}", hello.server.pid);
            println!("build:          {}", hello.build);
            println!("endpoint id:    {}", hello.server.endpoint_id);
            println!("managed by:     {managed}");
            println!("sessions:       {sessions}");
            println!("paired devices: {devices}");
        }
        (service::Server::Other, None) => {
            println!("running:        yes (a different build; `silo up` upgrades it)");
            println!("managed by:     {managed}");
        }
        (service::Server::Unresponsive, None) => {
            println!("running:        yes (not responding)");
            println!("managed by:     {managed}");
        }
        _ => println!("running:        no"),
    }
    println!("autostart:      {autostart}");
    if state.loaded && state.installed != state.desired {
        println!("note:           the service is out of date; `silo up` updates it");
    }
    Ok(0)
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
            let ours = std::fs::read(&runtime)
                .ok()
                .and_then(|b| serde_json::from_slice::<paths::Runtime>(&b).ok())
                .is_some_and(|r| r.pid == std::process::id());
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

/// Launch the menu-bar agent next to this binary when it runs from its
/// runtime directory (it exits at once if one already runs for this state dir).
#[cfg(target_os = "macos")]
fn start_menubar() {
    if std::env::var_os("SILO_NO_MENUBAR").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe().and_then(|e| e.canonicalize()) else {
        return;
    };
    let home = paths::home();
    let home = home.canonicalize().unwrap_or(home);
    let Some(agent) = silo_server::bundle::runtime_dir(&exe, &home, silo_proto::BUILD).map(|d| d.join("silo-menubar")).filter(|p| p.is_file()) else {
        return;
    };
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
    let home = paths::home();
    match Control::connect(&paths::socket(&home), &client_name()).await {
        Ok((control, events)) => Ok((Some(control), Some(events))),
        Err(error) => match silo_client::mismatch::different_build(&home, &error).await {
            Some(server) => Err(server.into()),
            None => Ok((None, None)),
        },
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
        assert!(matches!(
            Cli::parse_from(["silo", "devices", "revoke", "ab12"]).command,
            Some(Command::Devices { command: Some(DevicesCommand::Revoke { .. }) })
        ));
    }

    #[test]
    fn float_and_describe_commands() {
        assert!(matches!(Cli::parse_from(["silo", "float", "b1"]).command, Some(Command::Float { target }) if target == "b1"));
        let cli = Cli::parse_from(["silo", "api", "describe", "ping"]);
        assert!(matches!(cli.command, Some(Command::Api { command: ApiCommand::Describe { method } }) if method == "ping"));
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

    #[test]
    fn service_and_link_commands() {
        assert!(matches!(Cli::parse_from(["silo", "up"]).command, Some(Command::Up { no_offer: false })));
        assert!(matches!(Cli::parse_from(["silo", "up", "--no-offer"]).command, Some(Command::Up { no_offer: true })));
        assert!(matches!(Cli::parse_from(["silo", "down"]).command, Some(Command::Down { yes: false })));
        assert!(matches!(Cli::parse_from(["silo", "down", "--yes"]).command, Some(Command::Down { yes: true })));
        assert!(matches!(Cli::parse_from(["silo", "status"]).command, Some(Command::Status)));
        assert!(matches!(Cli::parse_from(["silo", "join", "silo1abc"]).command, Some(Command::Join { ticket }) if ticket == "silo1abc"));
        assert!(Cli::try_parse_from(["silo", "join"]).is_err());
        let Some(Command::Remote { command: RemoteCommand::Install { target, binary } }) =
            Cli::parse_from(["silo", "remote", "install", "me@box", "--binary", "/tmp/silo"]).command
        else {
            panic!()
        };
        assert_eq!((target.as_str(), binary), ("me@box", Some(PathBuf::from("/tmp/silo"))));
        let Some(Command::Remote { command: RemoteCommand::Install { binary: None, .. } }) =
            Cli::parse_from(["silo", "remote", "install", "box"]).command
        else {
            panic!()
        };
        let Some(Command::Server { adopt: true, takeover: false, .. }) = Cli::parse_from(["silo", "server", "--adopt"]).command else { panic!() };
        assert!(Cli::try_parse_from(["silo", "server", "--adopt", "--takeover"]).is_err());
    }
}
