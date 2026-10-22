//! JSON requests, responses and sequenced events.

use otto_llm::{Item, Role, Usage};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientRole {
    Owner,
    Viewer,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TunnelPeer {
    pub device: String,
    pub label: String,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClientHello {
    #[serde(default)]
    pub protocol: u32,
    pub version: String,
    pub client_name: String,
    #[serde(default)]
    pub role: ClientRole,
    pub token: Option<String>,
    pub peer: Option<TunnelPeer>,
}

impl std::fmt::Debug for ClientHello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientHello")
            .field("protocol", &self.protocol)
            .field("version", &self.version)
            .field("client_name", &self.client_name)
            .field("role", &self.role)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("peer", &self.peer)
            .finish()
    }
}

impl ClientHello {
    #[must_use]
    pub fn ours(client_name: &str, role: ClientRole) -> Self {
        Self { protocol: crate::PROTOCOL, version: env!("CARGO_PKG_VERSION").into(), client_name: client_name.into(), role, token: None, peer: None }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServerHello {
    pub protocol: u32,
    pub version: String,
    pub server_id: String,
    pub pid: u32,
    pub started_at: i64,
    pub client_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServerInfo {
    pub protocol: u32,
    pub version: String,
    pub server_id: String,
    pub pid: u32,
    pub started_at: i64,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum Method {
    #[serde(rename = "server.info")]
    ServerInfo {},
    #[serde(rename = "server.shutdown")]
    ServerShutdown {},
    #[serde(rename = "api.list")]
    ApiList {},
    #[serde(rename = "api.describe")]
    ApiDescribe { method: String },
    #[serde(rename = "ping")]
    Ping {},
    #[serde(rename = "project.open")]
    ProjectOpen { root: String },
    #[serde(rename = "project.list")]
    ProjectList {},
    #[serde(rename = "session.create")]
    SessionCreate {
        /// Exactly one of project_root and project_id is required by the server.
        project_root: Option<String>,
        project_id: Option<ProjectId>,
        agent: Option<String>,
        approval_mode: Option<crate::model::ApprovalMode>,
        model: Option<String>,
        provider: Option<String>,
        title: Option<String>,
        parent: Option<SessionId>,
    },
    #[serde(rename = "session.list")]
    SessionList {
        project_id: Option<ProjectId>,
        #[serde(default)]
        include_archived: bool,
    },
    #[serde(rename = "session.view")]
    SessionView { session_id: SessionId },
    #[serde(rename = "session.subscribe")]
    SessionSubscribe { session_id: SessionId, after_seq: Option<i64> },
    #[serde(rename = "session.unsubscribe")]
    SessionUnsubscribe { session_id: SessionId },
    #[serde(rename = "session.fork")]
    SessionFork { session_id: SessionId, at_seq: Option<i64> },
    #[serde(rename = "session.rename")]
    SessionRename { session_id: SessionId, title: String },
    #[serde(rename = "session.set_mode")]
    SessionSetMode { session_id: SessionId, approval_mode: crate::model::ApprovalMode },
    #[serde(rename = "session.archive")]
    SessionArchive { session_id: SessionId },
    #[serde(rename = "session.prompt")]
    SessionPrompt {
        session_id: SessionId,
        input: Vec<InputPart>,
        #[serde(default)]
        delivery: Delivery,
    },
    #[serde(rename = "session.cancel")]
    SessionCancel { session_id: SessionId },
    #[serde(rename = "session.compact")]
    SessionCompact { session_id: SessionId },
    #[serde(rename = "approval.answer")]
    ApprovalAnswer { session_id: SessionId, approval_id: String, decision: ApprovalDecision, note: Option<String> },
    #[serde(rename = "question.answer")]
    QuestionAnswer { session_id: SessionId, question_id: String, answer: String },
    #[serde(rename = "model.list")]
    ModelList { provider: Option<String> },
    #[serde(rename = "auth.status")]
    AuthStatus {},
    #[serde(rename = "auth.login")]
    AuthLogin {
        provider: String,
        mode: AuthMode,
        api_key: Option<String>,
        /// Request subscription device authorization instead of browser OAuth.
        #[serde(default)]
        device: bool,
    },
    #[serde(rename = "auth.login_complete")]
    AuthLoginComplete { login_id: String, code: Option<String> },
    #[serde(rename = "auth.login_cancel")]
    AuthLoginCancel { login_id: String },
    #[serde(rename = "auth.logout")]
    AuthLogout { provider: String },
    #[serde(rename = "tools.list")]
    ToolsList { session_id: Option<SessionId> },
    #[serde(rename = "agents.list")]
    AgentsList {},
    #[serde(rename = "config.get")]
    ConfigGet { key: Option<String> },
    #[serde(rename = "config.set")]
    ConfigSet { key: String, value: Value },
}

impl std::fmt::Debug for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut value = serde_json::to_value(self).map_err(|_| std::fmt::Error)?;
        match self {
            Self::AuthLogin { api_key: Some(_), .. } => value["params"]["api_key"] = Value::String("[REDACTED]".into()),
            Self::AuthLoginComplete { code: Some(_), .. } => value["params"]["code"] = Value::String("[REDACTED]".into()),
            _ => {}
        }
        f.debug_tuple("Method").field(&value).finish()
    }
}

pub const METHODS: &[(&str, &str)] = &[
    ("server.info", "Server identity and version."),
    ("server.shutdown", "Stop the server."),
    ("api.list", "List supported methods."),
    ("api.describe", "Parameter and result schemas and an example request."),
    ("ping", "Probe server liveness."),
    ("project.open", "Open or register a project root."),
    ("project.list", "List registered projects."),
    ("session.create", "Create a session in a project."),
    ("session.list", "List sessions, optionally including archived sessions."),
    ("session.view", "Snapshot a session and its durable sequence boundary."),
    ("session.subscribe", "Replay events after a sequence, then receive live events."),
    ("session.unsubscribe", "Stop receiving this session's events."),
    ("session.fork", "Fork a session at a durable sequence boundary."),
    ("session.rename", "Change a session title."),
    ("session.set_mode", "Change approval mode for subsequent tool calls; plan agents remain read-only."),
    ("session.archive", "Archive a session."),
    ("session.prompt", "Submit input: queue, steer at the next step, or interrupt."),
    ("session.cancel", "Cancel the current turn and its children."),
    ("session.compact", "Compact session history."),
    ("approval.answer", "Resolve a pending tool approval."),
    ("question.answer", "Answer a pending question."),
    ("model.list", "List available provider models."),
    ("auth.status", "List provider authentication status without secrets."),
    ("auth.login", "Store an API key or begin subscription login."),
    ("auth.login_complete", "Complete subscription login."),
    ("auth.login_cancel", "Abandon a pending subscription login."),
    ("auth.logout", "Remove provider credentials."),
    ("tools.list", "List tools available to a session."),
    ("agents.list", "List agent definitions."),
    ("config.get", "Read a configuration key, or all configuration."),
    ("config.set", "Set a configuration key."),
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "RawRequest")]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub method: Method,
}

