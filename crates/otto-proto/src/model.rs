//! Snapshot and input types; timestamps and sequences match the durable journal.

use otto_llm::{Effort, Item, MentionKind, Usage};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type SessionId = String;
pub type ProjectId = String;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProjectInfo {
    pub id: ProjectId,
    pub root: String,
    pub created_at: i64,
    /// Latest `updated_at` among the project's sessions, else `created_at`.
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Idle,
    Busy,
    WaitingApproval,
    Error,
    Archived,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UsageTotals {
    pub usage: Usage,
    pub cost_usd: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    #[default]
    Auto,
    AskOnEffects,
    AskAll,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub parent_id: Option<SessionId>,
    pub agent: String,
    #[serde(default)]
    pub approval_mode: ApprovalMode,
    pub provider: String,
    pub model: String,
    /// Reasoning effort for models that reason; `None` is the model's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    pub title: String,
    pub cwd: String,
    pub status: SessionStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seq: i64,
    pub usage: UsageTotals,
    /// A sub-agent the user stopped holds its report for the parent until it
    /// reports: a later turn, `session.hand_back`, or the next otto start.
    #[serde(default)]
    pub report_held: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputPart {
    Text {
        text: String,
    },
    Image {
        mime: String,
        data_base64: String,
    },
    File {
        path: String,
    },
    /// A file stored by `attachment.put`: the model gets a note naming it
    /// (`[attachment att_…: name, mime, size]`), and images downscaled.
    Attachment {
        id: String,
    },
    /// `@target` at this point of the text: an agent or a project path (a
    /// folder ends in `/`). The model gets a note naming them; the user item
    /// keeps them as `Item::mentions`.
    Mention {
        kind: MentionKind,
        target: String,
    },
}

/// A `files.complete` match: a path relative to `cwd`, folders ending in `/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileMatch {
    pub path: String,
    pub dir: bool,
}

/// A file the user attached to a session (`attachment.put`); kept until the
/// session is archived.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentInfo {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Queue,
    Steer,
    Interrupt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
    AlwaysAllow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Done,
    Cancelled,
    Interrupted,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PendingApproval {
    pub approval_id: String,
    pub call_id: String,
    pub tool: String,
    pub summary: String,
    /// Redacted before crossing the wire.
    pub input: Value,
    pub effects: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PendingQuestion {
    pub question_id: String,
    pub call_id: String,
    pub question: String,
    pub options: Vec<String>,
}

/// A pending input. `queue.changed` and `SessionView.queue` list pending steers
/// first (`delivery: steer`, delivered at the next step boundary), then inputs
/// waiting for the turn to end (`queue`, `interrupt`).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct QueuedInput {
    pub id: String,
    pub input: Vec<InputPart>,
    pub delivery: Delivery,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunningToolCall {
    pub call_id: String,
    pub name: String,
    /// Redacted before crossing the wire.
    pub input: Value,
    /// When the call started (ms since the Unix epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
}

/// Snapshot at `seq`; subscribe with `after_seq = seq` to continue it.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionView {
    pub info: SessionInfo,
    pub items: Vec<Item>,
    pub pending_approvals: Vec<PendingApproval>,
    pub pending_questions: Vec<PendingQuestion>,
    pub queue: Vec<QueuedInput>,
    pub running_tool_calls: Vec<RunningToolCall>,
    pub seq: i64,
    /// When the running turn started (ms since the Unix epoch); `None` when idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_at: Option<i64>,
    /// When the items' reasoning parts and tool calls ran, from the journal
    /// (`timing::part_times`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub part_times: Vec<crate::timing::PartTime>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub source: String,
    pub effects: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    ApiKey,
    Subscription,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AuthStatus {
    pub provider: String,
    pub mode: AuthMode,
    pub signed_in: bool,
    pub expires_at: Option<i64>,
    pub account: Option<String>,
    /// Sign-in modes this provider supports, independent of the current state.
    #[serde(default)]
    pub modes: Vec<AuthMode>,
    /// Set on the provider new sessions default to: the model they use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// The endpoint of a local or custom provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Subscription plan label (`Pro`, `Max`, `Plus`, `Team`, …) when the sign-in reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CommandInfo {
    /// Typed as `/name arguments`.
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentInfo {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    #[serde(default)]
    pub scope: AgentScope,
}

/// Where an agent definition comes from: built in, the user's `agents/`, or the
/// project's `.otto/agents/` (and `.claude/agents/`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentScope {
    #[default]
    Builtin,
    User,
    Project,
}

/// An agent definition (`agents.get` / `agents.save`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// `provider/model` or a model id; `None` inherits.
    pub model: Option<String>,
    pub effort: Option<String>,
    pub approval_mode: Option<ApprovalMode>,
    pub prompt: String,
    /// Tools shown to the model: names and `prefix*` patterns. With `loadable`
    /// also empty, every tool, each with its own default loading.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Tools granted behind `load_tools`.
    #[serde(default)]
    pub loadable: Vec<String>,
    /// Removed from the grant (`disallowedTools`).
    #[serde(default)]
    pub disallowed: Vec<String>,
    pub scope: AgentScope,
    /// A user or project definition named like a built-in.
    #[serde(default)]
    pub overrides_builtin: bool,
}

/// A tool an agent can list (`tools.catalog`).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolCatalogEntry {
    pub name: String,
    /// The first sentence of its description.
    pub description: String,
    /// `builtin`, `silo`, `mcp:<server>` or `ext:<plugin>`.
    pub source: String,
    /// The tool's own default: `default` (shown) or `deferred` (behind `load_tools`).
    pub loading: String,
    pub effects: Vec<String>,
}
