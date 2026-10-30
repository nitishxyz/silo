//! A minimal MCP client: JSON-RPC 2.0 over line-delimited stdio ([`crate::stdio`]) or
//! streamable HTTP ([`crate::http`]: POST with JSON or SSE responses, `Mcp-Session-Id`,
//! the GET notification stream, OAuth).
use crate::{config::ServerConfig, http::Http, oauth::TokenStore};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug)]
pub enum CallError {
    Timeout,
    Cancelled,
    Closed(String),
    Rpc {
        code: i64,
        message: String,
    },
    /// The HTTP server wants an OAuth sign-in (a `401` without a usable token).
    NeedsAuth,
}
impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "timed out"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Closed(why) => write!(f, "server connection closed: {why}"),
            Self::Rpc { code, message } => write!(f, "server error {code}: {message}"),
            Self::NeedsAuth => write!(f, "needs sign-in"),
        }
    }
}

pub(crate) type Reply = Result<Value, CallError>;

/// What the owning server hears about its connection.
pub enum ClientEvent {
    /// A `notifications/{tools,resources,prompts}/list_changed`.
    ListChanged,
    /// The connection ended unexpectedly.
    Closed(String),
}

pub type Hook = Box<dyn Fn(ClientEvent) + Send + Sync>;

pub(crate) enum Transport {
    Stdio { stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>, child: tokio::sync::Mutex<tokio::process::Child> },
    Http(Http),
}

/// Shared with the server: the folders it may work in (`roots/list`).
pub type Roots = Arc<Mutex<BTreeSet<PathBuf>>>;

pub struct Client {
    next: AtomicU64,
    pub(crate) pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    alive: AtomicBool,
    /// Bumped by every `list_changed` notification.
    pub changed: AtomicU64,
    pub(crate) transport: Transport,
    pub(crate) name: String,
    roots: Roots,
    hook: Hook,
    /// The server's `initialize` result `capabilities`.
    pub capabilities: Mutex<Value>,
    /// Ends background readers (the HTTP notification stream).
    pub(crate) stop: CancellationToken,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Client {
    pub(crate) fn new(transport: Transport, name: &str, roots: Roots, hook: Hook) -> Arc<Self> {
        Arc::new(Self {
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
            changed: AtomicU64::new(0),
            transport,
            name: name.into(),
            roots,
            hook,
            capabilities: Mutex::new(Value::Null),
            stop: CancellationToken::new(),
        })
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub(crate) fn close(&self, why: &str) {
        self.stop.cancel();
        if self.alive.swap(false, Ordering::SeqCst) && why != "shut down" {
            (self.hook)(ClientEvent::Closed(why.into()));
        }
        for (_, tx) in lock(&self.pending).drain() {
            let _ = tx.send(Err(CallError::Closed(why.into())));
        }
    }

    /// A server request's reply: `ping`, `roots/list`; sampling and elicitation are declined.
    fn answer(&self, method: &str, id: Value) -> Value {
        let result = match method {
            "ping" => json!({}),
            "roots/list" => {
                let roots: Vec<Value> = lock(&self.roots)
                    .iter()
                    .map(|root| {
                        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        json!({"uri":reqwest::Url::from_file_path(root).map(String::from).unwrap_or_default(),"name":name})
                    })
                    .collect();
                json!({"roots":roots})
            }
            "elicitation/create" => json!({"action":"decline"}),
            "sampling/createMessage" => {
                return json!({"jsonrpc":"2.0","id":id,"error":{"code":-1,"message":"otto does not let MCP servers use its model (sampling is not supported)"}});
            }
            _ => return json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("{method} is not supported by otto")}}),
        };
        json!({"jsonrpc":"2.0","id":id,"result":result})
    }

    /// Route one incoming message; returns the reply to a server request.
    fn dispatch(&self, message: Value) -> Option<Value> {
        if let Some(method) = message["method"].as_str() {
            let Some(id) = message.get("id") else {
                self.notification(method, &message["params"]);
                return None;
            };
            return Some(self.answer(method, id.clone()));
        }
        let id = message["id"].as_u64()?;
        let tx = lock(&self.pending).remove(&id)?;
        let _ = tx.send(match message.get("error") {
            Some(error) => {
                Err(CallError::Rpc { code: error["code"].as_i64().unwrap_or(0), message: error["message"].as_str().unwrap_or("unknown").into() })
            }
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        });
        None
    }

    fn notification(&self, method: &str, params: &Value) {
        match method {
            "notifications/tools/list_changed" | "notifications/resources/list_changed" | "notifications/prompts/list_changed" => {
                self.changed.fetch_add(1, Ordering::SeqCst);
                (self.hook)(ClientEvent::ListChanged);
            }
            "notifications/message" => tracing::debug!(server=%self.name, %params, "MCP server log"),
            _ => {}
        }
    }

    /// Boxed: a reply's POST may carry server messages back here.
    pub(crate) fn incoming(self: &Arc<Self>, message: Value) -> futures::future::BoxFuture<'static, ()> {
        let client = self.clone();
        Box::pin(async move {
            if let Some(reply) = client.dispatch(message) {
                let _ = client.send(reply).await;
            }
        })
    }

    async fn send(self: &Arc<Self>, message: Value) -> Result<(), CallError> {
        match &self.transport {
            Transport::Stdio { .. } => self.write_line(&message).await,
            Transport::Http(_) => {
                let client = self.clone();
                // The response (JSON or an SSE stream) is read in the background;
                // it resolves the pending request by id.
                let id = message["id"].as_u64().filter(|_| message.get("method").is_some());
                tokio::spawn(async move {
                    if let Err(error) = client.post(message).await
                        && let Some(id) = id
                        && let Some(tx) = lock(&client.pending).remove(&id)
                    {
                        let _ = tx.send(Err(error));
                    }
                });
                Ok(())
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
        lock(&self.pending).insert(id, tx);
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await?;
        let outcome = tokio::select! {
            reply = rx => return reply.unwrap_or_else(|_| Err(CallError::Closed("no reply".into()))),
            _ = tokio::time::sleep(timeout) => CallError::Timeout,
            _ = cancel.cancelled() => CallError::Cancelled,
        };
        lock(&self.pending).remove(&id);
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
            Transport::Http(http) => http.end().await,
        }
    }
}

