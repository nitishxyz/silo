//! Otto's settings as tools through Silo (`docs/AGENTS.md` §10): agent
//! definitions, configuration and MCP servers, offered to the local silo
//! server so the agent block, `silo mcp` clients (Claude Code, Codex, …)
//! and `silo tool call` can read and change them. Each tool runs the same
//! control method the app's settings use, on this daemon's own socket.
//! Changes wait for the user's approval in Silo (`approval: true`), and
//! results never carry secrets.

use std::path::PathBuf;

use otto_client::Connection;
use serde_json::{Value, json};
use silo_proto::tools::{ProvidedTool, ToolOutcome, ToolRequest};

/// `(tool, otto method, changes something, description)`.
const TOOLS: &[(&str, &str, bool, &str)] = &[
    (
        "otto_agents_list",
        "agents.list",
        false,
        "List otto's agent definitions (built-in, user and project): name, description, model, effort, approval mode, prompt and tool grants.",
    ),
    ("otto_agent_get", "agents.get", false, "Read one otto agent definition by name (cwd selects a project's definitions)."),
    (
        "otto_agent_save",
        "agents.save",
        true,
        "Create or change an otto agent definition (user scope: otto's agents folder for every project; project scope: <cwd>/.otto/agents). Pass the whole definition (read it with otto_agent_get first); previous_name renames or clones. Tool grants are names or prefix* patterns; silo:* grants every tool from Silo (browser, terminal, files and tools provided through it). The user approves it in Silo.",
    ),
    (
        "otto_agent_delete",
        "agents.delete",
        true,
        "Delete a user or project otto agent definition (deleting an override restores the built-in). The user approves it in Silo.",
    ),
    ("otto_tools_catalog", "tools.catalog", false, "Every tool an otto agent can be granted (built-in, Silo, MCP), with its source and effects."),
    ("otto_skills_list", "skills.list", false, "The skills otto agents can load (project skills need project_root)."),
    (
        "otto_config_get",
        "config.get",
        false,
        "Read otto's configuration (one key, or all of it): default agent, provider, model, effort, approval mode and more.",
    ),
    (
        "otto_config_set",
        "config.set",
        true,
        "Set one top-level key of otto's configuration; it applies to sessions created afterwards. Read it with otto_config_get first. The user approves it in Silo.",
    ),
    (
        "otto_mcp_list",
        "mcp.list",
        false,
        "List the MCP servers otto agents use (global, and a project's with project_root), their status and tool count.",
    ),
    (
        "otto_mcp_add",
        "mcp.add",
        true,
        "Add an MCP server (stdio command or HTTP URL) to otto's global or project configuration. When an HTTP server wants an OAuth sign-in, the result's sign_in has the sign-in url to give the user (then otto_mcp_login_complete). If the server does not let otto register itself, the user registers an OAuth app with it using the URL from otto_mcp_callback_url and gives you the client id (and secret, best as ${ENV_VAR}); pass them as transport.oauth {client_id, client_secret?, callback_port?, scope?}. The user approves it in Silo.",
    ),
    ("otto_mcp_update", "mcp.update", true, "Change an otto MCP server's transport, or turn it on or off. The user approves it in Silo."),
    ("otto_mcp_remove", "mcp.remove", true, "Remove an MCP server from otto's configuration. The user approves it in Silo."),
    (
        "otto_mcp_login",
        "mcp.login",
        true,
        "Start the OAuth sign-in of an HTTP MCP server whose status is needs_sign_in. Returns the sign-in url: give it to the user as a link (or open it with browser_open) and ask them to sign in; the browser then returns to otto on its own (callback http://127.0.0.1:<port>/callback). Then call otto_mcp_login_complete with the login_id. The user approves it in Silo.",
    ),
    (
        "otto_mcp_login_complete",
        "mcp.login_complete",
        false,
        "Finish an MCP sign-in started with otto_mcp_login: waits up to 90 seconds for the user to sign in in the browser, else reports it still waiting (call again once they have). If the browser could not reach the callback (an error page at 127.0.0.1), ask the user to paste that page's full address and pass it as code. Returns the server's row (status connected when it worked).",
    ),
    (
        "otto_mcp_logout",
        "mcp.logout",
        true,
        "Forget an HTTP MCP server's sign-in tokens; it needs otto_mcp_login again. The user approves it in Silo.",
    ),
    (
        "otto_mcp_callback_url",
        "",
        false,
        "The redirect (callback) URL to register when creating an OAuth app for an MCP server that does not let otto register itself (most need one): http://127.0.0.1:<port>/callback, port 33418 unless the app was registered with another (then set transport.oauth.callback_port to it).",
    ),
];

