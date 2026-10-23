//! The `otto-silo` command line, also embedded in `silo` as the hidden `silo otto`.

use std::{
    ffi::OsString,
    io::{self, IsTerminal, Read, Write},
    process::ExitCode,
    sync::OnceLock,
};

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use otto_llm::{ErrorClass, LlmError, providers::Providers};
use tokio_util::sync::CancellationToken;

mod ask;
mod auth;
mod daemon;
mod script;
mod services;
mod sessions;
mod silo_status;

#[derive(Parser)]
#[command(name = "otto-silo", version, about = "Otto agent harness: daemon, sessions, models and sign-in")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Host the local runtime or manage its daemon.
    Server(daemon::ServerArgs),
    /// Run a durable agent turn through the daemon.
    Run(sessions::RunArgs),
    /// List project sessions, or every project with --all.
    Sessions {
        #[arg(long)]
        all: bool,
    },
    /// Watch a session; read-only unless --interactive.
    Attach(sessions::AttachArgs),
    /// Rename, archive, fork or change the approval mode of a session.
    Session {
        #[command(subcommand)]
        command: sessions::SessionCommand,
    },
    /// Cancel a session's running turn.
    Cancel { id: String },
    /// Hand a stopped sub-agent back: its held report goes to the parent now.
    HandBack { id: String },
    /// Sign in, sign out, or inspect credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// List available model capabilities and pricing.
    Models {
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(otto_llm::providers::IDS))]
        provider: Option<String>,
        /// Include models for providers and auth modes not currently signed in.
        #[arg(long)]
        all: bool,
        #[arg(long, value_parser = ["api-key", "subscription"])]
        mode: Option<String>,
        /// Fetch current metadata before listing.
        #[arg(long)]
        refresh: bool,
    },
    #[command(hide = true)]
    CatalogRefresh,
    /// Send a prompt directly to a model.
    Ask(AskArgs),
}

#[derive(Subcommand)]
enum AuthCommand {
    Login(LoginArgs),
    Logout {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(otto_llm::providers::IDS))]
        provider: String,
    },
    Status,
}

#[derive(Args)]
struct LoginArgs {
    #[arg(value_parser = clap::builder::PossibleValuesParser::new(otto_llm::providers::IDS))]
    provider: Option<String>,
    /// Endpoint of a local or custom provider (ollama, lmstudio, custom).
    #[arg(long)]
    base_url: Option<String>,
    /// Read a key without echo, or pass it explicitly (visible in shell history).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "", conflicts_with_all = ["subscription", "max", "console", "device"])]
    api_key: Option<String>,
    #[arg(long)]
    subscription: bool,
    #[arg(long, conflicts_with_all = ["console", "device"])]
    max: bool,
    #[arg(long, conflicts_with = "device")]
    console: bool,
    #[arg(long)]
    device: bool,
}

#[derive(Args)]
struct AskArgs {
    #[arg(long)]
    model: Option<String>,
    #[arg(long, value_parser = ["none", "minimal", "low", "medium", "high", "xhigh", "max"])]
    effort: Option<String>,
    #[arg(long)]
    system: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    show_reasoning: bool,
    #[arg(long)]
    tools: bool,
    /// Prompt words; '-' reads standard input.
    #[arg(allow_hyphen_values = true)]
    prompt: Vec<String>,
}

/// Arguments before the subcommand when this process runs itself again: `otto` inside `silo`.
static SELF_PREFIX: OnceLock<Vec<OsString>> = OnceLock::new();

/// This executable with its subcommand prefix, for the daemon and helper processes it starts.
pub(crate) fn self_args(args: &[&str]) -> Vec<String> {
    let prefix = SELF_PREFIX.get().map(Vec::as_slice).unwrap_or_default();
    prefix.iter().map(|a| a.to_string_lossy().into_owned()).chain(args.iter().map(|a| (*a).to_owned())).collect()
}

/// Run the CLI with `args` as its argv. `args[0]` is the program; exactly `otto`
/// means the `silo otto` embedding, whose own child processes run `<exe> otto …`.
#[must_use]
pub fn main_from(args: Vec<OsString>) -> ExitCode {
    let embedded = args.first().is_some_and(|a| a == "otto");
    let _ = SELF_PREFIX.set(if embedded { vec!["otto".into()] } else { Vec::new() });
    let mut command = Cli::command();
    if embedded {
        command = command.bin_name("silo otto");
    }
    let cli = match command.try_get_matches_from(args).and_then(|m| Cli::from_arg_matches(&m)) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(2));
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(main_async(cli));
    // A blocked stdin read must not hold the exit.
    runtime.shutdown_background();
    ExitCode::from(code)
}

