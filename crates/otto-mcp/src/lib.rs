//! MCP client for otto (docs/OTTO.md §5): configured servers' tools as
//! otto tools named `mcp__<server>__<tool>`, their resources and prompts as tools,
//! their status, configuration edits and OAuth sign-in.
//!
//! Stdio and streamable HTTP transports, a hand-written JSON-RPC client
//! (protocol `2025-06-18`). Servers start lazily the first time a session in
//! their project asks for tools (or [`Mcp::list`] shows them), restart after a
//! crash, and stop with [`otto_tools::ProjectSources::shutdown`].
pub mod client;
pub mod config;
mod extra;
#[cfg(feature = "fake")]
pub mod fake;
mod http;
mod manage;
pub mod oauth;
pub mod schema;
pub mod server;
pub mod settings;
mod signin;
pub mod sse;
mod stdio;

use async_trait::async_trait;
use otto_tools::{ProjectSources, Tool};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

pub use config::Scope;
pub use manage::Listed;
pub use server::{Server, Status};
pub use signin::SignIn;

/// What clients hear: a server's status or tool count changed, or the configuration did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Status {
        name: String,
        status: Status,
        tools: usize,
    },
    /// Servers were added, changed or removed (through [`Mcp`]).
    Changed,
}

pub(crate) struct Shared {
    pub(crate) events: broadcast::Sender<Event>,
    pub(crate) tokens: Arc<oauth::TokenStore>,
}

/// Every configured server of every project, shared by configuration:
/// identical user-level servers run once.
pub struct Mcp {
    user_config_dir: PathBuf,
    shared: Arc<Shared>,
    servers: tokio::sync::Mutex<HashMap<String, Arc<Server>>>,
    /// Last problem reported per server, so failures are logged once.
    reported: Mutex<HashMap<String, String>>,
    logins: tokio::sync::Mutex<HashMap<String, signin::Pending>>,
}

impl Mcp {
    /// Servers from `user_config_dir/mcp.json` and project files; OAuth tokens in a
    /// credential store at `token_dir` (otto uses `<home>/auth/mcp`).
    pub fn new(user_config_dir: impl Into<PathBuf>, token_dir: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            user_config_dir: user_config_dir.into(),
            shared: Arc::new(Shared { events: broadcast::channel(256).0, tokens: Arc::new(oauth::TokenStore::new(token_dir)) }),
            servers: tokio::sync::Mutex::new(HashMap::new()),
            reported: Mutex::new(HashMap::new()),
            logins: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.shared.events.subscribe()
    }

    fn server(&self, map: &mut HashMap<String, Arc<Server>>, name: &str, config: &config::ServerConfig) -> Arc<Server> {
        let key = format!("{name}\u{0}{}", serde_json::to_string(config).unwrap_or_default());
        map.entry(key).or_insert_with(|| Server::new(name.to_owned(), config.clone(), self.shared.clone())).clone()
    }

    async fn servers(&self, root: &Path) -> Vec<Result<Arc<Server>, (String, String)>> {
        let mut map = self.servers.lock().await;
        config::configured(&self.user_config_dir, Some(root))
            .into_values()
            .filter(|c| c.enabled)
            .map(|c| {
                let name = c.name;
                let config = c.entry.map_err(|e| (name.clone(), e))?;
                let server = self.server(&mut map, &name, &config);
                server.add_root(root);
                Ok(server)
            })
            .collect()
    }

    fn report(&self, server: &str, problem: &str) {
        let mut reported = client::lock(&self.reported);
        if reported.get(server).is_none_or(|p| p != problem) {
            tracing::warn!(server, problem, "MCP server unavailable");
            reported.insert(server.into(), problem.into());
        }
    }

    /// Stop and forget running servers named `name` (their configuration changed).
    async fn drop_servers(&self, name: &str) {
        let stopped: Vec<Arc<Server>> = {
            let mut map = self.servers.lock().await;
            let keys: Vec<String> = map.iter().filter(|(_, s)| s.name == name).map(|(k, _)| k.clone()).collect();
            keys.iter().filter_map(|k| map.remove(k)).collect()
        };
        futures::future::join_all(stopped.iter().map(|s| s.shutdown())).await;
        client::lock(&self.reported).remove(name);
    }
}

#[async_trait]
impl ProjectSources for Mcp {
    async fn tools(&self, project_root: &Path) -> Vec<Arc<dyn Tool>> {
        let servers = self.servers(project_root).await;
        let listed = futures::future::join_all(servers.into_iter().map(|server| async move {
            match server {
                Ok(server) => server.tools().await.map_err(|e| (server.name.clone(), e)),
                Err(error) => Err(error),
            }
        }))
        .await;
        let mut tools = Vec::new();
        for result in listed {
            match result {
                Ok(listed) => tools.extend(listed),
                Err((name, error)) => self.report(&name, &error),
            }
        }
        tools
    }

    async fn shutdown(&self) {
        for (_, login) in self.logins.lock().await.drain() {
            login.task.abort();
        }
        let servers: Vec<_> = self.servers.lock().await.drain().map(|(_, s)| s).collect();
        futures::future::join_all(servers.iter().map(|s| s.shutdown())).await;
    }
}
