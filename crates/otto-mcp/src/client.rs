//! A minimal MCP client: JSON-RPC 2.0 over line-delimited stdio or
//! streamable HTTP (POST with JSON or SSE responses, `Mcp-Session-Id`).
use crate::config::ServerConfig;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;

pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug)]
pub enum CallError {
    Timeout,
    Cancelled,
    Closed(String),
    Rpc { code: i64, message: String },
}
impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "timed out"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Closed(why) => write!(f, "server connection closed: {why}"),
            Self::Rpc { code, message } => write!(f, "server error {code}: {message}"),
        }
    }
}

type Reply = Result<Value, CallError>;

struct Http {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    session: Mutex<Option<String>>,
    initialized: AtomicBool,
}

enum Transport {
    Stdio { stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>, child: tokio::sync::Mutex<tokio::process::Child> },
    Http(Http),
}

pub struct Client {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    alive: AtomicBool,
    /// Bumped by `notifications/tools/list_changed`.
    pub changed: AtomicU64,
    transport: Transport,
    name: String,
}

impl Client {
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn close(&self, why: &str) {
        self.alive.store(false, Ordering::SeqCst);
        for (_, tx) in self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drain() {
            let _ = tx.send(Err(CallError::Closed(why.into())));
        }
    }

    /// Route one incoming message; returns the reply to a server request.
    fn dispatch(&self, message: Value) -> Option<Value> {
        if let Some(method) = message["method"].as_str() {
            let id = message.get("id")?.clone();
            return Some(if method == "ping" {
                json!({"jsonrpc":"2.0","id":id,"result":{}})
            } else {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not supported by otto"}})
            });
        }
        if message["method"].is_null() && message.get("id").is_none() {
            return None;
        }
        let id = message["id"].as_u64()?;
        let tx = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id)?;
        let _ = tx.send(match message.get("error") {
            Some(error) => {
                Err(CallError::Rpc { code: error["code"].as_i64().unwrap_or(0), message: error["message"].as_str().unwrap_or("unknown").into() })
            }
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        });
        None
    }

    fn notification(&self, message: &Value) {
        if message.get("id").is_none() && message["method"] == "notifications/tools/list_changed" {
            self.changed.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn write_line(&self, message: &Value) -> Result<(), CallError> {
        let Transport::Stdio { stdin, .. } = &self.transport else { return Err(CallError::Closed("not a stdio server".into())) };
        let mut stdin = stdin.lock().await;
        let stdin = stdin.as_mut().ok_or_else(|| CallError::Closed("stdin closed".into()))?;
        stdin.write_all(format!("{message}\n").as_bytes()).await.map_err(|e| CallError::Closed(e.to_string()))?;
        stdin.flush().await.map_err(|e| CallError::Closed(e.to_string()))
    }

    async fn send(self: &Arc<Self>, message: Value) -> Result<(), CallError> {
        match &self.transport {
            Transport::Stdio { .. } => self.write_line(&message).await,
            Transport::Http(_) => {
                let client = self.clone();
                // The response (JSON or an SSE stream) is read in the background;
                // it resolves the pending request by id.
                let id = message["id"].as_u64();
                tokio::spawn(async move {
                    let Transport::Http(http) = &client.transport else { return };
                    if let Err(error) = client.post(http, message).await
                        && let Some(id) = id
                        && let Some(tx) = client.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id)
                    {
                        let _ = tx.send(Err(error));
                    }
                });
                Ok(())
            }
        }
    }

    fn request_builder(http: &Http, method: reqwest::Method) -> reqwest::RequestBuilder {
        let mut request = http.client.request(method, &http.url).header("Accept", "application/json, text/event-stream");
        for (name, value) in &http.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(session) = http.session.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() {
            request = request.header("Mcp-Session-Id", session);
        }
        if http.initialized.load(Ordering::SeqCst) {
            request = request.header("MCP-Protocol-Version", PROTOCOL_VERSION);
        }
        request
    }

    async fn post(self: &Arc<Self>, http: &Http, message: Value) -> Result<(), CallError> {
        let had_session = http.session.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some();
        let response = Self::request_builder(http, reqwest::Method::POST)
            .header("Content-Type", "application/json")
            .body(message.to_string())
            .send()
            .await
            .map_err(|e| CallError::Closed(e.to_string()))?;
        if let Some(session) = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *http.session.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session.to_owned());
        }
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND && had_session {
            self.close("session expired");
            return Err(CallError::Closed("session expired".into()));
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CallError::Closed(format!("HTTP {status}: {}", body.chars().take(300).collect::<String>())));
        }
        if status == reqwest::StatusCode::ACCEPTED {
            return Ok(());
        }
        let sse = response.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"));
        if !sse {
            let body = response.bytes().await.map_err(|e| CallError::Closed(e.to_string()))?;
            if body.is_empty() {
                return Ok(());
            }
            let value: Value = serde_json::from_slice(&body).map_err(|e| CallError::Closed(format!("invalid JSON from server: {e}")))?;
            for message in value.as_array().cloned().unwrap_or_else(|| vec![value]) {
                self.incoming(message).await;
            }
            return Ok(());
        }
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk.map_err(|e| CallError::Closed(e.to_string()))?).replace("\r\n", "\n"));
            while let Some(end) = buffer.find("\n\n") {
                let event: String = buffer.drain(..end + 2).collect();
                let data: Vec<&str> = event.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim_start).collect();
                if let Ok(message) = serde_json::from_str::<Value>(&data.join("\n")) {
                    self.incoming(message).await;
                }
            }
        }
        Ok(())
    }

    async fn incoming(self: &Arc<Self>, message: Value) {
        self.notification(&message);
        if let Some(reply) = self.dispatch(message) {
            match &self.transport {
                Transport::Http(http) => {
                    let _ = Self::request_builder(http, reqwest::Method::POST)
                        .header("Content-Type", "application/json")
                        .body(reply.to_string())
                        .send()
                        .await;
                }
                Transport::Stdio { .. } => {
                    let _ = self.write_line(&reply).await;
                }
            }
        }
    }

    /// One request; on timeout or cancel the server gets `notifications/cancelled`.
    pub async fn request(self: &Arc<Self>, method: &str, params: Value, timeout: Duration, cancel: &CancellationToken) -> Reply {
        if !self.alive() {
            return Err(CallError::Closed("not running".into()));
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id, tx);
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await?;
        let outcome = tokio::select! {
            reply = rx => return reply.unwrap_or_else(|_| Err(CallError::Closed("no reply".into()))),
            _ = tokio::time::sleep(timeout) => CallError::Timeout,
            _ = cancel.cancelled() => CallError::Cancelled,
        };
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
        let _ = self.notify("notifications/cancelled", json!({"requestId":id,"reason":outcome.to_string()})).await;
        Err(outcome)
    }

    pub async fn notify(self: &Arc<Self>, method: &str, params: Value) -> Result<(), CallError> {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params})).await
    }

    /// Stop the server process or end the HTTP session.
    pub async fn shutdown(&self) {
        self.close("shut down");
        match &self.transport {
            Transport::Stdio { stdin, child } => {
                stdin.lock().await.take();
                let mut child = child.lock().await;
                if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
                    let _ = child.kill().await;
                }
            }
            Transport::Http(http) => {
                if http.session.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                    let _ = tokio::time::timeout(Duration::from_secs(2), Self::request_builder(http, reqwest::Method::DELETE).send()).await;
                }
            }
        }
    }
}