async fn main_async(cli: Cli) -> u8 {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into());
    if matches!(&cli.command, Command::Server(args) if args.command.is_none()) {
        match otto_server::init_logging(&otto_proto::paths::home()) {
            Ok(file) => {
                let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(file)).try_init();
            }
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        }
    } else {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(io::stderr).try_init();
    }
    let cancel = CancellationToken::new();
    let owns_signals = matches!(&cli.command, Command::Server(_) | Command::Run(_) | Command::Attach(_));
    let result = if owns_signals {
        run(cli, cancel).await
    } else {
        tokio::select! {
            result = run(cli, cancel.clone()) => result,
            signal = tokio::signal::ctrl_c() => {
                cancel.cancel();
                match signal {
                    Ok(()) => Err(LlmError::new(ErrorClass::Cancelled, "interrupted").into()),
                    Err(error) => Err(error.into()),
                }
            }
        }
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error:#}");
            let rpc_auth = matches!(error.downcast_ref::<otto_client::ClientError>(), Some(otto_client::ClientError::Rpc(error)) if error.code == otto_proto::RpcError::AUTH_REQUIRED);
            if rpc_auth {
                eprintln!("Sign in with `otto-silo auth login <provider>`.");
                return 2;
            }
            match error.downcast_ref::<LlmError>().map(|error| &error.class) {
                Some(ErrorClass::Auth) => {
                    eprintln!("Sign in with `otto-silo auth login <provider>`.");
                    2
                }
                Some(ErrorClass::Cancelled) => 130,
                _ => 1,
            }
        }
    }
}

