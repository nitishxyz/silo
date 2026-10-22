//! Server configuration: Claude Code's `.mcp.json` format.
//!
//! Merged by name, later wins: user `mcp.json` (in otto's config dir), project
//! `.mcp.json`, project `.otto/mcp.json`. Each file holds `{"mcpServers": {name: server}}`.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// `stdio` (default with `command`), `http` / `streamable-http` (default with `url`).
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Working directory; project servers default to the project root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    /// Tools load through `load_tools`: `true`, `false`, or unset to defer
    /// only servers with many tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<bool>,
    /// Per-call timeout in milliseconds (default 10 minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Start + initialize timeout in milliseconds (default 30 seconds).
    #[serde(default, rename = "startupTimeout", skip_serializing_if = "Option::is_none")]
    pub startup_timeout: Option<u64>,
}

impl ServerConfig {
    pub fn is_http(&self) -> bool {
        matches!(self.kind.as_deref(), Some("http" | "streamable-http" | "streamable_http")) || (self.kind.is_none() && self.url.is_some())
    }
}

/// `${VAR}` and `${VAR:-default}`; an unset variable without a default is an error.
pub fn expand(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let end = rest[start..].find('}').ok_or_else(|| format!("unclosed ${{ in {text}"))? + start;
        let inner = &rest[start + 2..end];
        let (name, default) = inner.split_once(":-").map_or((inner, None), |(n, d)| (n, Some(d)));
        out.push_str(&env(name).or_else(|| default.map(String::from)).ok_or_else(|| format!("environment variable {name} is not set"))?);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn expanded(mut config: ServerConfig, env: &dyn Fn(&str) -> Option<String>) -> Result<ServerConfig, String> {
    let one = |s: &mut String| -> Result<(), String> {
        *s = expand(s, env)?;
        Ok(())
    };
    if let Some(command) = &mut config.command {
        one(command)?;
    }
    if let Some(url) = &mut config.url {
        one(url)?;
    }
    for value in config.args.iter_mut().chain(config.env.values_mut()).chain(config.headers.values_mut()) {
        one(value)?;
    }
    Ok(config)
}

/// One configured server: its config, or why it cannot be used.
pub type Entry = Result<ServerConfig, String>;

/// Every configured server for `project_root`, by name.
pub fn load(user_config_dir: &Path, project_root: &Path) -> BTreeMap<String, Entry> {
    let env = |name: &str| std::env::var(name).ok();
    let mut servers = BTreeMap::new();
    for (path, project) in
        [(user_config_dir.join("mcp.json"), false), (project_root.join(".mcp.json"), true), (project_root.join(".otto/mcp.json"), true)]
    {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(path=%path.display(), error=%e, "unreadable MCP config");
                continue;
            }
        };
        let file: Value = match serde_json::from_str(&text) {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!(path=%path.display(), error=%e, "invalid MCP config");
                continue;
            }
        };
        for (name, server) in file.get("mcpServers").or_else(|| file.get("servers")).and_then(Value::as_object).into_iter().flatten() {
            let entry =
                serde_json::from_value::<ServerConfig>(server.clone()).map_err(|e| e.to_string()).and_then(|c| expanded(c, &env)).map(|mut c| {
                    if project && c.cwd.is_none() && !c.is_http() {
                        c.cwd = Some(project_root.to_path_buf());
                    } else if let Some(cwd) = c.cwd.as_mut().filter(|cwd| cwd.is_relative()) {
                        *cwd = project_root.join(&*cwd);
                    }
                    c
                });
            servers.insert(name.clone(), entry);
        }
    }
    servers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion_with_defaults() {
        let env = |name: &str| (name == "HOME_DIR").then(|| "/h".to_owned());
        assert_eq!(expand("${HOME_DIR}/x ${MISSING:-d}", &env).unwrap(), "/h/x d");
        assert!(expand("${MISSING}", &env).is_err());
    }

    #[test]
    fn project_overrides_user_and_gets_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join("mcp.json"), r#"{"mcpServers":{"a":{"command":"one"},"b":{"url":"https://x"}}}"#).unwrap();
        std::fs::write(dir.path().join(".mcp.json"), r#"{"mcpServers":{"a":{"command":"two","args":["${OTTO_MCP_TEST_UNSET:-z}"]}}}"#).unwrap();
        let servers = load(&user, dir.path());
        let a = servers["a"].as_ref().unwrap();
        assert_eq!(a.command.as_deref(), Some("two"));
        assert_eq!(a.args, ["z"]);
        assert_eq!(a.cwd.as_deref(), Some(dir.path()));
        assert!(servers["b"].as_ref().unwrap().is_http());
    }
}
