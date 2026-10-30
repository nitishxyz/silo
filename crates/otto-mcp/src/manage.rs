//! Listing servers with their status, and changing the configuration.
use crate::{
    Event, Mcp, Status,
    config::{self, Scope, ServerConfig},
    settings,
};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// One configured server for settings.
#[derive(Clone, Debug)]
pub struct Listed {
    pub name: String,
    pub scope: Scope,
    /// As written in its file (variables unexpanded).
    pub config: ServerConfig,
    pub enabled: bool,
    pub status: Status,
    /// The server's own tools (resources and prompts not counted).
    pub tools: usize,
}

/// The transport keys of `config`: `type` and `command`/`args`/`env` or `url`/`headers`.
fn transport(config: &ServerConfig) -> Map<String, Value> {
    let mut out = Map::new();
    let value = serde_json::to_value(config).unwrap_or_default();
    for key in ["type", "command", "args", "env", "url", "headers"] {
        if let Some(v) = value.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    out
}

fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err("a server name is 1–64 letters, digits, '-', '_' or '.'".into());
    }
    Ok(())
}

fn valid_transport(config: &ServerConfig) -> Result<(), String> {
    match (config.is_http(), config.url.as_deref(), config.command.as_deref()) {
        (true, Some(url), _) if url.starts_with("http://") || url.starts_with("https://") => Ok(()),
        (true, _, _) => Err("an HTTP server needs an http:// or https:// url".into()),
        (false, _, Some(command)) if !command.trim().is_empty() => Ok(()),
        _ => Err("a stdio server needs a command".into()),
    }
}

impl Mcp {
    fn file(&self, root: Option<&Path>, scope: Scope, name: &str) -> Result<PathBuf, String> {
        match (scope, root) {
            (Scope::Global, _) => Ok(settings::user_file(&self.user_config_dir)),
            (Scope::Project, Some(root)) => Ok(settings::project_file(root, name)),
            (Scope::Project, None) => Err("a project server needs a project root".into()),
        }
    }

    /// Every server configured for `root` (or only the user's without one), starting
    /// enabled servers that have not started so their status and tools show.
    pub async fn list(&self, root: Option<&Path>) -> Vec<Listed> {
        let mut out = Vec::new();
        let mut map = self.servers.lock().await;
        for (name, configured) in config::configured(&self.user_config_dir, root) {
            let (enabled, status, tools) = match &configured.entry {
                _ if !configured.enabled => (false, Status::Idle, 0),
                Err(error) => (true, Status::Error(error.clone()), 0),
                Ok(config) => {
                    let server = self.server(&mut map, &name, config);
                    if let Some(root) = root {
                        server.add_root(root);
                    }
                    server.start();
                    let (status, tools) = server.status();
                    (true, if status == Status::Idle { Status::Connecting } else { status }, tools)
                }
            };
            out.push(Listed { name, scope: configured.scope, config: configured.raw, enabled, status, tools });
        }
        out
    }

    /// A new server in the user's `mcp.json` or the project's `.mcp.json`.
    pub async fn add(&self, root: Option<&Path>, scope: Scope, name: &str, config: &ServerConfig, enabled: bool) -> Result<(), String> {
        valid_name(name)?;
        valid_transport(config)?;
        let path = self.file(root, scope, name)?;
        if settings::entry(&path, name).is_some() {
            return Err(format!("a server named {name} already exists"));
        }
        let mut entry = transport(config);
        if let (Scope::Project, Some(root)) = (scope, root) {
            settings::set_project_disabled(&self.user_config_dir, root, name, !enabled)?;
        } else if !enabled {
            entry.insert("disabled".into(), Value::Bool(true));
        }
        settings::put(&path, name, entry)?;
        self.changed(name).await;
        Ok(())
    }

    /// Replace a server's transport and/or turn it on or off; other keys stay.
    pub async fn update(
        &self,
        root: Option<&Path>,
        scope: Scope,
        name: &str,
        config: Option<&ServerConfig>,
        enabled: Option<bool>,
    ) -> Result<(), String> {
        let path = self.file(root, scope, name)?;
        let before = settings::entry(&path, name).ok_or_else(|| format!("no server named {name}"))?;
        let mut entry = before.clone();
        if let Some(config) = config {
            valid_transport(config)?;
            for key in ["type", "command", "args", "env", "url", "headers"] {
                entry.remove(key);
            }
            entry.extend(transport(config));
        }
        if let Some(enabled) = enabled {
            if let (Scope::Project, Some(root)) = (scope, root) {
                settings::set_project_disabled(&self.user_config_dir, root, name, !enabled)?;
                if enabled {
                    entry.remove("disabled");
                }
            } else if enabled {
                entry.remove("disabled");
            } else {
                entry.insert("disabled".into(), Value::Bool(true));
            }
        }
        if entry != before {
            settings::put(&path, name, entry)?;
        }
        self.changed(name).await;
        Ok(())
    }

    /// Remove a server from its scope's files.
    pub async fn remove(&self, root: Option<&Path>, scope: Scope, name: &str) -> Result<(), String> {
        let removed = match (scope, root) {
            (Scope::Global, _) => settings::remove(&settings::user_file(&self.user_config_dir), name)?,
            (Scope::Project, Some(root)) => {
                let a = settings::remove(&root.join(".otto/mcp.json"), name)?;
                let b = settings::remove(&root.join(".mcp.json"), name)?;
                settings::set_project_disabled(&self.user_config_dir, root, name, false)?;
                a || b
            }
            (Scope::Project, None) => return Err("a project server needs a project root".into()),
        };
        if !removed {
            return Err(format!("no server named {name}"));
        }
        self.changed(name).await;
        Ok(())
    }

    async fn changed(&self, name: &str) {
        self.drop_servers(name).await;
        let _ = self.shared.events.send(Event::Changed);
    }
}
