//! One configured server: lazy start, tool list cache invalidated by `list_changed`
//! (and relisted right away), restart after a crash (bounded), its status, and its
//! tools as otto tools.
use crate::{
    Event, Shared,
    client::{CallError, Client, ClientEvent, Connect, Hook, Roots, connect, lock},
    config::ServerConfig,
    extra, schema,
};
use async_trait::async_trait;
use otto_tools::{Effects, Loading, OutputPart, Source, Tool, ToolCx, ToolError, ToolErrorKind, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Weak},
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

/// Where a server is, as settings show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not started yet (servers start when a session or `Mcp::list` needs them).
    Idle,
    Connecting,
    Connected,
    /// An HTTP server that wants an OAuth sign-in (`Mcp::login`).
    NeedsSignIn,
    Error(String),
}

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
    /// What `status` reports: readable while a start holds `state`.
    snapshot: std::sync::Mutex<(Status, usize)>,
    roots: Roots,
    shared: Arc<Shared>,
    me: Weak<Server>,
}

impl Server {
    pub(crate) fn new(name: String, config: ServerConfig, shared: Arc<Shared>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            name,
            config,
            state: tokio::sync::Mutex::new(State::default()),
            snapshot: std::sync::Mutex::new((Status::Idle, 0)),
            roots: Roots::default(),
            shared,
            me: me.clone(),
        })
    }

    /// The status and the number of the server's own tools.
    pub fn status(&self) -> (Status, usize) {
        lock(&self.snapshot).clone()
    }

    fn set(&self, status: Option<Status>, tools: Option<usize>) {
        let mut snapshot = lock(&self.snapshot);
        let next = (status.unwrap_or_else(|| snapshot.0.clone()), tools.unwrap_or(snapshot.1));
        if *snapshot != next {
            *snapshot = next.clone();
            drop(snapshot);
            // Stopping is not news: the next start reports `Connecting`.
            if next.0 != Status::Idle {
                let _ = self.shared.events.send(Event::Status { name: self.name.clone(), status: next.0, tools: next.1 });
            }
        }
    }

    /// A project the server serves (its `roots/list`); a running server hears of a new one.
    pub(crate) fn add_root(&self, root: &Path) {
        if !lock(&self.roots).insert(root.to_path_buf()) {
            return;
        }
        if let Some(me) = self.me.upgrade() {
            tokio::spawn(async move {
                let client = me.state.lock().await.client.clone().filter(|c| c.alive());
                if let Some(client) = client {
                    let _ = client.notify("notifications/roots/list_changed", json!({})).await;
                }
            });
        }
    }

    /// Start the server in the background unless it is running or failed recently.
    pub(crate) fn start(&self) {
        if lock(&self.snapshot).0 == Status::Idle
            && let Some(me) = self.me.upgrade()
        {
            tokio::spawn(async move {
                let _ = me.tools().await;
            });
        }
    }

    fn hook(&self) -> Hook {
        let me = self.me.clone();
        Box::new(move |event| {
            let Some(server) = me.upgrade() else { return };
            match event {
                ClientEvent::ListChanged => {
                    tokio::spawn(async move {
                        let _ = server.tools().await;
                    });
                }
                ClientEvent::Closed(why) => server.set(Some(Status::Error(format!("disconnected: {why}"))), None),
            }
        })
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
            let error = format!("MCP server {} stopped {STARTS_PER_MINUTE} times within a minute", self.name);
            self.set(Some(Status::Error(error.clone())), None);
            return Err(error);
        }
        state.starts.push(now);
        self.set(Some(Status::Connecting), None);
        let cx = Connect { tokens: self.shared.tokens.clone(), roots: self.roots.clone(), hook: self.hook() };
        match connect(&self.name, &self.config, cx).await {
            Ok(client) => {
                state.failed = None;
                state.client = Some(client.clone());
                self.set(Some(Status::Connected), None);
                Ok(client)
            }
            Err(CallError::NeedsAuth) => {
                let error = format!("MCP server {} needs sign-in", self.name);
                state.failed = Some((now, error.clone()));
                self.set(Some(Status::NeedsSignIn), Some(0));
                Err(error)
            }
            Err(error) => {
                let message = match error {
                    CallError::Closed(why) => why,
                    error => format!("initialize failed: {error}"),
                };
                state.failed = Some((now, format!("MCP server {}: {message}", self.name)));
                self.set(Some(Status::Error(message.clone())), Some(0));
                Err(format!("MCP server {}: {message}", self.name))
            }
        }
    }

    fn startup_timeout(&self) -> Duration {
        Duration::from_millis(self.config.startup_timeout.unwrap_or(30_000))
    }

    /// Every page of a `*/list` method's `key` array (at most 2000 entries).
    pub(crate) async fn list(&self, client: &Arc<Client>, method: &str, key: &str) -> Result<Vec<Value>, String> {
        let mut listed = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map_or_else(|| json!({}), |c| json!({"cursor":c}));
            let result = client
                .request(method, params, self.startup_timeout(), &CancellationToken::new())
                .await
                .map_err(|e| format!("MCP server {}: {e}", self.name))?;
            listed.extend(result[key].as_array().cloned().unwrap_or_default());
            cursor = result["nextCursor"].as_str().map(String::from);
            if cursor.is_none() || listed.len() > 2000 {
                return Ok(listed);
            }
        }
    }

    /// The server's tools (and its resources and prompts as tools), starting it if
    /// needed; relisted after `list_changed`.
    pub async fn tools(self: &Arc<Self>) -> Result<Vec<Arc<dyn Tool>>, String> {
        let mut state = self.state.lock().await;
        let client = self.client(&mut state).await?;
        let version = client.changed.load(std::sync::atomic::Ordering::SeqCst);
        if let Some((listed, tools)) = &state.tools
            && *listed == version
        {
            return Ok(tools.clone());
        }
        let listed = match self.list(&client, "tools/list", "tools").await {
            Ok(listed) => listed,
            Err(error) => {
                self.set(Some(Status::Error(error.clone())), None);
                return Err(error);
            }
        };
        let deferred = self.config.deferred.unwrap_or(listed.len() > DEFER_ABOVE);
        let mut tools: Vec<Arc<dyn Tool>> =
            listed.iter().filter_map(|t| McpTool::new(self.clone(), t, deferred)).map(|t| Arc::new(t) as Arc<dyn Tool>).collect();
        let own = tools.len();
        tools.extend(extra::tools(self, &client, deferred, &tools).await);
        state.tools = Some((version, tools.clone()));
        self.set(Some(Status::Connected), Some(own));
        Ok(tools)
    }

    /// One request to the running (or started) server, as a tool sees it.
    pub(crate) async fn request(&self, method: &str, params: Value, cancel: &CancellationToken) -> Result<Value, ToolError> {
        let client = {
            let mut state = self.state.lock().await;
            self.client(&mut state).await.map_err(ToolError::failed)?
        };
        let timeout = Duration::from_millis(self.config.timeout.unwrap_or(600_000));
        client.request(method, params, timeout, cancel).await.map_err(|error| match error {
            CallError::Cancelled => ToolError::new(ToolErrorKind::Cancelled, "cancelled; the MCP server was asked to stop"),
            CallError::Timeout => {
                ToolError::new(ToolErrorKind::Timeout, format!("no answer from MCP server {} within {}s", self.name, timeout.as_secs()))
            }
            CallError::NeedsAuth => ToolError::failed(format!("MCP server {} needs sign-in (in otto's MCP settings)", self.name)),
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
        self.set(Some(Status::Idle), Some(0));
    }

    /// Forget failures (after a sign-in or sign-out) and reconnect.
    pub(crate) async fn restart(&self) {
        self.shutdown().await;
        let mut state = self.state.lock().await;
        state.failed = None;
        state.starts.clear();
        drop(state);
        self.start();
    }
}

/// An otto tool spec for something `server` provides.
pub(crate) fn spec(server: &Server, remote: &str, description: &str, input_schema: Value, effects: Effects, deferred: bool) -> ToolSpec {
    let mut description: String = description.chars().take(DESCRIPTION_LIMIT).collect();
    description.push_str(&format!(" (MCP server {})", server.name));
    ToolSpec {
        name: schema::tool_name(&server.name, remote),
        description,
        input_schema,
        output_schema: None,
        effects,
        sensitive: vec![],
        loading: if deferred { Loading::Deferred } else { Loading::Eager },
        // Remote and often slow: models with async tool calling keep working meanwhile.
        mode: otto_tools::CallMode::Async,
        presentations: vec![],
        source: Source::Mcp { server: server.name.clone() },
        exclusive: false,
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
        Some(Self { spec: spec(&server, &remote, description, schema::sanitize(&tool["inputSchema"]), effects, deferred), server, remote })
    }
}

/// MCP content blocks as tool output parts.
pub fn output(result: &Value) -> ToolOutput {
    let mut content: Vec<OutputPart> = result["content"].as_array().into_iter().flatten().map(content_part).collect();
    if content.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        content.push(OutputPart::Json { value: structured.clone() });
    }
    ToolOutput { content, is_error: result["isError"] == true, metadata: Value::Null, truncated: None }
}