async fn run(cli: Cli, cancel: CancellationToken) -> Result<()> {
    let home = otto_proto::paths::home();
    let providers = Providers::new(otto_proto::paths::auth_dir(&home), home)?;
    match cli.command {
        Command::Server(args) => daemon::run(args).await,
        Command::Run(args) => sessions::run(args).await,
        Command::Sessions { all } => sessions::list(all).await,
        Command::Attach(args) => sessions::attach(args).await,
        Command::Session { command } => sessions::mutate(command).await,
        Command::Cancel { id } => sessions::cancel(&id).await,
        Command::HandBack { id } => sessions::hand_back(&id).await,
        Command::Auth { command } => auth::run(command, &providers).await,
        Command::CatalogRefresh => {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), providers.refresh_catalog()).await;
            Ok(())
        }
        Command::Models { provider, all, mode, refresh } => {
            use otto_llm::auth::AuthMode;
            if refresh {
                providers.force_refresh_catalog().await?;
            } else if std::env::var_os("OTTO_SILO_OFFLINE").is_none() {
                // A separate process can finish the bounded refresh after this listing exits.
                let _ = std::process::Command::new(std::env::current_exe()?)
                    .args(self_args(&["catalog-refresh"]))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
            let requested_mode = mode.as_deref().map(|mode| if mode == "subscription" { AuthMode::Subscription } else { AuthMode::ApiKey });
            let models = if all {
                providers.catalog_entries(provider.as_deref(), requested_mode).into_iter().map(|entry| (entry, requested_mode)).collect()
            } else {
                let mut models = Vec::new();
                for status in providers.status() {
                    if provider.as_deref().is_some_and(|provider| provider != status.provider) {
                        continue;
                    }
                    if let Some(error) = status.error {
                        bail!("{error}");
                    }
                    let mode = match status.mode.as_deref() {
                        Some("api_key") if status.signed_in => AuthMode::ApiKey,
                        Some("subscription") if status.signed_in => AuthMode::Subscription,
                        _ => continue,
                    };
                    if requested_mode.is_some_and(|requested| requested != mode) {
                        continue;
                    }
                    models.extend(providers.catalog_entries(Some(&status.provider), Some(mode)).into_iter().map(|entry| (entry, Some(mode))));
                }
                models
            };
            if models.is_empty() {
                eprintln!("No models available for current sign-ins. Use --all or `otto-silo auth login <provider>`.");
            }
            println!("PROVIDER/MODEL\tNAME\tAUTH MODES\tCONTEXT\tMAX OUTPUT\tLIST INPUT/OUTPUT USD per 1M\tFLAGS\tSTATUS");
            let provider_entries = providers.catalog_providers();
            for (entry, mode) in models {
                let info = entry.info;
                let modes = entry
                    .auth
                    .into_iter()
                    .map(|mode| match mode {
                        AuthMode::ApiKey => "api_key",
                        AuthMode::Subscription => "subscription",
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let price = if mode == Some(AuthMode::Subscription) {
                    "plan".into()
                } else {
                    entry.list_pricing.map_or_else(|| "unknown".into(), |p| format!("${:.2}/${:.2}", p.input, p.output))
                };
                let caps = [
                    (info.caps.reasoning, "reasoning"),
                    (info.caps.vision, "vision"),
                    (info.caps.parallel_tools, "tools"),
                    (info.caps.prompt_cache, "cache"),
                    (entry.deprecated, "deprecated"),
                ]
                .into_iter()
                .filter_map(|(enabled, name)| enabled.then_some(name))
                .collect::<Vec<_>>()
                .join(",");
                let status =
                    if provider_entries.iter().any(|p| p.id == info.provider.as_str() && p.implemented) { "supported" } else { "not yet supported" };
                println!(
                    "{}/{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    info.provider, info.id, info.name, modes, info.caps.context_window, info.caps.max_output, price, caps, status
                );
            }
            Ok(())
        }
        Command::Ask(args) => ask::run(args, &providers, cancel).await,
    }
}

async fn prompt(label: &str, secret: bool) -> Result<String> {
    if !io::stdin().is_terminal() {
        bail!("interactive input requires a TTY; specify provider/mode and use --api-key=KEY for noninteractive login");
    }
    eprint!("{label}: ");
    io::stderr().flush()?;
    let guard = if secret { Some(EchoGuard::new()?) } else { None };
    let result = tokio::task::spawn_blocking(move || {
        let stdin = io::stdin();
        let mut line = String::new();
        stdin.read_line(&mut line)?;
        Ok(line.trim().to_owned())
    })
    .await
    .context("input task failed")?;
    drop(guard);
    if secret {
        eprintln!();
    }
    result
}

struct EchoGuard(rustix::termios::Termios);
impl EchoGuard {
    fn new() -> Result<Self> {
        use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
        let original = tcgetattr(io::stdin())?;
        let mut hidden = original.clone();
        hidden.local_modes.remove(LocalModes::ECHO);
        tcsetattr(io::stdin(), OptionalActions::Now, &hidden)?;
        Ok(Self(original))
    }
}
impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(io::stdin(), rustix::termios::OptionalActions::Now, &self.0);
    }
}

async fn stdin_text() -> Result<String> {
    tokio::task::spawn_blocking(|| {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        Ok(text)
    })
    .await
    .context("stdin task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parsing() {
        let cli = Cli::try_parse_from(["otto-silo", "auth", "login", "anthropic", "--api-key"]).unwrap();
        assert!(matches!(cli.command, Command::Auth { command: AuthCommand::Login(LoginArgs { api_key: Some(key), .. }) } if key.is_empty()));
        assert!(Cli::try_parse_from(["otto-silo", "auth", "login", "--api-key=secret", "--subscription"]).is_err());
        assert!(Cli::try_parse_from(["otto-silo", "auth", "login", "--max", "--device"]).is_err());
        for provider in otto_llm::providers::IDS {
            assert!(Cli::try_parse_from(["otto-silo", "auth", "login", provider, "--api-key=k"]).is_ok(), "{provider}");
            assert!(Cli::try_parse_from(["otto-silo", "models", "--provider", provider]).is_ok(), "{provider}");
        }
        assert!(Cli::try_parse_from(["otto-silo", "auth", "login", "nowhere"]).is_err());
        assert!(Cli::try_parse_from(["otto-silo", "auth", "login", "ollama", "--base-url", "http://127.0.0.1:11434"]).is_ok());
        assert!(Cli::try_parse_from(["otto-silo", "ask", "--effort", "xhigh", "--tools", "hello"]).is_ok());
        assert!(Cli::try_parse_from(["otto-silo", "ask", "--effort", "invalid"]).is_err());
    }
}
