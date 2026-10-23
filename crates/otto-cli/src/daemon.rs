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
    Ok(otto_client::connect_or_start(&paths::home(), &std::env::current_exe()?, &["server", "--daemon"]).await?)
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
    let providers = otto_llm::Providers::new(paths::auth_dir(&home), home.clone())?;
    let services = Arc::new(crate::services::Services::new(providers)?);
    let store = Arc::new(Mutex::new(otto_store::Store::open(paths::db(&home))?));
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
    let silo = otto_silo_tools::connect(otto_silo_tools::Config::default());
    runtime.add_tools(silo.tools);
    runtime.set_terminals(silo.terminals);
    runtime.set_renderer(silo.renderer);
    runtime.add_source(otto_mcp::Mcp::new(paths::user_config()));
    // Children that finished while their parent was closed, or before a restart.
    runtime.deliver_pending().await?;
    let server = otto_server::Server::start(otto_server::ServerConfig {
        home: home.clone(),
        runtime,
        models: services.clone(),
        auth: services,
        idle_exit: args.idle_exit.or(args.daemon.then_some(300)).map(Duration::from_secs),
    })
    .await?;
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
