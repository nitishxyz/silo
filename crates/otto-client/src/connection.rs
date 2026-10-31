use otto_proto::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
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

/// A bidirectional byte stream a connection runs over (a socket, a tunnel stream).
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}
/// Opens a fresh stream to the daemon; a dialed connection reconnects through it.
pub type Dial = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Box<dyn Stream>>> + Send + 'static>> + Send + Sync>;

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
    starting: watch::Receiver<Option<Starting>>,
    redial: Option<Redial>,
}
enum Redial {
    Home(PathBuf),
    Dial(Dial),
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
    /// Move the idle session to this folder's project (absolute or `~/…`).
    pub folder: Option<String>,
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
        Arc::get_mut(&mut conn.inner).expect("new connection").redial = Some(Redial::Home(home.to_owned()));
        Ok(conn)
    }
    /// Connect over a stream from `dial`; `reconnect` dials again.
    /// # Errors
    /// Dial failure, transport, malformed hello, or protocol incompatibility.
    pub async fn connect_dial(dial: Dial) -> Result<Self> {
        let mut conn = Self::connect_stream(dial().await?).await?;
        Arc::get_mut(&mut conn.inner).expect("new connection").redial = Some(Redial::Dial(dial));
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
        let (starts, starting) = watch::channel(hello.starting.clone());
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
                            if let ServerMessage::Event { session_id: None, event: global, .. } = &event {
                                match &**global {
                                    Event::ServerStarting { starting } => _ = starts.send_replace(Some(starting.clone())),
                                    Event::ServerReady {} => _ = starts.send_replace(None),
                                    _ => {}
                                }
                            }
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
        Ok(Self { inner: Arc::new(Inner { tx, pending, next: AtomicU64::new(1), events, shutdown, hello, starting, redial: None }) })
    }
    #[must_use]
    pub fn hello(&self) -> &ServerHello {
        &self.inner.hello
    }
    /// What the daemon is doing while it starts ([`ServerHello::starting`], then `server.starting`), `None` once
    /// it is ready. Closes with the connection.
    #[must_use]
    pub fn starting(&self) -> watch::Receiver<Option<Starting>> {
        self.inner.starting.clone()
    }
    /// Wait until the daemon is ready, telling `progress` what it does meanwhile.
    /// # Errors
    /// The connection closed first.
    pub async fn ready(&self, mut progress: impl FnMut(&Starting)) -> Result<()> {
        let mut starting = self.starting();
        loop {
            match &*starting.borrow_and_update() {
                Some(now) => progress(now),
                None => return Ok(()),
            }
            starting.changed().await.map_err(|_| ClientError::Closed)?;
        }
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
    /// Neither a home nor a dialer to reconnect through, or reconnect failure.
    pub async fn reconnect(&self) -> Result<Self> {
        match self.inner.redial.as_ref().ok_or(ClientError::NoReconnect)? {
            Redial::Home(home) => Self::connect(home).await,
            Redial::Dial(dial) => Self::connect_dial(dial.clone()).await,
        }
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
        let SessionUpdate { provider, model, agent, effort, reset_effort, folder } = update;
        self.call_typed(Method::SessionUpdate { session_id: session_id.into(), provider, model, agent, effort, reset_effort, folder }).await
    }
    /// A new session with the history through `at_seq` (all of it when `None`); its `forked_from` names this one.
    pub async fn session_fork(&self, session_id: &str, at_seq: Option<i64>) -> Result<SessionInfo> {
        self.call_typed(Method::SessionFork { session_id: session_id.into(), at_seq }).await
    }
    pub async fn session_rename(&self, session_id: &str, title: &str) -> Result<Empty> {
        self.call_typed(Method::SessionRename { session_id: session_id.into(), title: title.into() }).await
    }
    pub async fn project_list(&self) -> Result<Vec<ProjectInfo>> {
        self.call_typed(Method::ProjectList {}).await
    }
    pub async fn session_list(&self, project_id: Option<String>, include_archived: bool) -> Result<Vec<SessionInfo>> {
        self.call_typed(Method::SessionList { project_id, include_archived }).await
    }
    /// The session's newest items that fit one reply (`has_older` when more are stored).
    pub async fn session_view(&self, session_id: &str) -> Result<SessionView> {
        self.call_typed(Method::SessionView { session_id: session_id.into(), limit: None, before_seq: None }).await
    }
    /// A page of at most `limit` items stored before item sequence `before_seq`
    /// (a view's `oldest_seq`). An older server ignores both and sends every item.
    pub async fn session_view_before(&self, session_id: &str, before_seq: i64, limit: Option<u32>) -> Result<SessionView> {
        self.call_typed(Method::SessionView { session_id: session_id.into(), limit, before_seq: Some(before_seq) }).await
    }
    pub async fn session_subscribe(&self, session_id: &str, after_seq: Option<i64>) -> Result<Subscribed> {
        self.call_typed(Method::SessionSubscribe { session_id: session_id.into(), after_seq }).await
    }
    pub async fn prompt(&self, session_id: &str, input: Vec<InputPart>, delivery: Delivery) -> Result<PromptAccepted> {
        self.call_typed(Method::SessionPrompt { session_id: session_id.into(), input, delivery }).await
    }
    /// Store a file (or a chunk of one, `append_to` its id) for the session's prompts.
    pub async fn attachment_put(
        &self,
        session_id: &str,
        name: &str,
        mime: &str,
        data_base64: String,
        append_to: Option<String>,
    ) -> Result<AttachmentInfo> {
        self.call_typed(Method::AttachmentPut { session_id: session_id.into(), name: name.into(), mime: mime.into(), data_base64, append_to }).await
    }
    pub async fn cancel(&self, session_id: &str) -> Result<Empty> {
        self.call_typed(Method::SessionCancel { session_id: session_id.into() }).await
    }
    /// Send a stopped sub-agent's held report to its parent now.
    pub async fn hand_back(&self, session_id: &str) -> Result<Empty> {
        self.call_typed(Method::SessionHandBack { session_id: session_id.into() }).await
    }
    /// Summarize the session's history now; returns once the compaction is recorded.
    pub async fn compact(&self, session_id: &str) -> Result<Empty> {
        self.call_typed(Method::SessionCompact { session_id: session_id.into() }).await
    }
    /// Detach a running shell call: it returns now and its command runs on as a background job.
    pub async fn detach_shell(&self, session_id: &str, call_id: &str) -> Result<Empty> {
        self.call_typed(Method::ShellDetach { session_id: session_id.into(), call_id: call_id.into() }).await
    }
    /// The session's background shell jobs.
    pub async fn jobs(&self, session_id: &str) -> Result<Vec<JobInfo>> {
        self.call_typed(Method::JobList { session_id: session_id.into() }).await
    }
    /// Stop one of the session's background shell jobs.
    pub async fn kill_job(&self, session_id: &str, job_id: &str) -> Result<Empty> {
        self.call_typed(Method::JobKill { session_id: session_id.into(), job_id: job_id.into() }).await
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
    /// Paths under `cwd` matching an `@` mention's `query`, best first (folders end in `/`).
    pub async fn files_complete(&self, cwd: &str, query: &str, limit: u32) -> Result<Vec<FileMatch>> {
        self.call_typed(Method::FilesComplete { cwd: cwd.into(), query: query.into(), limit }).await
    }
    pub async fn agents_list(&self) -> Result<Vec<AgentInfo>> {
        self.call_typed(Method::AgentsList {}).await
    }
    /// An agent as discovery resolves it; project definitions need `cwd`.
    pub async fn agents_get(&self, name: &str, cwd: Option<String>) -> Result<AgentDef> {
        self.call_typed(Method::AgentsGet { name: name.into(), cwd }).await
    }
    /// Write a user or project definition; a different `previous_name` removes that scope's old file.
    pub async fn agents_save(&self, agent: AgentDef, previous_name: Option<String>, cwd: Option<String>) -> Result<AgentDef> {
        self.call_typed(Method::AgentsSave { agent, previous_name, cwd }).await
    }
    pub async fn agents_delete(&self, name: &str, scope: AgentScope, cwd: Option<String>) -> Result<Empty> {
        self.call_typed(Method::AgentsDelete { name: name.into(), scope, cwd }).await
    }
    /// Every tool an agent can be granted, with `cwd`'s MCP tools.
    pub async fn tools_catalog(&self, cwd: Option<String>) -> Result<Vec<ToolCatalogEntry>> {
        self.call_typed(Method::ToolsCatalog { cwd }).await
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
    /// Every session's usage since `since_ms`, days starting `utc_offset_minutes` east of UTC.
    pub async fn usage_summary(&self, since_ms: i64, utc_offset_minutes: Option<i32>) -> Result<UsageSummary> {
        self.call_typed(Method::UsageSummary { since_ms, utc_offset_minutes }).await
    }
    /// MCP servers for `project_root` (global ones only without it), with status and tool count.
    pub async fn mcp_list(&self, project_root: Option<String>) -> Result<Vec<McpServer>> {
        self.call_typed(Method::McpList { project_root }).await
    }
    pub async fn mcp_add(
        &self,
        project_root: Option<String>,
        scope: McpScope,
        name: &str,
        transport: McpTransport,
        enabled: bool,
    ) -> Result<McpServer> {
        self.call_typed(Method::McpAdd { project_root, scope, name: name.into(), transport, enabled }).await
    }
    pub async fn mcp_update(
        &self,
        project_root: Option<String>,
        scope: McpScope,
        name: &str,
        transport: Option<McpTransport>,
        enabled: Option<bool>,
    ) -> Result<McpServer> {
        self.call_typed(Method::McpUpdate { project_root, scope, name: name.into(), transport, enabled }).await
    }
    pub async fn mcp_remove(&self, project_root: Option<String>, scope: McpScope, name: &str) -> Result<Empty> {
        self.call_typed(Method::McpRemove { project_root, scope, name: name.into() }).await
    }
    pub async fn mcp_login(&self, project_root: Option<String>, name: &str) -> Result<McpLoginStarted> {
        self.call_typed(Method::McpLogin { project_root, name: name.into() }).await
    }
    /// Waits for the browser without `code`; otherwise finishes with the pasted callback URL (or code).
    pub async fn mcp_login_complete(&self, login_id: &str, code: Option<String>) -> Result<McpServer> {
        self.call_typed(Method::McpLoginComplete { login_id: login_id.into(), code }).await
    }
    pub async fn mcp_login_cancel(&self, login_id: &str) -> Result<Empty> {
        self.call_typed(Method::McpLoginCancel { login_id: login_id.into() }).await
    }
    pub async fn mcp_logout(&self, project_root: Option<String>, name: &str) -> Result<Empty> {
        self.call_typed(Method::McpLogout { project_root, name: name.into() }).await
    }
    /// Dictated `text` tidied by a small model, spelling `vocabulary` as given.
    pub async fn text_polish(&self, text: String, vocabulary: Vec<String>, style: TextStyle) -> Result<String> {
        Ok(self.call_typed::<PolishedText>(Method::TextPolish { text, vocabulary, style }).await?.text)
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
