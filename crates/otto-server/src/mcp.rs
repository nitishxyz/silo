//! `mcp.*`: MCP server settings over [`otto_mcp::Mcp`], and its events as global events.
use otto_mcp::{Listed, Mcp, Scope, Status, config::ServerConfig};
use otto_proto::*;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::broadcast::{self, error::RecvError};

fn scope(scope: McpScope) -> Scope {
    match scope {
        McpScope::Global => Scope::Global,
        McpScope::Project => Scope::Project,
    }
}

fn status(status: Status, enabled: bool) -> McpStatus {
    match status {
        _ if !enabled => McpStatus::Disabled,
        Status::Idle | Status::Connecting => McpStatus::Connecting,
        Status::Connected => McpStatus::Connected,
        Status::NeedsSignIn => McpStatus::NeedsSignIn,
        Status::Error(message) => McpStatus::Error { message },
    }
}

fn transport(config: ServerConfig) -> McpTransport {
    if config.is_http() {
        McpTransport::Http { url: config.url.unwrap_or_default(), headers: config.headers }
    } else {
        McpTransport::Stdio { command: config.command.unwrap_or_default(), args: config.args, env: config.env }
    }
}

fn config(transport: McpTransport) -> ServerConfig {
    match transport {
        McpTransport::Stdio { command, args, env } => ServerConfig { command: Some(command), args, env, ..Default::default() },
        McpTransport::Http { url, headers } => ServerConfig { kind: Some("http".into()), url: Some(url), headers, ..Default::default() },
    }
}

fn server(listed: Listed) -> McpServer {
    McpServer {
        name: listed.name,
        scope: match listed.scope {
            Scope::Global => McpScope::Global,
            Scope::Project => McpScope::Project,
        },
        transport: transport(listed.config),
        status: status(listed.status, listed.enabled),
        enabled: listed.enabled,
        tools: u32::try_from(listed.tools).unwrap_or(u32::MAX),
    }
}

pub(crate) fn event(event: otto_mcp::Event) -> Event {
    match event {
        otto_mcp::Event::Status { name, status: s, tools } => {
            Event::McpStatus { name, status: status(s, true), tools: u32::try_from(tools).unwrap_or(u32::MAX) }
        }
        otto_mcp::Event::Changed => Event::McpChanged {},
    }
}

/// The runtime's global events and the MCP events, as one stream; it closes with the runtime's.
pub(crate) fn merge(mut runtime: broadcast::Receiver<Event>, mut mcp: broadcast::Receiver<otto_mcp::Event>) -> broadcast::Receiver<Event> {
    let (tx, rx) = broadcast::channel(256);
    tokio::spawn(async move {
        let mut mcp_open = true;
        loop {
            let event = tokio::select! {
                event = runtime.recv() => match event {
                    Ok(event) => event,
                    Err(RecvError::Lagged(_)) => Event::SessionsChanged {},
                    Err(RecvError::Closed) => return,
                },
                event = mcp.recv(), if mcp_open => match event {
                    Ok(e) => self::event(e),
                    Err(RecvError::Lagged(_)) => Event::McpChanged {},
                    Err(RecvError::Closed) => {
                        mcp_open = false;
                        continue;
                    }
                },
            };
            if tx.send(event).is_err() {
                return;
            }
        }
    });
    rx
}

fn root(project_root: Option<String>) -> Result<Option<PathBuf>, RpcError> {
    match project_root.filter(|r| !r.is_empty()).map(PathBuf::from) {
        Some(root) if !root.is_absolute() => Err(RpcError::bad_request("project_root must be absolute")),
        root => Ok(root),
    }
}

fn error(message: String) -> RpcError {
    if message.starts_with("no server named") || message == "no such sign-in" { RpcError::not_found(message) } else { RpcError::bad_request(message) }
}

/// The listed row of `name` (a global server as the global list shows it).
async fn row(mcp: &Mcp, root: Option<&Path>, scope: McpScope, name: &str) -> Result<Value, RpcError> {
    let root = if scope == McpScope::Global { None } else { root };
    let listed = mcp.list(root).await.into_iter().find(|l| l.name == name).ok_or_else(|| RpcError::not_found(name))?;
    Ok(json!(server(listed)))
}

pub(crate) async fn dispatch(mcp: &Arc<Mcp>, method: Method) -> Result<Value, RpcError> {
    match method {
        Method::McpList { project_root } => {
            let root = root(project_root)?;
            Ok(json!(mcp.list(root.as_deref()).await.into_iter().map(server).collect::<Vec<_>>()))
        }
        Method::McpAdd { project_root, scope: s, name, transport, enabled } => {
            let root = root(project_root)?;
            mcp.add(root.as_deref(), scope(s), &name, &config(transport), enabled).await.map_err(error)?;
            row(mcp, root.as_deref(), s, &name).await
        }
        Method::McpUpdate { project_root, scope: s, name, transport, enabled } => {
            let root = root(project_root)?;
            mcp.update(root.as_deref(), scope(s), &name, transport.map(config).as_ref(), enabled).await.map_err(error)?;
            row(mcp, root.as_deref(), s, &name).await
        }
        Method::McpRemove { project_root, scope: s, name } => {
            mcp.remove(root(project_root)?.as_deref(), scope(s), &name).await.map_err(error)?;
            Ok(json!({}))
        }
        Method::McpLogin { project_root, name } => {
            let started = mcp.login(root(project_root)?.as_deref(), &name).await.map_err(error)?;
            let callback = LoginCallback { port: started.port, path: started.path };
            Ok(json!(McpLoginStarted { login_id: started.login_id, url: started.url, callback }))
        }
        Method::McpLoginComplete { login_id, code } => {
            let (name, root) = mcp.login_complete(&login_id, code.as_deref()).await.map_err(error)?;
            row(mcp, root.as_deref(), if root.is_some() { McpScope::Project } else { McpScope::Global }, &name).await
        }
        Method::McpLoginCancel { login_id } => {
            mcp.login_cancel(&login_id).await;
            Ok(json!({}))
        }
        Method::McpLogout { project_root, name } => {
            mcp.logout(root(project_root)?.as_deref(), &name).await.map_err(error)?;
            Ok(json!({}))
        }
        _ => Err(RpcError::internal("not an mcp method")),
    }
}