/// How a connection reaches the rest of otto.
pub struct Connect {
    pub tokens: Arc<TokenStore>,
    pub roots: Roots,
    pub hook: Hook,
}

/// Start (or open) the server and run the initialize handshake.
pub async fn connect(name: &str, config: &ServerConfig, cx: Connect) -> Result<Arc<Client>, CallError> {
    let client = if config.is_http() {
        let url = config.url.clone().ok_or_else(|| CallError::Closed("no url configured".into()))?;
        Client::new(Transport::Http(Http::new(url, &config.headers, cx.tokens)), name, cx.roots, cx.hook)
    } else if config.kind.as_deref().is_none_or(|k| k == "stdio") {
        crate::stdio::spawn(name, config, cx.roots, cx.hook).map_err(CallError::Closed)?
    } else {
        return Err(CallError::Closed(format!("unsupported transport {}", config.kind.as_deref().unwrap_or_default())));
    };
    let timeout = Duration::from_millis(config.startup_timeout.unwrap_or(30_000));
    let capabilities = json!({"roots":{"listChanged":true}});
    let params =
        json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":capabilities,"clientInfo":{"name":"otto","version":env!("CARGO_PKG_VERSION")}});
    match client.request("initialize", params, timeout, &CancellationToken::new()).await {
        Ok(result) => *lock(&client.capabilities) = result["capabilities"].clone(),
        Err(error) => {
            client.shutdown().await;
            return Err(error);
        }
    }
    if let Transport::Http(http) = &client.transport {
        http.initialized.store(true, Ordering::SeqCst);
    }
    client.notify("notifications/initialized", json!({})).await?;
    if matches!(client.transport, Transport::Http(_)) {
        tokio::spawn(crate::http::listen(Arc::downgrade(&client)));
    }
    Ok(client)
}
