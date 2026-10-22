use otto_proto::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::{broadcast, mpsc, oneshot, watch},
};

pub type Result<T> = std::result::Result<T, ClientError>;
pub type Events = broadcast::Receiver<Result<ServerMessage>>;

#[derive(Clone, Debug, thiserror::Error)]
pub enum ClientError {
    #[error("I/O: {message}")]
    Io { kind: std::io::ErrorKind, message: String },
    #[error("invalid JSON: {0}")]
    Json(String),
    #[error("incompatible protocol: expected {expected}, got {got:?}")]
    Incompatible { expected: u32, got: Option<u32> },
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("connection closed")]
    Closed,
    #[error("operation timed out")]
    Timeout,
    #[error("no reconnect transport configured")]
    NoReconnect,
}
impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        Self::Io { kind: e.kind(), message: e.to_string() }
    }
}
impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}

type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value>>>>>>;

#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}
struct Inner {
    tx: mpsc::Sender<Frame>,
    pending: Pending,
    next: AtomicU64,
    events: broadcast::Sender<Result<ServerMessage>>,
    shutdown: watch::Sender<bool>,
    hello: ServerHello,
    home: Option<PathBuf>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionCreateOptions {
    pub project_root: Option<String>,
    pub project_id: Option<String>,
    pub agent: Option<String>,
    pub approval_mode: Option<otto_proto::ApprovalMode>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub title: Option<String>,
    pub parent: Option<String>,
    pub effort: Option<Effort>,
}

/// Fields for `session.update`; `None` leaves the field unchanged.
#[derive(Clone, Debug, Default)]
pub struct SessionUpdate {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub effort: Option<Effort>,
    /// Back to the agent's effort, else the model's default.
    pub reset_effort: bool,
}

impl Connection {
    /// # Errors
    /// Socket or hello exchange failure (bounded to five seconds).
    pub async fn connect(home: &Path) -> Result<Self> {
        let mut conn = tokio::time::timeout(Duration::from_secs(5), async {
            Self::connect_stream(tokio::net::UnixStream::connect(paths::socket(home)).await?).await
        })
        .await
        .map_err(|_| ClientError::Timeout)??;
        Arc::get_mut(&mut conn.inner).expect("new connection").home = Some(home.to_owned());
        Ok(conn)
    }
    /// # Errors
    /// Transport, malformed hello, or protocol incompatibility.
    pub async fn connect_stream<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(stream: S) -> Result<Self> {
        Self::connect_stream_with_hello(stream, ClientHello::ours("otto-client", ClientRole::Owner)).await
    }
    /// # Errors
    /// Transport, malformed hello, or protocol incompatibility.
    pub async fn connect_stream_with_hello<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(mut stream: S, hello: ClientHello) -> Result<Self> {
        let hello = tokio::time::timeout(Duration::from_secs(5), async {
            write_frame(&mut stream, &Frame::json(kind::HELLO, &hello)?).await?;
            stream.flush().await?;
            let frame = read_frame(&mut stream).await?.ok_or(ClientError::Closed)?;
            if frame.kind != kind::HELLO {
                if let Ok(ServerMessage::Response { error: Some(error), .. }) = frame.parse()
                    && error.code != RpcError::INCOMPATIBLE
                {
                    return Err(ClientError::Rpc(error));
                }
                return Err(ClientError::Incompatible { expected: PROTOCOL, got: None });
            }
            let hello: ServerHello = frame.parse().map_err(|_| ClientError::Incompatible { expected: PROTOCOL, got: None })?;
            if hello.protocol != PROTOCOL {
                return Err(ClientError::Incompatible { expected: PROTOCOL, got: Some(hello.protocol) });
            }
            Ok(hello)
        })
        .await
        .map_err(|_| ClientError::Timeout)??;
        let (tx, mut rx) = mpsc::channel::<Frame>(64);
        let (events, _) = broadcast::channel(1024);
        let (shutdown, mut stop) = watch::channel(false);
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (mut reader, mut writer) = tokio::io::split(stream);
        let p = pending.clone();
        let e = events.clone();
        tokio::spawn(async move {
            let read = async {
                loop {
                    let frame = read_frame(&mut reader).await?.ok_or(ClientError::Closed)?;
                    if frame.kind != kind::CONTROL {
                        return Err(ClientError::Json("expected CONTROL frame".into()));
                    }
                    let message: ServerMessage = frame.parse()?;
                    match message {
                        ServerMessage::Response { id, result, error } => {
                            if let Some(sender) = p.lock().unwrap().as_mut().and_then(|map| map.remove(&id)) {
                                let _ = sender.send(match error {
                                    Some(e) => Err(e.into()),
                                    None => Ok(result.unwrap_or(Value::Null)),
                                });
                            }
                        }
                        event => {
                            let _ = e.send(Ok(event));
                        }
                    }
                }
                #[allow(unreachable_code)]
                Ok::<(), ClientError>(())
            };
            let write = async {
                while let Some(frame) = rx.recv().await {
                    write_frame(&mut writer, &frame).await?;
                    writer.flush().await?;
                }
                Ok::<(), ClientError>(())
            };
            let error = tokio::select! {
                result = read => result.err().unwrap_or(ClientError::Closed),
                result = write => result.err().unwrap_or(ClientError::Closed),
                _ = stop.changed() => ClientError::Closed,
            };
            tracing::debug!(%error, "otto connection closed");
            if let Some(calls) = p.lock().unwrap().take() {
                for (_, sender) in calls {
                    let _ = sender.send(Err(error.clone()));
                }
            }
            let _ = e.send(Err(error));
        });
        Ok(Self { inner: Arc::new(Inner { tx, pending, next: AtomicU64::new(1), events, shutdown, hello, home: None }) })
    }
    #[must_use]
    pub fn hello(&self) -> &ServerHello {
        &self.inner.hello
    }
    /// Subscribe before issuing session.subscribe; replay can precede its response.
    #[must_use]
    pub fn events(&self) -> Events {
        let rx = self.inner.events.subscribe();
        if self.inner.pending.lock().unwrap().is_none() {
            let _ = self.inner.events.send(Err(ClientError::Closed));
        }
        rx
    }
    pub fn close(&self) {
        self.inner.shutdown.send_replace(true);
    }
    /// The transport has ended: every call fails from now on.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.pending.lock().unwrap().is_none()
    }
    /// # Errors
    /// No local transport, or reconnect failure. Tunnels supply a new connection to the follower.
    pub async fn reconnect(&self) -> Result<Self> {
        Self::connect(self.inner.home.as_deref().ok_or(ClientError::NoReconnect)?).await
    }
    /// # Errors
    /// Serialization, frame limit, transport, or server error.
    pub async fn call(&self, method: Method) -> Result<Value> {
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let frame = Frame::json(kind::CONTROL, &Request { id, method })?;
        if frame.payload.len() >= MAX_FRAME {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame exceeds MAX_FRAME").into());
        }
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().as_mut().ok_or(ClientError::Closed)?.insert(id, tx);
        let _guard = PendingGuard { id, pending: self.inner.pending.clone() };
        self.inner.tx.send(frame).await.map_err(|_| ClientError::Closed)?;
        rx.await.map_err(|_| ClientError::Closed)?
    }
    /// # Errors
    /// Call failure or a result that does not decode as `T`.
    pub async fn call_typed<T: DeserializeOwned>(&self, method: Method) -> Result<T> {
        Ok(serde_json::from_value(self.call(method).await?)?)
    }
    pub async fn session_create(&self, o: SessionCreateOptions) -> Result<SessionInfo> {
        self.call_typed(Method::SessionCreate {
            project_root: o.project_root,
            project_id: o.project_id,
            agent: o.agent,
            approval_mode: o.approval_mode,
            model: o.model,
            provider: o.provider,
            title: o.title,
            parent: o.parent,
            effort: o.effort,
        })
        .await
    }
    pub async fn session_update(&self, session_id: &str, update: SessionUpdate) -> Result<SessionInfo> {
        let SessionUpdate { provider, model, agent, effort, reset_effort } = update;
        self.call_typed(Method::SessionUpdate { session_id: session_id.into(), provider, model, agent, effort, reset_effort }).await
    }
    pub async fn project_list(&self) -> Result<Vec<ProjectInfo>> {
        self.call_typed(Method::ProjectList {}).await
    }
    pub async fn session_list(&self, project_id: Option<String>, include_archived: bool) -> Result<Vec<SessionInfo>> {
        self.call_typed(Method::SessionList { project_id, include_archived }).await
    }
    pub async fn session_view(&self, session_id: &str) -> Result<SessionView> {
        self.call_typed(Method::SessionView { session_id: session_id.into() }).await
    }
    pub async fn session_subscribe(&self, session_id: &str, after_seq: Option<i64>) -> Result<Subscribed> {
        self.call_typed(Method::SessionSubscribe { session_id: session_id.into(), after_seq }).await
    }
    pub async fn prompt(&self, session_id: &str, input: Vec<InputPart>, delivery: Delivery) -> Result<PromptAccepted> {
        self.call_typed(Method::SessionPrompt { session_id: session_id.into(), input, delivery }).await
    }
    pub async fn cancel(&self, session_id: &str) -> Result<Empty> {
        self.call_typed(Method::SessionCancel { session_id: session_id.into() }).await
    }
    /// Send a stopped sub-agent's held report to its parent now.
    pub async fn hand_back(&self, session_id: &str) -> Result<Empty> {
        self.call_typed(Method::SessionHandBack { session_id: session_id.into() }).await
    }
    /// Withdraw a pending input (queued, or a steer the model has not taken yet).
    pub async fn queue_remove(&self, session_id: &str, input_id: &str) -> Result<Empty> {
        self.call_typed(Method::SessionQueueRemove { session_id: session_id.into(), input_id: input_id.into() }).await
    }
    /// Move a pending input to `Steer` (next step), `Queue` (after the turn) or `Interrupt` (now).
    pub async fn queue_update(&self, session_id: &str, input_id: &str, delivery: Delivery) -> Result<Empty> {
        self.call_typed(Method::SessionQueueUpdate { session_id: session_id.into(), input_id: input_id.into(), delivery }).await
    }
    pub async fn skills_list(&self, project_root: Option<String>) -> Result<Vec<SkillInfo>> {
        self.call_typed(Method::SkillsList { project_root }).await
    }
    pub async fn commands_list(&self, project_root: Option<String>) -> Result<Vec<CommandInfo>> {
        self.call_typed(Method::CommandsList { project_root }).await
    }
    pub async fn approval_answer(&self, session_id: &str, approval_id: &str, decision: ApprovalDecision, note: Option<String>) -> Result<Empty> {
        self.call_typed(Method::ApprovalAnswer { session_id: session_id.into(), approval_id: approval_id.into(), decision, note }).await
    }
    pub async fn question_answer(&self, session_id: &str, question_id: &str, answer: String) -> Result<Empty> {
        self.call_typed(Method::QuestionAnswer { session_id: session_id.into(), question_id: question_id.into(), answer }).await
    }
    pub async fn auth_status(&self) -> Result<Vec<AuthStatus>> {
        self.call_typed(Method::AuthStatus {}).await
    }
    pub async fn model_list(&self, provider: Option<String>) -> Result<Vec<ModelInfo>> {
        self.call_typed(Method::ModelList { provider }).await
    }
}
struct PendingGuard {
    id: u64,
    pending: Pending,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(map) = self.pending.lock().unwrap().as_mut() {
            map.remove(&self.id);
        }
    }
}
