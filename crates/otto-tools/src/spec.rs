use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub effects: Effects,
    /// JSON Pointers into the input redacted in the log, events and
    /// approvals.
    #[serde(default)]
    pub sensitive: Vec<String>,
    #[serde(default)]
    pub loading: Loading,
    #[serde(default)]
    pub mode: CallMode,
    /// Model-family shapes (§5); empty: the generic JSON shape for everyone.
    #[serde(default)]
    pub presentations: Vec<Presentation>,
    #[serde(default)]
    pub source: Source,
    /// Calls must not run in parallel with other exclusive calls.
    #[serde(default)]
    pub exclusive: bool,
}

impl ToolSpec {
    /// The shape `family` sees: its presentation, if it has one.
    #[must_use]
    pub fn presentation(&self, family: &str) -> Option<&Presentation> {
        self.presentations.iter().find(|p| p.family == family)
    }
}

/// What a call may do; drives approval policy (§5.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct Effects {
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
    #[serde(default)]
    pub process: bool,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub secrets: bool,
    /// Writes outside this machine (pushes, posts, messages).
    #[serde(default)]
    pub external: bool,
}

impl Effects {
    pub const NONE: Self = Self { read: false, write: false, process: false, network: false, secrets: false, external: false };
    pub const READ: Self = Self { read: true, ..Self::NONE };
    pub const WRITE: Self = Self { read: true, write: true, ..Self::NONE };
    pub const PROCESS: Self = Self { read: true, write: true, process: true, ..Self::NONE };
    pub const NETWORK: Self = Self { network: true, ..Self::NONE };

    /// Read-only: no approval in `ask_on_effects`.
    #[must_use]
    pub fn read_only(&self) -> bool {
        !(self.write || self.process || self.external || self.secrets)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Loading {
    #[default]
    Eager,
    /// Not in the prompt until loaded (tool search / `load_tools`).
    Deferred,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallMode {
    #[default]
    Sync,
    /// Long-running: returns a handle, the result arrives later.
    Async,
}

/// One model-facing shape of a tool for a tool family (`anthropic`,
/// `openai`, `gemini`, `generic`). The executor gets the input as the
/// model produced it in this shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Presentation {
    pub family: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema; `None` with `freeform` set means a custom/freeform tool.
    #[serde(default)]
    pub input_schema: Option<Value>,
    /// Freeform tool format (e.g. a Lark grammar); input arrives as a string.
    #[serde(default)]
    pub freeform: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    #[default]
    Builtin,
    Silo,
    Mcp {
        server: String,
    },
    Ext {
        plugin: String,
    },
}

/// Asks the user whether a call may run (or a question). Implemented by the
/// runtime; tools call it for decisions they make mid-call.
#[async_trait]
pub trait Approver: Send + Sync {
    /// `true` when allowed.
    async fn approve(&self, call_id: &str, summary: &str, detail: Value) -> bool;
    /// Free-form question to the user (`ask_user`); `None` if unanswered.
    async fn ask(&self, call_id: &str, question: &str, options: &[String]) -> Option<String>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub call_id: String,
    #[serde(default)]
    pub message: Option<String>,
    /// Output produced so far (e.g. shell output chunks).
    #[serde(default)]
    pub output: Option<String>,
}

/// Everything a call gets besides its input.
#[derive(Clone)]
pub struct ToolCx {
    pub call_id: String,
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    pub cancel: CancellationToken,
    pub progress: Option<mpsc::UnboundedSender<Progress>>,
    pub approver: Option<Arc<dyn Approver>>,
    /// Where large outputs spill.
    pub spill_dir: Option<PathBuf>,
    /// The attachments root (`<state>/attachments`); a session's files are in
    /// its folder, looked up through `sessions` (`attachments::find`).
    pub attachments: Option<PathBuf>,
    /// Runtime services for tools that need the session (subagents, todos,
    /// skills), downcast by those tools.
    pub services: Option<Arc<dyn Any + Send + Sync>>,
    /// The calling session, then its ancestors (a subagent's commands run
    /// next to its parent's agent block).
    pub sessions: Vec<String>,
    /// Delivers text to the calling session later (background results).
    pub mailbox: Option<Arc<dyn Mailbox>>,
    /// The model can view images (its catalog caps); `read` describes an image as text otherwise.
    pub vision: bool,
}

/// Text for a session after its call returned: injected at its next step,
/// or starting a turn when it is idle.
pub trait Mailbox: Send + Sync {
    fn deliver(&self, text: String);
}

impl ToolCx {
    pub fn new(call_id: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        Self {
            call_id: call_id.into(),
            project_root: cwd.clone(),
            cwd,
            cancel: CancellationToken::new(),
            progress: None,
            approver: None,
            spill_dir: None,
            attachments: None,
            services: None,
            sessions: Vec::new(),
            mailbox: None,
            vision: true,
        }
    }
    pub fn progress(&self, message: Option<String>, output: Option<String>) {
        if let Some(tx) = &self.progress {
            let _ = tx.send(Progress { call_id: self.call_id.clone(), message, output });
        }
    }
}

impl std::fmt::Debug for ToolCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCx").field("call_id", &self.call_id).field("cwd", &self.cwd).finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: Vec<OutputPart>,
    /// The call ran but failed (non-zero exit, no match); shown to the model
    /// as an error result.
    #[serde(default)]
    pub is_error: bool,
    /// Structured data for clients (exit code, paths, diff stats).
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub truncated: Option<Truncation>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { content: vec![OutputPart::Text { text: text.into() }], ..Self::default() }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self { is_error: true, ..Self::text(text) }
    }
    #[must_use]
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }
    /// The text parts joined.
    #[must_use]
    pub fn joined_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                OutputPart::Text { text } => Some(text.clone()),
                OutputPart::Json { value } => Some(value.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputPart {
    Text {
        text: String,
    },
    Json {
        value: Value,
    },
    Image {
        mime: String,
        data_base64: String,
    },
    /// A document (PDF) for providers that read them.
    Document {
        mime: String,
        data_base64: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    File {
        path: PathBuf,
    },
}

/// The model saw a head and tail; the whole output is in `full_path`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Truncation {
    pub original_bytes: u64,
    #[serde(default)]
    pub full_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    InvalidInput,
    Denied,
    NotFound,
    Failed,
    Cancelled,
    Timeout,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
}

impl ToolError {
    pub fn new(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::InvalidInput, message)
    }
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Failed, message)
    }
}

/// A set of tools that can change (MCP servers, silo, extensions).
#[async_trait]
pub trait ToolSource: Send + Sync {
    fn name(&self) -> &str;
    async fn list(&self) -> Result<Vec<Arc<dyn Tool>>, ToolError>;
    /// Fires when `list` would answer differently.
    fn changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
    }
}

/// Tools whose set depends on the project (MCP servers from `.mcp.json`).
/// Listed again before every model request, so changes apply at the next step.
#[async_trait]
pub trait ProjectSources: Send + Sync {
    /// The tools of every healthy source for `project_root`, starting sources
    /// lazily. Failing sources are skipped, not fatal.
    async fn tools(&self, project_root: &std::path::Path) -> Vec<Arc<dyn Tool>>;
    /// Stop every started source (child processes, connections).
    async fn shutdown(&self) {}
}
