use anyhow::Result;
use clap::{Args, Subcommand};
use otto_client::Connection;
use otto_proto::{Method, paths};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Args)]
pub struct ServerArgs {
    #[command(subcommand)]
    pub command: Option<ServerCommand>,
    #[arg(long, conflicts_with = "daemon")]
    pub foreground: bool,
    #[arg(long)]
    pub daemon: bool,
    #[arg(long)]
    pub idle_exit: Option<u64>,
}
#[derive(Subcommand)]
pub enum ServerCommand {
    /// Stop the running daemon.
    Stop,
    /// Show the running daemon, or say that none is running.
    Status,
}

pub async fn connect() -> Result<Connection> {
    let args = crate::self_args(&["server", "--daemon"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let connection = otto_client::connect_or_start(&paths::home(), &std::env::current_exe()?, &args).await?;
    let mut told = false;
    connection
        .ready(|starting| {
            if matches!(starting.phase, otto_proto::StartPhase::Migrating | otto_proto::StartPhase::Compacting) && !std::mem::replace(&mut told, true)
            {
                eprintln!("otto is updating its data…");
            }
        })
        .await?;
    Ok(connection)
}
pub async fn run(args: ServerArgs) -> Result<()> {
    let home = paths::home();
    if let Some(command) = args.command {
        let Ok(connection) = Connection::connect(&home).await else {
            println!("not running");
            return Ok(());
        };
        let method = match command {
            ServerCommand::Stop => Method::ServerShutdown {},
            ServerCommand::Status => Method::ServerInfo {},
        };
        println!("{}", connection.call(method).await?);
        return Ok(());
    }
    // The lease and socket first: another start sees this one (starting) instead of racing on the store.
    let server = otto_server::Server::start(otto_server::ServerConfig {
        home: home.clone(),
        idle_exit: args.idle_exit.or(args.daemon.then_some(300)).map(Duration::from_secs),
        build: silo_proto::BUILD.into(),
    })
    .await?;
    tracing::info!(pid = server.info.pid, "otto starting");
    let phase = |phase, step, steps| otto_proto::Starting { phase, step, steps };
    let store = tokio::task::block_in_place(|| {
        otto_store::Store::open_with(paths::db(&home), &|migration| {
            server.starting(match migration {
                otto_store::Migration::Step { done, of } => phase(otto_proto::StartPhase::Migrating, done, of),
                otto_store::Migration::Compacting => phase(otto_proto::StartPhase::Compacting, 0, 0),
            });
        })
    })?;
    server.starting(phase(otto_proto::StartPhase::Loading, 0, 0));
    let providers = otto_llm::Providers::new(paths::auth_dir(&home), home.clone())?;
    let services = Arc::new(crate::services::Services::new(providers)?);
    let store = Arc::new(Mutex::new(store));
    let runtime = Arc::new(
        otto_core::Runtime::new(otto_core::RuntimeConfig {
            store,
            models: services.clone(),
            state_dir: home.clone(),
            user_config_dir: paths::user_config(),
        })
        .await?,
    );
    runtime.set_auto_title(true);
    // Sign-ins made by the CLI reach the clients.
    runtime.watch_auth(paths::auth_dir(&home).join("auth.json"));
    let silo = otto_silo_tools::connect(otto_silo_tools::Config::default());
    runtime.add_tools(silo.tools);
    runtime.set_terminals(silo.terminals);
    runtime.set_renderer(silo.renderer);
    let mcp = otto_mcp::Mcp::new(paths::user_config(), paths::auth_dir(&home).join("mcp"));
    runtime.add_source(mcp.clone());
    // Children that finished while their parent was closed, or before a restart.
    runtime.deliver_pending().await?;
    server.ready(otto_server::Services { runtime, models: services.clone(), auth: services, mcp }).await?;
    tracing::info!(pid = server.info.pid, "otto ready");
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let serving = server.serve_forever();
    tokio::pin!(serving);
    tokio::select! {
        result = &mut serving => result?,
        _ = async { tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} } } => {
            let connection = Connection::connect(&home).await?;
            connection.call(Method::ServerShutdown {}).await?;
            serving.await?;
        }
    }
    Ok(())
}
