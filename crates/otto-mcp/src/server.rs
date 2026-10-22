//! One configured server: lazy start, tool list cache invalidated by
//! `list_changed`, restart after a crash (bounded), and its tools as otto tools.
use crate::{
    client::{CallError, Client, connect},
    config::ServerConfig,
    schema,
};
use async_trait::async_trait;
use otto_tools::{Effects, Loading, OutputPart, Source, Tool, ToolCx, ToolError, ToolErrorKind, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// More tools than this and the server's tools load through `load_tools`.
pub const DEFER_ABOVE: usize = 10;
/// Starts allowed per minute before the server counts as broken.
const STARTS_PER_MINUTE: usize = 3;
/// After a failed start, calls fail fast for this long.
const BACKOFF: Duration = Duration::from_secs(30);
const DESCRIPTION_LIMIT: usize = 2048;

#[derive(Default)]
struct State {
    client: Option<Arc<Client>>,
    /// Tools listed at a `list_changed` count.
    tools: Option<(u64, Vec<Arc<dyn Tool>>)>,
    starts: Vec<Instant>,
    failed: Option<(Instant, String)>,
}

pub struct Server {
    pub name: String,
    pub config: ServerConfig,
    state: tokio::sync::Mutex<State>,
}

impl Server {
    pub fn new(name: String, config: ServerConfig) -> Self {
        Self { name, config, state: tokio::sync::Mutex::new(State::default()) }
    }

    async fn client(&self, state: &mut State) -> Result<Arc<Client>, String> {
        if let Some(client) = state.client.as_ref().filter(|c| c.alive()) {
            return Ok(client.clone());
        }
        if let Some(dead) = state.client.take() {
            dead.shutdown().await;
            state.tools = None;
        }
        let now = Instant::now();
        if let Some((at, error)) = &state.failed
            && now.duration_since(*at) < BACKOFF
        {
            return Err(error.clone());
        }
        state.starts.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if state.starts.len() >= STARTS_PER_MINUTE {
            return Err(format!("MCP server {} stopped {STARTS_PER_MINUTE} times within a minute", self.name));
        }
        state.starts.push(now);
        match connect(&self.name, &self.config).await {
            Ok(client) => {
                state.failed = None;
                state.client = Some(client.clone());
                Ok(client)
            }
            Err(error) => {
                let error = format!("MCP server {}: {error}", self.name);
                state.failed = Some((now, error.clone()));
                Err(error)
            }
        }
    }

    fn startup_timeout(&self) -> Duration {
        Duration::from_millis(self.config.startup_timeout.unwrap_or(30_000))
    }

    /// The server's tools, starting it if needed; relisted after `list_changed`.
    pub async fn tools(self: &Arc<Self>) -> Result<Vec<Arc<dyn Tool>>, String> {
        let mut state = self.state.lock().await;
        let client = self.client(&mut state).await?;
        let version = client.changed.load(std::sync::atomic::Ordering::SeqCst);
        if let Some((listed, tools)) = &state.tools
            && *listed == version
        {
            return Ok(tools.clone());
        }
        let mut listed = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map_or_else(|| json!({}), |c| json!({"cursor":c}));
            let result = client
                .request("tools/list", params, self.startup_timeout(), &CancellationToken::new())
                .await
                .map_err(|e| format!("MCP server {}: {e}", self.name))?;
            listed.extend(result["tools"].as_array().cloned().unwrap_or_default());
            cursor = result["nextCursor"].as_str().map(String::from);
            if cursor.is_none() || listed.len() > 2000 {
                break;
            }
        }
        let deferred = self.config.deferred.unwrap_or(listed.len() > DEFER_ABOVE);
        let tools: Vec<Arc<dyn Tool>> =
            listed.iter().filter_map(|t| McpTool::new(self.clone(), t, deferred)).map(|t| Arc::new(t) as Arc<dyn Tool>).collect();
        state.tools = Some((version, tools.clone()));
        Ok(tools)
    }

    async fn call(&self, tool: &str, arguments: Value, cancel: &CancellationToken) -> Result<Value, ToolError> {
        let client = {
            let mut state = self.state.lock().await;
            self.client(&mut state).await.map_err(ToolError::failed)?
        };
        let timeout = Duration::from_millis(self.config.timeout.unwrap_or(600_000));
        client.request("tools/call", json!({"name":tool,"arguments":arguments}), timeout, cancel).await.map_err(|error| match error {
            CallError::Cancelled => ToolError::new(ToolErrorKind::Cancelled, "cancelled; the MCP server was asked to stop"),
            CallError::Timeout => {
                ToolError::new(ToolErrorKind::Timeout, format!("no answer from MCP server {} within {}s", self.name, timeout.as_secs()))
            }
            error => ToolError::failed(format!("MCP server {}: {error}", self.name)),
        })
    }

    pub async fn shutdown(&self) {
        let client = {
            let mut state = self.state.lock().await;
            // Cached tools hold this server; dropping them ends the cycle.
            state.tools = None;
            state.client.take()
        };
        if let Some(client) = client {
            client.shutdown().await;
        }
    }

    /// `running`, `stopped`, or the last start error.
    pub async fn status(&self) -> (String, usize) {
        let state = self.state.lock().await;
        let tools = state.tools.as_ref().map_or(0, |t| t.1.len());
        match (&state.client, &state.failed) {
            (Some(client), _) if client.alive() => ("running".into(), tools),
            (_, Some((_, error))) => (error.clone(), tools),
            _ => ("stopped".into(), tools),
        }
    }
}

