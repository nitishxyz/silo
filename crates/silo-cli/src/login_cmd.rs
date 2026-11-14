//! `silo login-server`: run the login server and manage who may log in
//! (`docs/LOGIN_SERVER.md` §4, §8). The daemon is `silo_login`; this is
//! its command line.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use silo_client::transport::{EndpointAddr, LoginTicket, RelayConfig};
use silo_login::{
    Broker,
    admin::{self, AdminRequest, NotRunning},
    policy::Policy,
    spawner::{AsUser, Spawner, Unprivileged},
    users::{self, System},
};

use crate::{parse_duration, parse_relay};

/// Where the policy and the daemon's state are.
#[derive(clap::Args, Clone, Debug)]
pub struct Paths {
    /// The accounts file.
    #[arg(long, default_value = silo_login::DEFAULT_ACCOUNTS)]
    pub accounts: PathBuf,
    /// The daemon's state directory (identity key, admin socket).
    #[arg(long, default_value = silo_login::DEFAULT_STATE_DIR)]
    pub state_dir: PathBuf,
}

#[derive(clap::Subcommand, Clone, Debug)]
pub enum LoginCommand {
    /// Run the login server (as root; the systemd unit runs this).
    Run {
        #[command(flatten)]
        paths: Paths,
        /// Relays: `default`, `disabled` or a relay URL.
        #[arg(long, value_parser = parse_relay, default_value = "default")]
        relay: RelayConfig,
        /// Fixed UDP port; a random one by default.
        #[arg(long)]
        port: Option<u16>,
        /// Do not advertise or discover on the LAN.
        #[arg(long)]
        no_mdns: bool,
        /// Not root: serve only your own account, for trying it out.
        #[arg(long)]
        unprivileged: bool,
    },
    /// Print a single-use login ticket for an account.
    Invite {
        account: String,
        /// How long it stays valid (`10m`, `1d`; at most 7 days).
        #[arg(long, value_parser = parse_duration, default_value = "1d")]
        ttl: Duration,
        /// Also print it as a QR code.
        #[arg(long)]
        qr: bool,
        #[command(flatten)]
        paths: Paths,
    },
    /// Let a device (its id) use an account.
    Allow {
        account: String,
        device: String,
        #[command(flatten)]
        paths: Paths,
    },
    /// Stop a device using an account; its server revokes it.
    Deny {
        account: String,
        device: String,
        #[command(flatten)]
        paths: Paths,
    },
    /// List the accounts and their devices.
    List {
        #[command(flatten)]
        paths: Paths,
    },
    /// Print the machine's login ticket (no invite): `account@` in front
    /// picks the account.
    Ticket {
        /// The account to put in it.
        account: Option<String>,
        #[command(flatten)]
        paths: Paths,
    },
    /// Re-read the accounts file.
    Reload {
        #[command(flatten)]
        paths: Paths,
    },
    /// Install the systemd unit and an empty accounts file, and start it.
    Install {
        #[arg(long, default_value = silo_login::DEFAULT_ACCOUNTS)]
        accounts: PathBuf,
    },
}

