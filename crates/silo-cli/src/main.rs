//! `silo`: the server daemon and command-line client in one binary.

mod attach;
mod disclaim;
mod generated;
mod permissions;
mod run;

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
    permissions::PermissionId,
};

#[derive(Parser)]
#[command(name = "silo", version = version(), about = "Block terminal multiplexer")]
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
    /// Serve Silo tools over MCP stdio.
    Mcp,
    /// Report program status; hook failures never fail the harness.
    Report {
        state: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        block: Option<String>,
        #[arg(long)]
        codex_notify: bool,
    },
    /// Notify the user from this block.
    Notify {
        #[arg(long)]
        title: Option<String>,
        #[arg(required = true)]
        body: Vec<String>,
    },
    /// Install harness status hooks.
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
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
    /// Update this machine's Silo (the app, this CLI, or both) to the
    /// latest release; a running server is upgraded live.
    #[command(visible_alias = "upgrade")]
    Update {
        /// Only report whether an update is available.
        #[arg(long)]
        check: bool,
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
    /// Create an agent token: `SILO_TOKEN` for a process limited to it.
    Grant {
        /// Limit it to a session (id or name).
        #[arg(long, conflicts_with = "block")]
        session: Option<String>,
        /// Limit it to a block.
        #[arg(long)]
        block: Option<String>,
        /// Watch only.
        #[arg(long, conflicts_with_all = ["interact", "control"])]
        view: bool,
        /// Type and resize too (the default).
        #[arg(long, conflicts_with = "control")]
        interact: bool,
        /// Create, close and rearrange too.
        #[arg(long)]
        control: bool,
        /// How long it stays valid (`90s`, `10m`, `8h`, `1d`; default: until revoked).
        #[arg(long, value_parser = parse_duration)]
        ttl: Option<Duration>,
        /// Shown with the grant, e.g. the agent's name.
        #[arg(long)]
        label: Option<String>,
    },
    /// List grants: paired devices, agent tokens and share links.
    Grants {
        #[command(subcommand)]
        command: Option<GrantsCommand>,
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
        /// Open an otto agent block instead of a program; prints the session id.
        #[arg(long, conflicts_with_all = ["command", "detached", "browser"])]
        agent: bool,
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
    /// Run a command in a block's shell, print its output and exit with
    /// its code.
    Run {
        target: String,
        /// Give up after this long (`30s`, `10m`).
        #[arg(long, value_parser = parse_duration)]
        timeout: Option<Duration>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
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
    Kill {
        target: String,
        /// Close even when programs are running.
        #[arg(long)]
        force: bool,
    },
    /// Stop the server and every process it runs.
    KillServer {
        /// Stop even when programs are running.
        #[arg(long)]
        force: bool,
    },
    /// Print server events as JSON lines.
    Events,
    /// The clipboard of the Silo client showing this block (`SILO_BLOCK`).
    Clipboard {
        #[command(subcommand)]
        command: ClipboardCommand,
    },
    /// macOS privacy permissions of Silo Server (which programs in Silo
    /// terminals get): Screen Recording, Accessibility, Full Disk Access,
    /// Input Monitoring.
    Permissions {
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<PermissionsCommand>,
    },
    /// Raw control API.
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
    /// Call a control method, its params as flags (`silo do <method> --help`).
    #[command(disable_help_flag = true)]
    Do {
        #[arg(allow_hyphen_values = true)]
        method: String,
        /// `--param value`…, required params by position, `--json PARAMS`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The embedded otto harness (`otto-silo`): `silo otto server --daemon`, …
    #[command(hide = true, disable_help_flag = true)]
    Otto {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },
}

#[derive(Subcommand)]
enum PermissionsCommand {
    /// Help grant a permission: macOS asks (Screen Recording), then Silo
    /// shows Silo Server to drag into System Settings' list.
    Request {
        /// screen-recording, accessibility, full-disk-access or input-monitoring.
        #[arg(value_parser = parse_permission)]
        id: PermissionId,
        /// Wait until it is granted.
        #[arg(long)]
        wait: bool,
        /// How long `--wait` waits (`90s`, `10m`).
        #[arg(long, value_parser = parse_duration, default_value = "5m", requires = "wait")]
        timeout: Duration,
    },
    /// Open a permission's page in System Settings.
    Open {
        #[arg(value_parser = parse_permission)]
        id: PermissionId,
    },
    /// Check every permission as this process (the server's child) and
    /// print JSON; what the server runs.
    #[command(hide = true)]
    Probe {
        /// Ask macOS first (Screen Recording shows its dialog).
        #[arg(long, value_parser = parse_permission)]
        request: Option<PermissionId>,
    },
}

fn parse_permission(text: &str) -> Result<PermissionId, String> {
    text.parse()
}

#[derive(Subcommand)]
enum GrantsCommand {
    /// Revoke a grant by id or id prefix; its connections close.
    Revoke { id: String },
}

#[derive(Subcommand)]
enum DevicesCommand {
    /// Revoke a device by id or id prefix.
    Revoke { id: String },
}

#[derive(Subcommand)]
enum ClipboardCommand {
    /// Print the clipboard; exits 1 when no client answers.
    Paste {
        /// `text`, `types` or an image MIME type such as `image/png`.
        #[arg(long = "type", default_value = "text")]
        kind: String,
    },
    /// Set the clipboard from stdin.
    Copy,
    /// Run as a clipboard tool shim (`pbpaste`, `xclip`, ...).
    #[command(hide = true)]
    Shim {
        tool: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Install silo on `target` (an ssh destination), start it and add it
    /// as a host here. `target` downloads the published release of this
    /// silo's version number; for a dev build that release may be another
    /// build than this one (`--local` sends this one).
    Install {
        target: String,
        /// Upload this silo instead (it must run on `target`).
        #[arg(long, conflicts_with = "binary")]
        local: bool,
        /// Upload this silo binary instead.
        #[arg(long)]
        binary: Option<PathBuf>,
    },
    /// Upgrade a paired host (name or id prefix) live, sessions kept: it
    /// downloads the published release of this silo's version number (for a
    /// dev build, maybe another build than this one; `--local` sends this
    /// one). Skips hosts on that build, refuses newer ones.
    #[command(group = clap::ArgGroup::new("target").required(true))]
    Upgrade {
        #[arg(value_name = "HOST", group = "target")]
        name: Option<String>,
        /// Every paired host.
        #[arg(long, group = "target")]
        all: bool,
        /// Go over ssh to this destination instead (silo in ~/.local/bin).
        #[arg(long, value_name = "DESTINATION", group = "target")]
        ssh: Option<String>,
        /// Send this silo instead (it must run on the host).
        #[arg(long, conflicts_with = "binary")]
        local: bool,
        /// Send this silo binary instead.
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Install even when the host runs this build or a newer version.
        #[arg(long)]
        force: bool,
    },
}

/// `silo --version`: version and build, read by `silo remote upgrade`, and
/// ` dev` after them from a dev-channel build.
fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| silo_proto::upgrade::channel_version_text(env!("CARGO_PKG_VERSION"), silo_proto::BUILD))
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
    Rm {
        #[arg(value_name = "HOST")]
        name: String,
    },
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

fn main() -> std::process::ExitCode {
    // First, before any thread: a server (or otto daemon) becomes its own
    // responsible process, so macOS privacy prompts for its shells name
    // Silo Server, not whatever started it (`server --daemon` after its
    // forks: then this is the detached daemon, re-executed).
    let daemonized = disclaim::at_start();
    let cli = Cli::parse();
    // otto runs as itself: its own runtime, logging and exit code.
    if let Some(Command::Otto { args }) = cli.command {
        return otto_cli::main_from(std::iter::once("otto".into()).chain(args).collect());
    }
    // An agent token limits this process to its grant (unix socket only).
    silo_client::set_token(std::env::var("SILO_TOKEN").ok().filter(|t| !t.trim().is_empty()));
    // Closes that would end running programs are refused unless forced
    // (`silo_cli::guarded` asks or names `--force`).
    silo_client::advertise(silo_proto::compat::feature::CLOSE_GUARD);
    let command = cli.command.unwrap_or(Command::Attach { target: None, watch: false });
    let server = matches!(command, Command::Server { .. });
    let status = match (&command, daemonized) {
        (Command::Server { daemon: true, .. }, Some(daemonized)) => daemonized.status,
        (Command::Server { daemon: true, takeover, .. }, None) => daemonize(*takeover),
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
    if server && code == 0 && silo_server::layout_cmds::restart_requested() {
        restart();
    }
    std::process::exit(code);
}

/// `server.restart`: run this binary again as the same server, in this
/// process (see [`restart_args`]).
fn restart() -> ! {
    use std::os::unix::process::CommandExt;
    let args = restart_args(std::env::args_os().skip(1).collect());
    let error = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe).args(args).exec(),
        Err(error) => error,
    };
    eprintln!("silo: restart failed: {error}");
    std::process::exit(1);
}

/// The `silo server` args a restart runs with: the same server, keeping
/// this pid (the one launchd or systemd watches). The old server is gone,
/// so no `--takeover`; this process is already detached, so `--adopt`
/// (which also takes over a server started meanwhile) instead of a
/// `--daemon` that would fork it away, with the idle exit `--daemon` gave.
fn restart_args(args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let daemon = args.iter().any(|a| a == "--daemon");
    let idle_exit = args.iter().any(|a| a == "--idle-exit" || a.to_string_lossy().starts_with("--idle-exit="));
    let mut out: Vec<_> = args.into_iter().filter(|a| a != "--takeover" && a != "--daemon").collect();
    if daemon && !out.iter().any(|a| a == "--adopt") {
        out.push("--adopt".into());
    }
    if daemon && !idle_exit {
        out.extend(["--idle-exit".into(), "10".into()]);
    }
    out
}

/// Detach from the caller: double fork with `setsid`, so the server
/// survives the terminal or app that started it. With `report`, the caller
/// waits for one status byte (0 = success) on a pipe whose write end the
/// daemon gets, and exits with it; EOF without a byte is failure.
fn daemonize(report: bool) -> Option<std::fs::File> {
    let home = paths::home();
    let _ = std::fs::create_dir_all(&home);
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths::log(&home)).ok();
    daemonize_to(report, log)
}

/// [`daemonize`], the daemon's output going to `log`. On macOS the final
/// process then re-executes itself in place as its own responsible process
/// (`disclaim::after_daemonize`): the one that started it is gone by then.
fn daemonize_to(report: bool, log: Option<std::fs::File>) -> Option<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // Never hold the caller's terminal or pipes open.
    let mut pipe = [-1; 2];
    #[allow(unsafe_code)]
    // SAFETY: single-threaded at this point (before the tokio runtime);
    // only pipe/fork/setsid/dup2/read/close/_exit/waitpid are used.
    unsafe {
        // Close-on-exec, so no program the daemon starts holds it open.
        if report && (libc::pipe(pipe.as_mut_ptr()) != 0 || libc::fcntl(pipe[1], libc::F_SETFD, libc::FD_CLOEXEC) != 0) {
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
        drop(log);
        disclaim::after_daemonize(report.then(|| std::fs::File::from_raw_fd(pipe[1])))
    }
}

fn init_tracing() {
    // fff (search) logs every query at info and every folder outside a
    // repo as a warning.
    let filter = tracing_subscriber::EnvFilter::try_from_env("SILO_LOG").unwrap_or_else(|_| "info,fff_search=error".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(std::io::stderr().is_terminal()).with_writer(std::io::stderr).try_init();
}

#[derive(Subcommand)]
enum HooksCommand {
    Install {
        #[arg(value_parser = ["claude", "codex", "opencode"])]
        harness: String,
        #[arg(long)]
        print: bool,
    },
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
                | Command::Update { .. }
                | Command::Down { .. }
                | Command::Status
                | Command::KillServer { .. }
                | Command::Pair { .. }
                | Command::Devices { .. }
                | Command::Grant { .. }
                | Command::Grants { .. }
                | Command::Id
                | Command::Hosts { .. }
                | Command::Clipboard { .. }
                | Command::Permissions { .. }
        )
    {
        bail!("--host is not supported for this command");
    }
    match command {
        Command::Mcp => {
            let (control, events, _net) = connect(host).await?;
            silo_cli::mcp::serve(silo_tools::Tools::new(control, events, silo_tools::Context::default())).await?;
            Ok(0)
        }
        Command::Report { state, kind, app, title, message, id, block, codex_notify } => {
            let Some(block) = block.or_else(|| std::env::var("SILO_BLOCK").ok()).filter(|s| !s.is_empty()) else {
                return Ok(0);
            };
            let report = async {
                let (state, app) = if codex_notify {
                    let payload: serde_json::Value = serde_json::from_str(state.as_deref().context("missing Codex notify payload")?)?;
                    if payload["type"] != "agent-turn-complete" {
                        return Ok::<_, anyhow::Error>(());
                    }
                    ("done".to_owned(), Some("codex".to_owned()))
                } else {
                    (state.context("state is required")?, app)
                };
                let (control, _events, _net) = connect(host).await?;
                let mut params = serde_json::json!({"block_id":block, "state":state});
                for (key, value) in [("kind", kind), ("app", app), ("title", title), ("message", message), ("id", id)] {
                    if let Some(value) = value {
                        params[key] = value.into();
                    }
                }
                control.call_raw("block.report", params).await?;
                Ok(())
            };
            match tokio::time::timeout(Duration::from_secs(5), report).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("silo report: {error:#}"),
                Err(_) => eprintln!("silo report: timed out"),
            }
            Ok(0)
        }
        Command::Notify { title, body } => {
            let block = std::env::var("SILO_BLOCK").context("notify requires SILO_BLOCK")?;
            let (control, _events, _net) = connect(host).await?;
            let mut params = serde_json::json!({"block_id":block,"body":body.join(" ")});
            if let Some(title) = title {
                params["title"] = title.into();
            }
            control.call_raw("block.notify", params).await?;
            Ok(0)
        }
        Command::Otto { .. } => unreachable!("dispatched in main"),
        Command::Hooks { command: HooksCommand::Install { harness, print } } => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            silo_cli::hooks::install(&PathBuf::from(home), &harness, print)?;
            Ok(0)
        }
        Command::Server { daemon, idle_exit, takeover, adopt, relay, no_mdns, port } => {
            init_tracing();
            let home = paths::home();
            let takeover = takeover || (adopt && silo_client::mismatch::server_lock_held(&home));
            tracing::info!(daemon, takeover, adopt, ?idle_exit, pid = std::process::id(), "silo server starting");
            silo_server::upgrade::clean_stale_beside_exe();
            let idle_exit = match idle_exit {
                Some(0) => None,
                Some(secs) => Some(Duration::from_secs(secs)),
                None => daemon.then(|| Duration::from_secs(10)),
            };
            #[cfg(target_os = "macos")]
            if daemon || takeover || adopt {
                start_menubar();
            }
            let mut config = silo_server::Config {
                home,
                idle_exit,
                keep_session: idle_exit.is_none(),
                relay,
                mdns: !no_mdns,
                port,
                system_clipboard: silo_server::sysclip::native(),
                require_approval: true,
                agent_closed: Some(std::sync::Arc::new(hand_back)),
                ..Default::default()
            };
            config.services.insert("otto".into(), otto_service(config.home.clone()));
            // Replace a daemon in this otto home serving another Silo (or an
            // older one) now, not at the first agent.
            let silo_home = config.home.clone();
            tokio::spawn(async move {
                if let Err(error) = otto_claim(&silo_home, &otto_proto::paths::home()).await {
                    tracing::warn!("otto: {error:#}");
                }
            });
            if takeover {
                if let Err(error) = takeover_reporting(config, status).await {
                    if adopt && error.downcast_ref::<silo_proto::OtherChannel>().is_some() {
                        // Exit 0: the service manager must not retry adopting it.
                        tracing::error!("adopt: {error:#}");
                        eprintln!("silo: {error}");
                        return Ok(0);
                    }
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
        Command::New { name, cwd, agent: true, .. } => {
            let (control, _, _net) = connect(host).await?;
            let cwd = cwd.or_else(|| std::env::current_dir().ok()).map(|p| p.to_string_lossy().into_owned());
            let spawn = SpawnSpec { cwd, ..SpawnSpec::default() };
            let created: Created = control.call(Method::SessionCreate { name, spawn, kind: BlockKind::Agent, url: None }).await?;
            println!("{}", created.session_id);
            Ok(0)
        }
        Command::New { name, detached, cwd, command, browser: None, agent: false } => {
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
        Command::Run { target, timeout, command } => {
            let (control, mut events, _net) = connect(host).await?;
            let block = resolve_block(&control, Some(&target)).await?;
            run::run(&control, &mut events, block, &command, timeout).await
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
        Command::Kill { target, force } => {
            let (control, _, _net) = connect(host).await?;
            let session = resolve_session(&control, Some(&target)).await?;
            let session_id = session.session.id;
            silo_cli::guarded(force, |force| control.call_value(Method::SessionClose { session_id: session_id.clone(), force })).await?;
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
        Command::Update { check } => silo_cli::update::run(check).await,
        Command::Remote { command: RemoteCommand::Install { target, local, binary } } => {
            remote_install(&target, &silo_cli::upgrade::Source::new(local, binary)).await
        }
        Command::Remote { command: RemoteCommand::Upgrade { name, all, ssh, local, binary, force } } => {
            let target = match (ssh, all) {
                (Some(destination), _) => UpgradeTarget::Ssh(destination),
                (None, true) => UpgradeTarget::All,
                (None, false) => UpgradeTarget::Host(name.context("name a host, or pass --all or --ssh")?),
            };
            remote_upgrade(target, &silo_cli::upgrade::Source::new(local, binary), force).await
        }
        Command::Down { yes } => down(yes).await,
        Command::Status => show_status().await,
        Command::KillServer { force } => {
            let (control, _) = connect_existing().await?;
            let Some(control) = control else {
                eprintln!("no server running");
                return Ok(0);
            };
            shutdown(control, force).await?;
            Ok(0)
        }
        Command::Clipboard { command } => match command {
            ClipboardCommand::Paste { kind } => silo_cli::clipboard::paste_command(&kind).await,
            ClipboardCommand::Copy => silo_cli::clipboard::copy_command().await,
            ClipboardCommand::Shim { tool, args } => Ok(silo_cli::clipboard::shim(&tool, &args).await),
        },
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
        Command::Permissions { command: Some(PermissionsCommand::Probe { request }), .. } => {
            silo_server::permissions::probe_main(request);
            Ok(0)
        }
        Command::Permissions { command: Some(PermissionsCommand::Open { id }), .. } => permissions::open(id),
        Command::Permissions { command: Some(PermissionsCommand::Request { id, wait, timeout }), .. } => {
            let (control, _events, _net) = connect(None).await?;
            permissions::request(&control, id, wait, timeout).await
        }
        Command::Permissions { json, command: None } => {
            let (control, _events, _net) = connect(None).await?;
            permissions::status(&control, json).await
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
                    // Sent as given: the server knows its methods.
                    let described = control.call::<silo_proto::control::MethodDescription>(Method::ApiDescribe { method: method.clone() }).await;
                    if described.ok().and_then(|d| d.access).is_some_and(|a| a.approval) {
                        silo_cli::confirmed(&control, || control.call_raw(&method, params.clone())).await?
                    } else {
                        control.call_raw(&method, params).await?
                    }
                }
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(0)
        }
        Command::Do { method, .. } if method == "--help" || method == "-h" => {
            println!(
                "Usage: silo do <method> [--param value]… [--json PARAMS]\n\nMethods: silo api list. A method's params: silo do <method> --help."
            );
            Ok(0)
        }
        Command::Do { method, args } => {
            let (control, _, _net) = connect(host).await?;
            let described = control.call::<silo_proto::control::MethodDescription>(Method::ApiDescribe { method: method.clone() }).await;
            let described = match described {
                Err(error) if error.downcast_ref::<silo_proto::control::RpcError>().is_some_and(|e| e.code == "not_found") => {
                    bail!("unknown method {method}; see `silo api list`")
                }
                other => other?,
            };
            if args.iter().any(|a| a == "--help" || a == "-h") {
                print!("{}", generated::usage(&method, &described.description, &described.params_schema));
                return Ok(0);
            }
            let params = generated::build(&described.params_schema, &args)?;
            let approval = described.access.is_some_and(|a| a.approval);
            let call = |params: serde_json::Value| async {
                if approval {
                    silo_cli::confirmed(&control, || control.call_raw(&method, params.clone())).await
                } else {
                    control.call_raw(&method, params).await
                }
            };
            let value = if silo_cli::CLOSES.contains(&method.as_str()) {
                let force = params.get("force").and_then(serde_json::Value::as_bool).unwrap_or(false);
                silo_cli::guarded(force, |force| {
                    let mut params = params.clone();
                    if force {
                        params["force"] = true.into();
                    }
                    call(params)
                })
                .await?
            } else {
                call(params).await?
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
        Command::Grant { session, block, view, interact: _, control: full, ttl, label } => {
            let control = local_server().await?;
            let scope = match (session, block) {
                (Some(session), _) => silo_proto::access::Scope::Session { session_id: session },
                (None, Some(block)) => silo_proto::access::Scope::Block { block_id: block },
                (None, None) => silo_proto::access::Scope::Server,
            };
            let rights = match (view, full) {
                (true, _) => silo_proto::access::Rights::View,
                (_, true) => silo_proto::access::Rights::Control,
                _ => silo_proto::access::Rights::Interact,
            };
            let created = silo_cli::create_token(&control, scope, rights, ttl, label).await?;
            println!("SILO_TOKEN={}", created.secret);
            if std::io::stdout().is_terminal() {
                eprintln!("Shown once. Revoke it with `silo grants revoke {}`.", created.grant.id);
            }
            Ok(0)
        }
        Command::Grants { command } => {
            let control = local_server().await?;
            match command {
                None => {
                    let grants = silo_cli::grants(&control).await?;
                    if grants.is_empty() {
                        eprintln!("no grants");
                    }
                    print!("{}", silo_cli::format_grants(&grants));
                }
                Some(GrantsCommand::Revoke { id }) => {
                    let grant = silo_cli::revoke_grant(&control, &id).await?;
                    println!("revoked {}", grant.id);
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
                HostsCommand::Rm { name } => {
                    let removed = Hosts::update(&home, |hosts| hosts.remove(&name))?;
                    println!("removed {} ({})", removed.name, removed.id);
                }
            }
            Ok(0)
        }
    }
}

fn client_name() -> String {
    agent_client_name(std::env::var("SILO_AGENT").ok().as_deref())
}

/// `silo cli`, with the agent `SILO_AGENT` names: `silo cli · claude-code`.
fn agent_client_name(agent: Option<&str>) -> String {
    let agent = agent.map(|a| silo_proto::text::untrusted(&a.replace('(', ""), 64)).filter(|a| !a.is_empty());
    let name = agent.map_or_else(|| "silo cli".to_owned(), |agent| format!("silo cli · {agent}"));
    format!("{name} {} (pid {})", env!("CARGO_PKG_VERSION"), std::process::id())
}

/// How this machine's server is named when offered to another.
fn hostname() -> String {
    let host = silo_server::hostname();
    if host.is_empty() { "silo host".to_owned() } else { host }
}

/// How this device is named on hosts it pairs with.
fn device_name() -> String {
    let host = silo_server::hostname();
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

/// Stop the server behind `control` and wait for it to exit; `force`:
/// even when programs are running (else [`silo_cli::guarded`] asks).
async fn shutdown(control: Control, force: bool) -> anyhow::Result<()> {
    let pid = control.hello().server.pid;
    let ctl = &control;
    silo_cli::guarded(force, |force| async move { silo_cli::confirmed(ctl, move || ctl.call_value(Method::ServerShutdown { force })).await }).await?;
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

/// What `silo remote upgrade` upgrades.
enum UpgradeTarget {
    Host(String),
    All,
    Ssh(String),
}

async fn remote_upgrade(target: UpgradeTarget, source: &silo_cli::upgrade::Source, force: bool) -> anyhow::Result<i32> {
    let candidate = silo_cli::upgrade::Candidate::new(source)?;
    let home = paths::home();
    let hosts = match target {
        UpgradeTarget::Ssh(destination) => {
            println!("{}", silo_cli::upgrade::over_ssh(&destination, &candidate, force)?);
            return Ok(0);
        }
        UpgradeTarget::All => Hosts::load(&home)?.select(None)?,
        UpgradeTarget::Host(name) => Hosts::load(&home)?.select(Some(&name))?,
    };
    let mut failed = false;
    for host in &hosts {
        match silo_cli::upgrade::paired(&home, host, &candidate, force, &NetOptions::default()).await {
            Ok(line) => println!("{line}"),
            Err(error) if hosts.len() == 1 => return Err(error),
            Err(error) => {
                eprintln!("silo: {error:#}");
                failed = true;
            }
        }
    }
    Ok(i32::from(failed))
}

async fn remote_install(target: &str, source: &silo_cli::upgrade::Source) -> anyhow::Result<i32> {
    use silo_cli::upgrade::Candidate;
    let install = link::Install::new(target)?;
    let candidate = Candidate::new(source)?;
    let (control, mut events, _net) = connect(None).await?;
    link::require_remote(&control).await?;
    let detect = install.detect();
    let output = std::process::Command::new(&detect[0]).args(&detect[1..]).stderr(std::process::Stdio::inherit()).output().context("run ssh")?;
    if !output.status.success() {
        bail!("could not reach {target} over ssh ({})", output.status);
    }
    let platform = link::Platform::from_uname(&String::from_utf8_lossy(&output.stdout))?;
    candidate.check_platform(target, platform.name())?;
    if let Candidate::Release { version } = &candidate {
        silo_cli::upgrade::check_release(&install, platform, version)?;
    }
    control.subscribe().await?;
    let invite = link::invite(&control, link::INSTALL_JOIN_TTL, InvitePurpose::Join).await?;
    match &candidate {
        Candidate::File { path, .. } => {
            eprintln!("silo: uploading {} to {target}", path.display());
            run_interactive(&install.upload(path))?;
            silo_cli::upgrade::check_staged(&install)?;
            run_interactive(&install.join_uploaded(&invite.ticket))?;
        }
        Candidate::Release { version } => {
            eprintln!("silo: installing silo {version} ({}) on {target}", platform.name());
            run_interactive(&install.join_download(version, &invite.ticket))?;
        }
    }
    let offer = link::wait_offer(&mut events, &invite.invite_id, Duration::from_secs(30)).await?;
    // A reinstall keeps the server's key: the host keeps the name given here.
    let host = silo_cli::add_host(&paths::home(), &offer.ticket, None, &NetOptions::default(), &device_name()).await?;
    println!("Added {}.", host.name);
    Ok(0)
}

async fn down(yes: bool) -> anyhow::Result<i32> {
    let runner = service::System;
    let svc = local_service(&runner)?;
    let (server, control) = probe_server().await;
    let sessions = match (&control, server) {
        (Some(control), _) => {
            let sessions = silo_cli::sessions(control).await?;
            let view = match sessions.as_slice() {
                [only] => Some(control.call::<SessionView>(Method::SessionView { session_id: only.id.clone() }).await?),
                _ => None,
            };
            let mut count = silo_cli::sessions_to_end(&sessions, view.as_ref(), std::env::var("SHELL").ok().as_deref());
            // An idle-looking default session may still run a program;
            // then the stop below (forced) must be confirmed too.
            if count == 0 && control.peer_supports(silo_proto::compat::feature::CLOSE_GUARD) {
                let busy: silo_proto::control::BusyBlocks =
                    control.call(Method::BlocksBusy { session_id: None, window_id: None, block_ids: None }).await?;
                if !busy.busy.is_empty() {
                    count = sessions.len();
                }
            }
            Some(count)
        }
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
        // Ending the sessions was confirmed above (or `--yes`).
        (_, Some(control)) => shutdown(control, true).await?,
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
        (service::Server::OtherChannel(channel), None) => {
            println!("running:        yes (a {channel} build; this {} build leaves it alone)", silo_proto::CHANNEL);
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

/// An agent block showing otto `session` closed: if that is a sub-agent the
/// user stopped, its held report goes to the parent (`session.hand_back`).
/// Anything else is refused (`bad_request`) and ignored, as is an otto
/// daemon that isn't running: its next start reports held turns itself.
fn hand_back(session: String) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
    rt.spawn(async move {
        let Ok(otto) = otto_client::Connection::connect(&otto_proto::paths::home()).await else { return };
        if let Err(error) = otto.hand_back(&session).await {
            tracing::debug!(%session, %error, "closed agent block: nothing handed back");
        }
    });
}

/// The `otto` tunnel service: this silo's own otto daemon (`silo otto server
/// --daemon`, told this server's home), started on demand in its own otto
/// home (`otto_proto::paths::home`, one per Silo home), after [`otto_claim`].
fn otto_service(silo_home: PathBuf) -> silo_server::ServiceFn {
    std::sync::Arc::new(move || {
        let silo_home = silo_home.clone();
        Box::pin(async move { otto_socket(&silo_home, &otto_proto::paths::home()).await.map_err(|e| std::io::Error::other(format!("otto: {e:#}"))) })
    })
}

async fn otto_socket(silo_home: &std::path::Path, home: &std::path::Path) -> anyhow::Result<PathBuf> {
    if !otto_claim(silo_home, home).await? {
        let exe = std::env::current_exe().context("locate silo executable")?;
        otto_client::ensure_started(
            home,
            &exe,
            &["otto", "server", "--daemon"],
            &[("SILO_HOME", silo_home.as_os_str()), ("OTTO_SILO_HOME", home.as_os_str())],
        )
        .await?;
        // Another server may have started its own first.
        if std::path::Path::new(&otto_client::Connection::connect(home).await?.hello().silo_home) != silo_home {
            bail!("otto here serves another Silo");
        }
    }
    Ok(otto_proto::paths::socket(home))
}

/// Make the daemon in otto `home`, if any, this server's (at server start and
/// before each `otto` service call): one serving another or an unknown
/// (pre-`silo_home`) Silo home is stopped, and so is an older one serving this
/// home, even when busy, so an upgrade moves otto along with the server.
/// otto's recovery ends the turns it stopped as interrupted (sub-agents report
/// that to their parents); a new prompt continues them. Whether a daemon to
/// keep runs.
async fn otto_claim(silo_home: &std::path::Path, home: &std::path::Path) -> anyhow::Result<bool> {
    use otto_client::{ClientError, Connection};
    let otto = match Connection::connect(home).await {
        Ok(otto) => otto,
        Err(ClientError::Incompatible { .. }) => bail!("an incompatible otto is running; it stops when idle"),
        Err(_) => return Ok(false),
    };
    let (version, serves) = (otto.hello().version.clone(), otto.hello().silo_home.clone());
    let ours = std::path::Path::new(&serves) == silo_home;
    if ours && !older(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(true);
    }
    if ours {
        let sessions = otto
            .session_list(None, false)
            .await
            .map(|s| s.iter().filter(|s| matches!(s.status, otto_proto::SessionStatus::Busy | otto_proto::SessionStatus::WaitingApproval)).count())
            .unwrap_or(0);
        tracing::info!(old = %version, busy = sessions, "restarting an older otto daemon");
    } else {
        let serves = if serves.is_empty() { "an unknown Silo" } else { serves.as_str() };
        tracing::warn!(%version, %serves, home = %home.display(), "replacing an otto daemon that serves another Silo");
    }
    let _ = otto.call(otto_proto::Method::ServerShutdown {}).await;
    drop(otto);
    match otto_released(home).await {
        Err(error) if !ours => {
            tracing::warn!("{error:#}");
            bail!("otto here serves another Silo")
        }
        result => result.map(|()| false),
    }
}

/// `theirs` is an older `major.minor.patch` than `ours` (pre-release suffixes ignored).
fn older(theirs: &str, ours: &str) -> bool {
    let parse = |v: &str| v.split(['-', '+']).next().unwrap_or_default().split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parse(theirs) < parse(ours)
}

/// Wait (up to five seconds) until a stopping otto daemon has let go of its lock.
async fn otto_released(home: &std::path::Path) -> anyhow::Result<()> {
    let lock = otto_proto::paths::server_lock(home);
    for _ in 0..100 {
        let free = std::fs::OpenOptions::new().write(true).open(&lock).map_or(true, |file| file.try_lock().is_ok());
        if free {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("the old otto did not stop")
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
    let Some(agent) = silo_server::bundle::runtime_dir(&exe, &home).map(|d| d.join("silo-menubar")).filter(|p| p.is_file()) else {
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
        Err(error) => match silo_client::mismatch::incompatible_server(&home, &error).await {
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
    fn permissions_commands_parse() {
        let Some(Command::Permissions { json: false, command: None }) = Cli::parse_from(["silo", "permissions"]).command else { panic!() };
        let Some(Command::Permissions { json: true, command: None }) = Cli::parse_from(["silo", "permissions", "--json"]).command else { panic!() };
        let cli = Cli::parse_from(["silo", "permissions", "request", "screen-recording"]);
        let Some(Command::Permissions { command: Some(PermissionsCommand::Request { id, wait, timeout }), .. }) = cli.command else { panic!() };
        assert_eq!((id, wait, timeout), (PermissionId::ScreenRecording, false, Duration::from_secs(300)));
        let cli = Cli::parse_from(["silo", "permissions", "request", "full_disk_access", "--wait", "--timeout", "30s"]);
        let Some(Command::Permissions { command: Some(PermissionsCommand::Request { id, wait, timeout }), .. }) = cli.command else { panic!() };
        assert_eq!((id, wait, timeout), (PermissionId::FullDiskAccess, true, Duration::from_secs(30)));
        let cli = Cli::parse_from(["silo", "permissions", "open", "accessibility"]);
        assert!(matches!(
            cli.command,
            Some(Command::Permissions { command: Some(PermissionsCommand::Open { id: PermissionId::Accessibility }), .. })
        ));
        let cli = Cli::parse_from(["silo", "permissions", "probe", "--request", "screen-recording"]);
        assert!(matches!(
            cli.command,
            Some(Command::Permissions { command: Some(PermissionsCommand::Probe { request: Some(PermissionId::ScreenRecording) }), .. })
        ));
        // The words the server runs its probe child with.
        let words = silo_server::permissions::probe_args(Some(PermissionId::ScreenRecording));
        let cli = Cli::parse_from(std::iter::once("silo").chain(words.iter().map(String::as_str)));
        assert!(matches!(cli.command, Some(Command::Permissions { command: Some(PermissionsCommand::Probe { request: Some(_) }), .. })));
        assert!(Cli::try_parse_from(["silo", "permissions", "request", "camera"]).is_err());
        assert!(Cli::try_parse_from(["silo", "permissions", "request", "accessibility", "--timeout", "1m"]).is_err(), "--timeout needs --wait");
        assert!(Cli::try_parse_from(["silo", "permissions", "open"]).is_err());
        // The probe is the server's child, charged to it: it never disclaims.
        assert!(!disclaim::disclaims(&words));
        let help = <Cli as clap::CommandFactory>::command().find_subcommand("permissions").unwrap().clone().render_long_help().to_string();
        assert!(!help.lines().any(|l| l.trim_start().starts_with("probe")), "{help}");
    }

    #[test]
    fn otto_is_embedded_and_hidden() {
        let Some(Command::Otto { args }) = Cli::parse_from(["silo", "otto", "server", "--daemon"]).command else { panic!() };
        assert_eq!(args, ["server", "--daemon"]);
        let Some(Command::Otto { args }) = Cli::parse_from(["silo", "otto", "--help"]).command else { panic!() };
        assert_eq!(args, ["--help"], "otto prints its own help");
        let help = <Cli as clap::CommandFactory>::command().render_long_help().to_string();
        assert!(!help.lines().any(|l| l.trim_start().starts_with("otto")), "{help}");
    }

    #[test]
    fn older_compares_release_numbers() {
        assert!(older("0.1.6", "0.1.7"));
        assert!(older("0.9.9", "0.10.0"));
        assert!(!older("0.1.7", "0.1.7"));
        assert!(!older("0.2.0", "0.1.7"));
        assert!(!older("0.1.7-dev", "0.1.7"));
    }

    /// A fake otto daemon in a fresh otto home holding its lock: `version`,
    /// serving `silo_home`, one session busy or not. It stops (lets go of the
    /// lock and socket) on `server.shutdown` when `stops`. The home and
    /// whether it was asked to stop.
    async fn fake_otto(
        name: &str,
        version: &str,
        silo_home: &str,
        busy: bool,
        stops: bool,
    ) -> (PathBuf, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        use otto_proto::{Frame, Method, Request, ServerMessage, kind, read_frame, write_frame};
        let home = std::env::temp_dir().join(format!("silo-otto-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let lock = std::fs::File::create(otto_proto::paths::server_lock(&home)).unwrap();
        lock.try_lock().unwrap();
        let listener = tokio::net::UnixListener::bind(otto_proto::paths::socket(&home)).unwrap();
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (version, silo_home, flag, socket) = (version.to_owned(), silo_home.to_owned(), asked.clone(), otto_proto::paths::socket(&home));
        tokio::spawn(async move {
            let mut lock = Some(lock);
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = read_frame(&mut stream).await;
                let hello = otto_proto::ServerHello {
                    protocol: otto_proto::PROTOCOL,
                    version: version.clone(),
                    server_id: "s".into(),
                    pid: 1,
                    started_at: 0,
                    client_id: "c".into(),
                    home: String::new(),
                    silo_home: silo_home.clone(),
                };
                let _ = write_frame(&mut stream, &Frame::json(kind::HELLO, &hello).unwrap()).await;
                while let Ok(Some(frame)) = read_frame(&mut stream).await {
                    let Ok(request) = frame.parse::<Request>() else { continue };
                    let result = match request.method {
                        Method::SessionList { .. } => {
                            let status = if busy { "busy" } else { "idle" };
                            serde_json::json!([{"id":"s","project_id":"p","agent":"a","provider":"p","model":"m","title":"t","cwd":"/","status":status,"created_at":0,"updated_at":0,"last_seq":0,"usage":{"usage":{"input":0,"output":0},"cost_usd":0.0}}])
                        }
                        _ => serde_json::json!({}),
                    };
                    let shutdown = matches!(request.method, Method::ServerShutdown {});
                    let response = ServerMessage::Response { id: request.id, result: Some(result), error: None };
                    let _ = write_frame(&mut stream, &Frame::json(kind::CONTROL, &response).unwrap()).await;
                    if shutdown {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        if stops {
                            drop(lock.take());
                            let _ = std::fs::remove_file(&socket);
                            return;
                        }
                    }
                }
            }
        });
        (home, asked)
    }

    /// Whether `otto_claim` keeps the daemon, whether it asked it to stop,
    /// and its error.
    async fn claim(name: &str, version: &str, serves: &str, busy: bool, stops: bool) -> (Option<bool>, bool, String) {
        let (home, asked) = fake_otto(name, version, serves, busy, stops).await;
        let result = otto_claim(std::path::Path::new("/here/silo"), &home).await;
        std::fs::remove_dir_all(&home).unwrap();
        let error = result.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
        (result.ok(), asked.load(std::sync::atomic::Ordering::SeqCst), error)
    }

    /// This server's otto home holds its own daemon: a current one serving
    /// this Silo is kept; one serving another or an unknown Silo, or an older
    /// one of this Silo, is stopped even when busy (an upgrade moves otto too).
    #[tokio::test]
    async fn otto_claim_replaces_a_daemon_of_another_silo() {
        let current = env!("CARGO_PKG_VERSION");
        assert_eq!(claim("ours", current, "/here/silo", false, true).await, (Some(true), false, String::new()));
        assert_eq!(claim("ours-busy", current, "/here/silo", true, true).await, (Some(true), false, String::new()));
        assert_eq!(claim("other", current, "/elsewhere/silo", true, true).await, (Some(false), true, String::new()));
        assert_eq!(claim("pre", current, "", true, true).await, (Some(false), true, String::new()));
        assert_eq!(claim("old-busy", "0.0.1", "/here/silo", true, true).await, (Some(false), true, String::new()));
        assert_eq!(claim("old-idle", "0.0.1", "/here/silo", false, true).await, (Some(false), true, String::new()));
    }

    /// A daemon of another Silo that does not stop is refused, never reused.
    #[tokio::test]
    async fn otto_claim_refuses_a_daemon_of_another_silo_that_stays() {
        let (kept, asked, error) = claim("stays", env!("CARGO_PKG_VERSION"), "/elsewhere/silo", false, false).await;
        assert_eq!((kept, asked, error.as_str()), (None, true, "otto here serves another Silo"));
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
    fn grant_commands_and_agent_names() {
        let Some(Command::Grant { session, view, control, ttl, label, .. }) =
            Cli::parse_from(["silo", "grant", "--session", "work", "--view", "--ttl", "8h", "--label", "claude-code"]).command
        else {
            panic!()
        };
        assert_eq!(
            (session.as_deref(), view, control, ttl, label.as_deref()),
            (Some("work"), true, false, Some(Duration::from_secs(8 * 3600)), Some("claude-code"))
        );
        assert!(Cli::try_parse_from(["silo", "grant", "--view", "--control"]).is_err());
        assert!(Cli::try_parse_from(["silo", "grant", "--session", "a", "--block", "b"]).is_err());
        assert!(matches!(
            Cli::parse_from(["silo", "grants", "revoke", "ab"]).command,
            Some(Command::Grants { command: Some(GrantsCommand::Revoke { .. }) })
        ));
        let named = agent_client_name(Some("claude-code"));
        assert_eq!(silo_proto::model::short_client_name(&named), "silo cli · claude-code");
        assert_eq!(silo_proto::model::short_client_name(&agent_client_name(None)), "silo cli");
        assert_eq!(silo_proto::model::short_client_name(&agent_client_name(Some(" \u{202e} "))), "silo cli");
    }

    #[test]
    fn a_restart_keeps_its_process_and_server_args() {
        let restart = |args: &str| {
            let out = restart_args(args.split_whitespace().map(Into::into).collect());
            out.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
        };
        assert_eq!(restart("server --adopt"), "server --adopt", "launchd, systemd");
        assert_eq!(restart("server --takeover --daemon --idle-exit 0"), "server --idle-exit 0 --adopt", "systemd after a reload");
        assert_eq!(restart("server --daemon"), "server --adopt --idle-exit 10");
        assert_eq!(restart("server --port 7000"), "server --port 7000");
        for args in ["server --adopt", "server --idle-exit 0 --adopt", "server --adopt --idle-exit 10"] {
            assert!(Cli::try_parse_from(std::iter::once("silo").chain(args.split_whitespace())).is_ok(), "{args}");
        }
    }

    #[test]
    fn do_takes_the_method_s_flags_as_given() {
        let cli = Cli::parse_from(["silo", "do", "window.step", "--session-id", "s1", "--delta", "-1"]);
        let Some(Command::Do { method, args }) = cli.command else { panic!() };
        assert_eq!((method.as_str(), args), ("window.step", vec!["--session-id".to_owned(), "s1".into(), "--delta".into(), "-1".into()]));
        let Some(Command::Do { args, .. }) = Cli::parse_from(["silo", "do", "app.full_screen", "--help"]).command else { panic!() };
        assert_eq!(args, ["--help"]);
        let cli = Cli::parse_from(["silo", "do", "app.full_screen", "--host", "mini"]);
        assert_eq!(cli.host.as_deref(), Some("mini"));
        let Some(Command::Do { args, .. }) = cli.command else { panic!() };
        assert!(args.is_empty(), "--host stays global");
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
        assert!(matches!(Cli::parse_from(["silo", "update"]).command, Some(Command::Update { check: false })));
        assert!(matches!(Cli::parse_from(["silo", "upgrade", "--check"]).command, Some(Command::Update { check: true })));
        assert!(matches!(Cli::parse_from(["silo", "up", "--no-offer"]).command, Some(Command::Up { no_offer: true })));
        assert!(matches!(Cli::parse_from(["silo", "down"]).command, Some(Command::Down { yes: false })));
        assert!(matches!(Cli::parse_from(["silo", "down", "--yes"]).command, Some(Command::Down { yes: true })));
        assert!(matches!(Cli::parse_from(["silo", "status"]).command, Some(Command::Status)));
        assert!(matches!(Cli::parse_from(["silo", "join", "silo1abc"]).command, Some(Command::Join { ticket }) if ticket == "silo1abc"));
        assert!(Cli::try_parse_from(["silo", "join"]).is_err());
        let Some(Command::Remote { command: RemoteCommand::Install { target, binary, local: false } }) =
            Cli::parse_from(["silo", "remote", "install", "me@box", "--binary", "/tmp/silo"]).command
        else {
            panic!()
        };
        assert_eq!((target.as_str(), binary), ("me@box", Some(PathBuf::from("/tmp/silo"))));
        let Some(Command::Remote { command: RemoteCommand::Install { binary: None, local: false, .. } }) =
            Cli::parse_from(["silo", "remote", "install", "box"]).command
        else {
            panic!()
        };
        let Some(Command::Remote { command: RemoteCommand::Install { binary: None, local: true, .. } }) =
            Cli::parse_from(["silo", "remote", "install", "box", "--local"]).command
        else {
            panic!()
        };
        assert!(Cli::try_parse_from(["silo", "remote", "install", "box", "--local", "--binary", "/tmp/silo"]).is_err());
        assert!(Cli::try_parse_from(["silo", "remote", "install", "box", "--download"]).is_err());
        use silo_cli::upgrade::Source;
        assert_eq!(Source::new(false, None), Source::Release);
        assert_eq!(Source::new(true, None), Source::Local);
        assert_eq!(Source::new(false, Some("/tmp/silo".into())), Source::Binary("/tmp/silo".into()));
        let Some(Command::Remote { command: RemoteCommand::Upgrade { name, all: false, ssh: None, local: false, binary: None, force: false } }) =
            Cli::parse_from(["silo", "remote", "upgrade", "mini"]).command
        else {
            panic!()
        };
        assert_eq!(name.as_deref(), Some("mini"));
        assert!(Cli::parse_from(["silo", "remote", "upgrade", "mini"]).host.is_none());
        let Some(Command::Remote { command: RemoteCommand::Upgrade { name: None, all: true, force: true, binary, .. } }) =
            Cli::parse_from(["silo", "remote", "upgrade", "--all", "--force", "--binary", "/tmp/silo"]).command
        else {
            panic!()
        };
        assert_eq!(binary, Some(PathBuf::from("/tmp/silo")));
        let Some(Command::Remote { command: RemoteCommand::Upgrade { name: None, ssh, local: true, .. } }) =
            Cli::parse_from(["silo", "remote", "upgrade", "--ssh", "me@box", "--local"]).command
        else {
            panic!()
        };
        assert_eq!(ssh.as_deref(), Some("me@box"));
        assert!(Cli::try_parse_from(["silo", "remote", "upgrade", "mini", "--local", "--binary", "/tmp/silo"]).is_err());
        assert!(Cli::try_parse_from(["silo", "remote", "upgrade"]).is_err());
        let cli = Cli::parse_from(["silo", "hosts", "rm", "mini"]);
        assert!(cli.host.is_none());
        assert!(matches!(cli.command, Some(Command::Hosts { command: HostsCommand::Rm { name } }) if name == "mini"));
        assert!(Cli::try_parse_from(["silo", "remote", "upgrade", "mini", "--all"]).is_err());
        assert!(Cli::try_parse_from(["silo", "remote", "upgrade", "--all", "--ssh", "me@box"]).is_err());
        let dev = if silo_proto::CHANNEL == silo_proto::Channel::Dev { " dev" } else { "" };
        assert_eq!(version(), format!("{} ({}){dev}", env!("CARGO_PKG_VERSION"), silo_proto::BUILD));
        let Some(Command::Server { adopt: true, takeover: false, .. }) = Cli::parse_from(["silo", "server", "--adopt"]).command else { panic!() };
        assert!(Cli::try_parse_from(["silo", "server", "--adopt", "--takeover"]).is_err());
    }
}