/// One content block (also a prompt message's content).
pub(crate) fn content_part(part: &Value) -> OutputPart {
    match part["type"].as_str() {
        Some("text") => OutputPart::Text { text: part["text"].as_str().unwrap_or_default().into() },
        Some("image") => OutputPart::Image {
            mime: part["mimeType"].as_str().unwrap_or("image/png").into(),
            data_base64: part["data"].as_str().unwrap_or_default().into(),
        },
        Some("resource") => resource_part(&part["resource"]),
        Some("resource_link") => OutputPart::Text {
            text: format!("Resource link: {} {}", part["uri"].as_str().unwrap_or_default(), part["name"].as_str().unwrap_or_default()),
        },
        Some(other) => OutputPart::Text { text: format!("[{other} content omitted]") },
        None => OutputPart::Json { value: part.clone() },
    }
}

/// A resource's contents: text, an image, or a note that binary data was left out.
pub(crate) fn resource_part(resource: &Value) -> OutputPart {
    let uri = resource["uri"].as_str().unwrap_or_default();
    let mime = resource["mimeType"].as_str().unwrap_or_default();
    match (resource["text"].as_str(), resource["blob"].as_str()) {
        (Some(text), _) => OutputPart::Text { text: format!("Resource {uri}:\n{text}") },
        (None, Some(blob)) if mime.starts_with("image/") => OutputPart::Image { mime: mime.into(), data_base64: blob.into() },
        _ => OutputPart::Text { text: format!("[binary resource {uri} omitted]") },
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let arguments = if input.is_object() { input } else { json!({}) };
        Ok(output(&self.server.request("tools/call", json!({"name":self.remote,"arguments":arguments}), &cx.cancel).await?))
    }
}
