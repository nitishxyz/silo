//! MCP client for otto (docs/OTTO.md §5): configured servers' tools as
//! otto tools named `mcp__<server>__<tool>`.
//!
//! Stdio and streamable HTTP transports, a hand-written JSON-RPC client
//! (protocol `2025-06-18`). Servers start lazily the first time a session in
//! their project asks for tools, restart after a crash, and stop with
//! [`otto_tools::ProjectSources::shutdown`].
pub mod client;
pub mod config;
pub mod schema;
pub mod server;

use async_trait::async_trait;
use otto_tools::{ProjectSources, Tool};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub use server::Server;

/// Every configured server of every project, shared by configuration:
/// identical user-level servers run once.
pub struct Mcp {
    user_config_dir: PathBuf,
    servers: tokio::sync::Mutex<HashMap<String, Arc<Server>>>,
    /// Last problem reported per server, so failures are logged once.
    reported: Mutex<HashMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    /// `running`, `stopped`, `disabled`, or an error.
    pub state: String,
    pub tools: usize,
}

impl Mcp {
    pub fn new(user_config_dir: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            user_config_dir: user_config_dir.into(),
            servers: tokio::sync::Mutex::new(HashMap::new()),
            reported: Mutex::new(HashMap::new()),
        })
    }

    async fn servers(&self, root: &Path) -> Vec<Result<Arc<Server>, (String, String)>> {
        let mut map = self.servers.lock().await;
        config::load(&self.user_config_dir, root)
            .into_iter()
            .filter(|(_, entry)| entry.as_ref().map_or(true, |c| !c.disabled))
            .map(|(name, entry)| {
                let config = entry.map_err(|e| (name.clone(), e))?;
                let key = format!("{name}\u{0}{}", serde_json::to_string(&config).unwrap_or_default());
                Ok(map.entry(key).or_insert_with(|| Arc::new(Server::new(name, config))).clone())
            })
            .collect()
    }

    fn report(&self, server: &str, problem: &str) {
        let mut reported = self.reported.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if reported.get(server).is_none_or(|p| p != problem) {
            tracing::warn!(server, problem, "MCP server unavailable");
            reported.insert(server.into(), problem.into());
        }
    }

    /// Configured servers for a project and their state, without starting any.
    pub async fn status(&self, root: &Path) -> Vec<ServerStatus> {
        let mut out = BTreeMap::new();
        for (name, entry) in config::load(&self.user_config_dir, root) {
            if let Ok(config) = &entry
                && config.disabled
            {
                out.insert(name.clone(), ServerStatus { name, state: "disabled".into(), tools: 0 });
            }
        }
        for server in self.servers(root).await {
            let status = match server {
                Ok(server) => {
                    let (state, tools) = server.status().await;
                    ServerStatus { name: server.name.clone(), state, tools }
                }
                Err((name, error)) => ServerStatus { name, state: error, tools: 0 },
            };
            out.insert(status.name.clone(), status);
        }
        out.into_values().collect()
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
        let servers: Vec<_> = self.servers.lock().await.drain().map(|(_, s)| s).collect();
        futures::future::join_all(servers.iter().map(|s| s.shutdown())).await;
    }
}