struct McpTool {
    spec: ToolSpec,
    server: Arc<Server>,
    remote: String,
}

impl McpTool {
    fn new(server: Arc<Server>, tool: &Value, deferred: bool) -> Option<Self> {
        let remote = tool["name"].as_str()?.to_owned();
        let annotations = &tool["annotations"];
        let network = server.config.is_http();
        // Unannotated tools may change things outside this machine.
        let effects = if annotations["readOnlyHint"] == true {
            Effects { read: true, network, ..Effects::NONE }
        } else {
            Effects { read: true, write: annotations["destructiveHint"] != false, external: true, network, ..Effects::NONE }
        };
        let description =
            tool["description"].as_str().or_else(|| annotations["title"].as_str()).or_else(|| tool["title"].as_str()).unwrap_or(&remote);
        let mut description: String = description.chars().take(DESCRIPTION_LIMIT).collect();
        description.push_str(&format!(" (MCP server {})", server.name));
        Some(Self {
            spec: ToolSpec {
                name: schema::tool_name(&server.name, &remote),
                description,
                input_schema: schema::sanitize(&tool["inputSchema"]),
                output_schema: None,
                effects,
                sensitive: vec![],
                loading: if deferred { Loading::Deferred } else { Loading::Eager },
                // Remote and often slow: models with async tool calling keep working meanwhile.
                mode: otto_tools::CallMode::Async,
                presentations: vec![],
                source: Source::Mcp { server: server.name.clone() },
                exclusive: false,
            },
            server,
            remote,
        })
    }
}

/// MCP content blocks as tool output parts.
pub fn output(result: &Value) -> ToolOutput {
    let mut content: Vec<OutputPart> = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|part| match part["type"].as_str() {
            Some("text") => OutputPart::Text { text: part["text"].as_str().unwrap_or_default().into() },
            Some("image") => OutputPart::Image {
                mime: part["mimeType"].as_str().unwrap_or("image/png").into(),
                data_base64: part["data"].as_str().unwrap_or_default().into(),
            },
            Some("resource") => {
                let resource = &part["resource"];
                let uri = resource["uri"].as_str().unwrap_or_default();
                match resource["text"].as_str() {
                    Some(text) => OutputPart::Text { text: format!("Resource {uri}:\n{text}") },
                    None => OutputPart::Text { text: format!("[binary resource {uri} omitted]") },
                }
            }
            Some("resource_link") => OutputPart::Text {
                text: format!("Resource link: {} {}", part["uri"].as_str().unwrap_or_default(), part["name"].as_str().unwrap_or_default()),
            },
            Some(other) => OutputPart::Text { text: format!("[{other} content omitted]") },
            None => OutputPart::Json { value: part.clone() },
        })
        .collect();
    if content.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        content.push(OutputPart::Json { value: structured.clone() });
    }
    ToolOutput { content, is_error: result["isError"] == true, metadata: Value::Null, truncated: None }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let arguments = if input.is_object() { input } else { json!({}) };
        Ok(output(&self.server.call(&self.remote, arguments, &cx.cancel).await?))
    }
}