/// How long `otto_mcp_login_complete` waits for the browser (under the
/// tool call's own timeout).
const LOGIN_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// The tools, their input schemas being the methods' params.
pub(crate) fn tools() -> Vec<ProvidedTool> {
    TOOLS
        .iter()
        .filter_map(|(name, method, changes, description)| {
            let mut input_schema = match *method {
                "" => json!({"type": "object", "properties": {"callback_port": {"type": "integer", "description": "The port the OAuth app's redirect URL uses (default 33418)"}}}),
                method => otto_proto::api::describe(method)?.params_schema,
            };
            if input_schema.get("type").and_then(Value::as_str) != Some("object") {
                input_schema = json!({"type": "object", "properties": {}});
            }
            input_schema.as_object_mut()?.remove("$schema");
            input_schema.as_object_mut()?.remove("title");
            Some(ProvidedTool { name: (*name).into(), description: (*description).into(), input_schema, read_only: !changes, approval: *changes })
        })
        .collect()
}

/// Runs calls on the daemon's own socket: a connection per call, so none
/// outlives it and keeps the daemon from its idle exit.
#[derive(Clone)]
pub(crate) struct Settings {
    home: PathBuf,
}

impl Settings {
    pub(crate) fn new(home: PathBuf) -> Self {
        Self { home }
    }

    async fn connection(&self) -> anyhow::Result<Connection> {
        Ok(Connection::connect(&self.home).await?)
    }

    pub(crate) async fn call(&self, request: ToolRequest) -> ToolOutcome {
        let Some((_, method, _, _)) = TOOLS.iter().find(|(name, ..)| *name == request.name) else {
            return ToolOutcome::error(format!("unknown tool {}", request.name));
        };
        if request.name == "otto_mcp_callback_url" {
            let port = request.input["callback_port"].as_u64().and_then(|p| u16::try_from(p).ok());
            return ToolOutcome::json(json!({"callback_url": otto_proto::McpOAuth::callback_url(port)}));
        }
        let waits = request.name == "otto_mcp_login_complete" && request.input["code"].as_str().is_none_or(str::is_empty);
        let method: otto_proto::Method = match serde_json::from_value(json!({"method": method, "params": request.input})) {
            Ok(method) => method,
            Err(error) => return ToolOutcome::error(format!("invalid input: {error}")),
        };
        let result = match self.connection().await {
            Ok(connection) => {
                let call = connection.call(method);
                let result = if waits {
                    match tokio::time::timeout(LOGIN_WAIT, call).await {
                        Ok(result) => result.map_err(anyhow::Error::from),
                        Err(_) => {
                            connection.close();
                            return ToolOutcome::text(
                                "The sign-in is not finished yet: ask the user to finish signing in in the browser, then call otto_mcp_login_complete again with the same login_id.",
                            );
                        }
                    }
                } else {
                    call.await.map_err(anyhow::Error::from)
                };
                connection.close();
                result
            }
            Err(error) => Err(error),
        };
        let result = match result {
            Ok(row) if matches!(request.name.as_str(), "otto_mcp_add" | "otto_mcp_update") => Ok(self.sign_in_if_needed(&request.input, row).await),
            other => other,
        };
        match result {
            Ok(value) => ToolOutcome::json(redact(value)),
            // An earlier wait that gave up still finished it in the background.
            Err(error) if request.name == "otto_mcp_login_complete" && format!("{error:#}").contains("no such sign-in") => ToolOutcome::error(
                "That sign-in already finished or expired. Check otto_mcp_list: status connected means it worked; needs_sign_in means start again with otto_mcp_login.",
            ),
            Err(error) => ToolOutcome::error(format!("{error:#}")),
        }
    }
}

impl Settings {
    /// One otto call on a fresh connection.
    async fn one(&self, method: otto_proto::Method) -> anyhow::Result<Value> {
        let connection = self.connection().await?;
        let result = connection.call(method).await.map_err(anyhow::Error::from);
        connection.close();
        result
    }