fn new(transport: Transport, name: &str) -> Arc<Client> {
    Arc::new(Client {
        next: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        alive: AtomicBool::new(true),
        changed: AtomicU64::new(0),
        transport,
        name: name.into(),
    })
}

fn stdio(name: &str, config: &ServerConfig) -> Result<Arc<Client>, String> {
    let command = config.command.as_deref().ok_or("no command configured")?;
    let mut process = tokio::process::Command::new(command);
    process
        .args(&config.args)
        .envs(&config.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &config.cwd {
        process.current_dir(cwd);
    }
    let mut child = process.spawn().map_err(|e| format!("cannot start {command}: {e}"))?;
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let client = new(Transport::Stdio { stdin: tokio::sync::Mutex::new(stdin), child: tokio::sync::Mutex::new(child) }, name);
    let weak: Weak<Client> = Arc::downgrade(&client);
    if let Some(stdout) = stdout {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(client) = weak.upgrade() else { return };
                match serde_json::from_str::<Value>(&line) {
                    Ok(message) => client.incoming(message).await,
                    Err(_) => tracing::debug!(server=%client.name, line, "non-JSON output from MCP server"),
                }
            }
            if let Some(client) = weak.upgrade() {
                client.close("process exited");
            }
        });
    }
    if let Some(stderr) = stderr {
        let name = name.to_owned();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(server=%name, line, "MCP server stderr");
            }
        });
    }
    Ok(client)
}

/// Start (or open) the server and run the initialize handshake.
pub async fn connect(name: &str, config: &ServerConfig) -> Result<Arc<Client>, String> {
    let client = if config.is_http() {
        let url = config.url.clone().ok_or("no url configured")?;
        new(
            Transport::Http(Http {
                client: reqwest::Client::new(),
                url,
                headers: config.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                session: Mutex::new(None),
                initialized: AtomicBool::new(false),
            }),
            name,
        )
    } else if config.kind.as_deref().is_none_or(|k| k == "stdio") {
        stdio(name, config)?
    } else {
        return Err(format!("unsupported transport {}", config.kind.as_deref().unwrap_or_default()));
    };
    let timeout = Duration::from_millis(config.startup_timeout.unwrap_or(30_000));
    let init = client
        .request(
            "initialize",
            json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{},"clientInfo":{"name":"otto","version":env!("CARGO_PKG_VERSION")}}),
            timeout,
            &CancellationToken::new(),
        )
        .await;
    if let Err(error) = init {
        client.shutdown().await;
        return Err(format!("initialize failed: {error}"));
    }
    if let Transport::Http(http) = &client.transport {
        http.initialized.store(true, Ordering::SeqCst);
    }
    client.notify("notifications/initialized", json!({})).await.map_err(|e| e.to_string())?;
    Ok(client)
}