#[derive(Deserialize)]
struct RawRequest {
    id: u64,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

impl TryFrom<RawRequest> for Request {
    type Error = String;

    fn try_from(raw: RawRequest) -> Result<Self, Self::Error> {
        let params = raw.params.unwrap_or_else(|| serde_json::json!({}));
        let method = serde_json::from_value(serde_json::json!({"method": raw.method, "params": params})).map_err(|e| e.to_string())?;
        Ok(Self { id: raw.id, method })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Empty {}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ApiMethod {
    pub method: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MethodDescription {
    pub description: String,
    pub params_schema: Value,
    pub result_schema: Value,
    pub example: Value,
}

/// Subscription is installed before this acknowledgement. Events may precede it.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Subscribed {
    pub session_id: SessionId,
    pub seq: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PromptAccepted {
    pub input_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoginResult {
    SignedIn {
        auth: AuthStatus,
    },
    Pending {
        url: String,
        login_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification_uri: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval_secs: Option<u64>,
        /// The provider shows a code to paste into `auth.login_complete`; otherwise
        /// `auth.login_complete` without a code waits for the browser or device approval.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        needs_code: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ConfigValue {
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub const BAD_REQUEST: &str = "bad_request";
    pub const NOT_FOUND: &str = "not_found";
    pub const FORBIDDEN: &str = "forbidden";
    pub const INTERNAL: &str = "internal";
    pub const BUSY: &str = "busy";
    pub const METHOD_NOT_FOUND: &str = "method_not_found";
    pub const INCOMPATIBLE: &str = "incompatible";
    pub const AUTH_REQUIRED: &str = "auth_required";

    #[must_use]
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), data: None }
    }
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(Self::BAD_REQUEST, message)
    }
    #[must_use]
    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Self::new(Self::NOT_FOUND, format!("{what} not found"))
    }
    #[must_use]
    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new(Self::INTERNAL, message.to_string())
    }
    #[must_use]
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(Self::FORBIDDEN, message)
    }
    #[must_use]
    pub fn busy(message: impl Into<String>) -> Self {
        Self::new(Self::BUSY, message)
    }
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(Self::METHOD_NOT_FOUND, format!("{method} is not supported by this server"))
    }
    #[must_use]
    pub fn incompatible(message: impl Into<String>) -> Self {
        Self::new(Self::INCOMPATIBLE, message)
    }
    #[must_use]
    pub fn auth_required(message: impl Into<String>) -> Self {
        Self::new(Self::AUTH_REQUIRED, message)
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for RpcError {}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ServerMessage {
    /// Ephemeral overlay, never a journal event. `id: 0` lets older clients
    /// ignore this as an unsolicited response. Send with `live: true`.
    Live { live: bool, id: u64, session_id: SessionId, after_seq: i64, event: Box<Event> },
    /// Unknown or malformed ephemeral payload; never advance the journal cursor.
    LiveUnknown { session_id: SessionId, after_seq: i64, event: String },
    Response {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
    Event {
        /// Per-session journal sequence when session_id is set; global index sequence otherwise.
        seq: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        #[serde(flatten)]
        event: Box<Event>,
    },
    /// An unknown event from a newer peer; safe to skip.
    UnknownEvent { seq: i64, session_id: Option<SessionId> },
    /// A known event failed to decode; re-fetch the affected snapshot.
    MalformedEvent { seq: i64, session_id: Option<SessionId>, event: String, error: String },
}

impl<'de> Deserialize<'de> for ServerMessage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Response {
            id: u64,
            #[serde(default)]
            result: Option<Value>,
            #[serde(default)]
            error: Option<RpcError>,
        }
        let mut value = Value::deserialize(deserializer)?;
        if value.get("live").is_some_and(|live| live != &Value::Bool(true)) {
            return Err(serde::de::Error::custom("invalid live marker"));
        }
        if value.get("live") == Some(&Value::Bool(true)) {
            let session_id = serde_json::from_value(value["session_id"].clone()).map_err(serde::de::Error::custom)?;
            let after_seq = value["after_seq"].as_i64().filter(|seq| *seq >= 0).ok_or_else(|| serde::de::Error::custom("invalid live anchor"))?;
            if value.get("id").and_then(Value::as_u64) != Some(0) || value.get("seq").is_some() {
                return Err(serde::de::Error::custom("invalid live envelope"));
            }
            let name = value["event"]["event"].as_str().unwrap_or_default().to_owned();
            return Ok(match serde_json::from_value::<Event>(value["event"].clone()) {
                Ok(event) if event.is_live_delta() => Self::Live { live: true, id: 0, session_id, after_seq, event: Box::new(event) },
                _ => Self::LiveUnknown { session_id, after_seq, event: name },
            });
        }
        if let Some(seq) = value.as_object_mut().and_then(|o| o.remove("seq")) {
            let seq = seq.as_i64().ok_or_else(|| serde::de::Error::custom("seq is not an i64"))?;
            let session_id = serde_json::from_value(value.as_object_mut().and_then(|o| o.remove("session_id")).unwrap_or(Value::Null))
                .map_err(serde::de::Error::custom)?;
            let name = value.get("event").and_then(Value::as_str).unwrap_or_default().to_owned();
            if !EVENTS.contains(&name.as_str()) {
                return Ok(Self::UnknownEvent { seq, session_id });
            }
            return Ok(match serde_json::from_value::<Event>(value) {
                Ok(event) => Self::Event { seq, session_id, event: Box::new(event) },
                Err(error) => Self::MalformedEvent { seq, session_id, event: name, error: error.to_string() },
            });
        }
        let Response { id, result, error } = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self::Response { id, result, error })
    }
}