    /// `row` of a server just added or changed and, when it turns out to want
    /// an OAuth sign-in, that sign-in started: `{server, sign_in: {login_id,
    /// url, callback_url, next}}`, so the agent hands the user the link at once.
    async fn sign_in_if_needed(&self, input: &Value, row: Value) -> Value {
        let (Some(name), enabled) = (row["name"].as_str().map(str::to_owned), row["enabled"].as_bool().unwrap_or(true)) else { return row };
        if !enabled || row["transport"]["type"] != "http" {
            return row;
        }
        let project_root = input["project_root"].clone();
        let list = || otto_proto::Method::McpList { project_root: project_root.as_str().map(str::to_owned) };
        // It connects in the background: wait a little for the outcome.
        let mut row = row;
        let deadline = tokio::time::Instant::now() + SIGN_IN_PROBE;
        while row["status"]["state"] == "connecting" && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let Ok(Value::Array(rows)) = self.one(list()).await else { break };
            if let Some(found) = rows.into_iter().find(|r| r["name"] == name.as_str() && r["scope"] == row["scope"]) {
                row = found;
            }
        }
        if row["status"]["state"] != "needs_sign_in" {
            return row;
        }
        let login = otto_proto::Method::McpLogin { project_root: project_root.as_str().map(str::to_owned), name: name.clone() };
        let sign_in = match self.one(login).await {
            Ok(started) => json!({
                "login_id": started["login_id"],
                "url": started["url"],
                "callback_url": format!("http://127.0.0.1:{}{}", started["callback"]["port"], started["callback"]["path"].as_str().unwrap_or("/callback")),
                "next": "Give the user this url as a link to sign in (the browser returns to otto on its own), then call otto_mcp_login_complete with login_id. If their browser ends on an error page at 127.0.0.1, ask them to paste that page's address and pass it as code.",
            }),
            Err(error) => json!({
                "error": format!("{error:#}"),
                "next": "Tell the user the sign-in could not start. If the server does not let otto register itself, they register an OAuth app with it using the callback URL from otto_mcp_callback_url, then you set transport.oauth with its client id (otto_mcp_update) and call otto_mcp_login.",
            }),
        };
        json!({"server": row, "sign_in": sign_in})
    }
}

/// How long an add waits to learn whether the server wants a sign-in.
const SIGN_IN_PROBE: std::time::Duration = std::time::Duration::from_secs(15);

/// `value` without secrets: MCP servers' `env` and `headers` values, and
/// anything under a key naming a key, token, secret or password.
fn redact(value: Value) -> Value {
    fn secret(key: &str) -> bool {
        let key = key.to_ascii_lowercase();
        ["key", "token", "secret", "password", "authorization"].iter().any(|s| key.contains(s))
    }
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let v = match v {
                        Value::Object(inner) if k == "env" || k == "headers" => {
                            Value::Object(inner.into_iter().map(|(k, _)| (k, Value::String("[REDACTED]".into()))).collect())
                        }
                        Value::String(_) | Value::Number(_) if secret(&k) => Value::String("[REDACTED]".into()),
                        other => redact(other),
                    };
                    (k, v)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact).collect()),
        other => other,
    }
}

/// Offer the tools to the local silo for as long as the daemon runs.
pub(crate) fn spawn(home: PathBuf) {
    let settings = Settings::new(home);
    let config = otto_silo_tools::Config { client_name: "otto settings".into(), ..otto_silo_tools::Config::default() };
    let handler = otto_silo_tools::handler(move |request| {
        let settings = settings.clone();
        async move { settings.call(request).await }
    });
    tokio::spawn(otto_silo_tools::provide(config, tools(), handler));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_is_valid_and_changes_need_approval() {
        let tools = tools();
        assert_eq!(tools.len(), TOOLS.len(), "every method is described");
        for tool in &tools {
            tool.validate().unwrap();
            assert_eq!(tool.approval, !tool.read_only, "{}", tool.name);
        }
        let save = tools.iter().find(|t| t.name == "otto_agent_save").unwrap();
        assert!(save.input_schema["properties"]["agent"].is_object(), "{}", save.input_schema);
    }

    #[test]
    fn inputs_become_methods() {
        for (name, method, ..) in TOOLS {
            if *name == "otto_agents_list" || *name == "otto_mcp_list" || *name == "otto_config_get" {
                let parsed: Result<otto_proto::Method, _> = serde_json::from_value(json!({"method": method, "params": {}}));
                assert!(parsed.is_ok(), "{name}: {parsed:?}");
            }
        }
    }

    #[test]
    fn the_callback_url_is_the_registered_port() {
        let outcome = |input: Value| {
            let request = ToolRequest {
                request_id: "r".into(),
                name: "otto_mcp_callback_url".into(),
                block_id: None,
                input,
                caller: serde_json::from_value(json!({"client_id": 1, "label": "t"})).unwrap(),
            };
            tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(Settings::new("/nonexistent".into()).call(request))
        };
        assert_eq!(outcome(json!({})), ToolOutcome::json(json!({"callback_url": "http://127.0.0.1:33418/callback"})));
        assert_eq!(outcome(json!({"callback_port": 8765})), ToolOutcome::json(json!({"callback_url": "http://127.0.0.1:8765/callback"})));
    }

    #[test]
    fn secrets_are_redacted() {
        let value = json!({"servers": [{"transport": {"url": "u", "headers": {"Authorization": "Bearer x"}, "env": {"A": "b"}}}], "apiKey": "k", "enabled": true});
        let clean = redact(value);
        assert_eq!(clean["servers"][0]["transport"]["headers"]["Authorization"], "[REDACTED]");
        assert_eq!(clean["servers"][0]["transport"]["env"]["A"], "[REDACTED]");
        assert_eq!(clean["apiKey"], "[REDACTED]");
        assert_eq!(clean["enabled"], true);
        assert_eq!(clean["servers"][0]["transport"]["url"], "u");
    }
}
