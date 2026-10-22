//! Snapshot and input types; timestamps and sequences match the durable journal.

use otto_llm::{Effort, Item, Usage};
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
    Plan,
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputPart {
    Text { text: String },
    Image { mime: String, data_base64: String },
    File { path: String },
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
}