pub const EVENTS: &[&str] = &[
    "sessions.changed",
    "session.changed",
    "turn.started",
    "turn.finished",
    "item.started",
    "item.done",
    "text.delta",
    "reasoning.delta",
    "tool.input_delta",
    "tool.call",
    "tool.progress",
    "tool.result",
    "approval.requested",
    "approval.resolved",
    "question.asked",
    "question.resolved",
    "usage",
    "compaction",
    "subagent.changed",
    "queue.changed",
    "error",
];

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", content = "data")]
pub enum Event {
    #[serde(rename = "sessions.changed")]
    SessionsChanged {},
    #[serde(rename = "session.changed")]
    SessionChanged { info: SessionInfo },
    #[serde(rename = "turn.started")]
    TurnStarted { turn: i64 },
    #[serde(rename = "turn.finished")]
    TurnFinished { turn: i64, status: TurnStatus, usage: Usage },
    #[serde(rename = "item.started")]
    ItemStarted { item_id: String, role: Role },
    #[serde(rename = "item.done")]
    ItemDone { item: Item },
    #[serde(rename = "text.delta")]
    TextDelta { item_id: String, text: String },
    #[serde(rename = "reasoning.delta")]
    ReasoningDelta { item_id: String, text: String, summary: bool },
    #[serde(rename = "tool.input_delta")]
    ToolInputDelta { call_id: String, delta: String },
    #[serde(rename = "tool.call")]
    ToolCall { call_id: String, name: String, input: Value },
    #[serde(rename = "tool.progress")]
    ToolProgress { call_id: String, message: Option<String>, output: Option<Value> },
    #[serde(rename = "tool.result")]
    ToolResult { call_id: String, is_error: bool, output: Value, item_id: String },
    #[serde(rename = "approval.requested")]
    ApprovalRequested { approval_id: String, call_id: String, tool: String, summary: String, input: Value, effects: Vec<String> },
    #[serde(rename = "approval.resolved")]
    ApprovalResolved { approval_id: String, decision: ApprovalDecision },
    #[serde(rename = "question.asked")]
    QuestionAsked { question_id: String, call_id: String, question: String, options: Vec<String> },
    #[serde(rename = "question.resolved")]
    QuestionResolved { question_id: String, answer: String },
    #[serde(rename = "usage")]
    Usage { turn: i64, step: i64, usage: Usage, cost_usd: Option<f64> },
    #[serde(rename = "compaction")]
    Compaction { through_seq: i64, summary: Option<String> },
    #[serde(rename = "subagent.changed")]
    SubagentChanged { parent: SessionId, child: SessionId, status: SessionStatus },
    #[serde(rename = "queue.changed")]
    QueueChanged { queued: Vec<QueuedInput> },
    #[serde(rename = "error")]
    Error { class: String, message: String, retryable: bool },
}

impl Event {
    #[must_use]
    pub fn is_live_delta(&self) -> bool {
        matches!(self, Self::TextDelta { .. } | Self::ReasoningDelta { .. } | Self::ToolInputDelta { .. } | Self::ToolProgress { .. })
    }
}