/// Run `command`; the exit code.
///
/// # Errors
/// The command failed.
pub async fn run(command: LoginCommand) -> anyhow::Result<i32> {
    match command {
        LoginCommand::Run { paths, relay, port, no_mdns, unprivileged } => {
            let filter = tracing_subscriber::EnvFilter::try_from_env("SILO_LOG").unwrap_or_else(|_| "info,iroh::net_report=error".into());
            let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
            let options = silo_login::RunOptions { accounts: paths.accounts, state_dir: paths.state_dir, relay, port, mdns: !no_mdns, unprivileged };
            silo_login::run(options).await?;
        }
        LoginCommand::Invite { account, ttl, qr, paths } => {
            let reply = admin::request(&paths.state_dir, &AdminRequest::Invite { account: account.clone(), ttl_secs: ttl.as_secs() }).await?;
            let ticket = ticket_of(&reply)?;
            println!("{ticket}");
            if qr {
                println!("{}", crate::qr(&ticket)?);
            }
            eprintln!("Single use, valid for {}: in Silo, Add Host with it, or `silo hosts add <ticket>`.", human(ttl));
        }
        LoginCommand::Allow { account, device, paths } => {
            let changed = match admin::request(&paths.state_dir, &AdminRequest::Allow { account: account.clone(), device: device.clone() }).await {
                Err(error) if error.downcast_ref::<NotRunning>().is_some() => local(&paths)?.allow(&account, &device).await?,
                reply => reply?.get("changed").and_then(serde_json::Value::as_bool).unwrap_or(true),
            };
            println!("{}", if changed { format!("{device} may use {account}") } else { format!("{device} could already use {account}") });
        }
        LoginCommand::Deny { account, device, paths } => {
            let changed = match admin::request(&paths.state_dir, &AdminRequest::Deny { account: account.clone(), device: device.clone() }).await {
                Err(error) if error.downcast_ref::<NotRunning>().is_some() => local(&paths)?.deny(&account, &device).await?,
                reply => reply?.get("changed").and_then(serde_json::Value::as_bool).unwrap_or(true),
            };
            println!("{}", if changed { format!("{device} may no longer use {account}") } else { format!("{device} could not use {account}") });
        }
        LoginCommand::List { paths } => print!("{}", format_policy(&Policy::load(&paths.accounts)?)),
        LoginCommand::Ticket { account, paths } => {
            if account.as_deref().is_some_and(|a| !silo_client::transport::valid_account(a)) {
                anyhow::bail!("{:?} is not a user name", account.unwrap_or_default());
            }
            let ticket = match admin::request(&paths.state_dir, &AdminRequest::Ticket { account: account.clone() }).await {
                Ok(reply) => ticket_of(&reply)?,
                Err(error) if error.downcast_ref::<NotRunning>().is_some() => {
                    // Its id only: devices find its relay and addresses by
                    // lookup once it runs.
                    let id = silo_login::daemon::identity(&paths.state_dir)?.id();
                    eprintln!("The login server is not running; this ticket holds only its id.");
                    LoginTicket { addr: EndpointAddr::new(id), secret: None, account }.encode()
                }
                Err(error) => return Err(error),
            };
            println!("{ticket}");
        }
        LoginCommand::Reload { paths } => {
            admin::request(&paths.state_dir, &AdminRequest::Reload).await?;
            println!("reloaded {}", paths.accounts.display());
        }
        LoginCommand::Install { accounts } => {
            let exe = std::env::current_exe().context("this binary's path")?;
            let installed = silo_login::install::install(&exe, &accounts)?;
            println!("installed {} and started it", installed.unit.display());
            if installed.created_accounts {
                println!("created {} (nobody may log in yet): `silo login-server invite <account>`", installed.accounts.display());
            }
        }
    }
    Ok(0)
}

fn ticket_of(reply: &serde_json::Value) -> anyhow::Result<String> {
    reply.get("ticket").and_then(serde_json::Value::as_str).map(str::to_owned).context("the login server sent no ticket")
}

/// A broker in this process, for changing the accounts file (and
/// revoking) while the daemon is not running.
fn local(paths: &Paths) -> anyhow::Result<Broker> {
    let exe = std::env::current_exe().context("this binary's path")?;
    let (spawner, only): (Arc<dyn Spawner>, Option<String>) = if rustix::process::geteuid().is_root() {
        (Arc::new(AsUser::new(exe)), None)
    } else {
        let me = users::current().context("this process's account")?;
        (Arc::new(Unprivileged::new(exe, silo_proto::paths::home())), Some(me.name))
    };
    Broker::new(paths.accounts.clone(), Arc::new(System), spawner, only)
}

/// The accounts and their devices, one line each.
#[must_use]
pub fn format_policy(policy: &Policy) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    if policy.accounts().values().all(Vec::is_empty) {
        return "no devices may log in yet; `silo login-server invite <account>`\n".to_owned();
    }
    for (account, devices) in policy.accounts() {
        for device in devices {
            let default = policy.defaults().get(&device.id).is_some_and(|d| d == account);
            let _ = writeln!(out, "{account:<12} {} {}{}", device.id, device.name, if default { " (default)" } else { "" });
        }
    }
    out
}

fn human(ttl: Duration) -> String {
    match ttl.as_secs() {
        secs @ 0..120 => format!("{secs}s"),
        secs @ 120..7200 => format!("{}m", secs / 60),
        secs @ 7200..172_800 => format!("{}h", secs / 3600),
        secs => format!("{}d", secs / 86_400),
    }
}
