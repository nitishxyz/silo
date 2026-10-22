//! The generic model interface the harness calls (docs/OTTO.md §4.1, §4.4).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[cfg(feature = "client")]
use tokio_util::sync::CancellationToken;

use crate::types::ProviderId;
#[cfg(feature = "client")]
use crate::{error::LlmError, event::EventStream, request::Request};

#[cfg(feature = "client")]
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    fn info(&self) -> &ModelInfo;
    /// Start one model call. Cancelling `cx.cancel` ends the stream with an
    /// `ErrorClass::Cancelled` error and closes the connection.
    async fn stream(&self, req: Request, cx: CallCx) -> Result<EventStream, LlmError>;
    async fn count_tokens(&self, _req: &Request) -> Result<Option<u64>, LlmError> {
        Ok(None)
    }
    /// Live provider session (OpenAI Responses WebSocket), when this model
    /// streams over one. Only such models can be steered mid-response.
    fn session(&self) -> Option<&dyn ModelSession> {
        None
    }
}

/// Provider-side session state (O3); a cache, never the truth (§7.3).
#[cfg(feature = "client")]
#[async_trait::async_trait]
pub trait ModelSession: Send + Sync {
    /// Queue user input into the response that is streaming right now.
    /// `Ok(false)`: no stream can take it; deliver it at the next step
    /// boundary instead. `Ok(true)` hands it to the stream, which reports it in
    /// [`crate::StreamEvent::Steered`] once the provider commits it. Input the
    /// stream never reports was not applied and still belongs to the caller.
    async fn steer(&self, id: &str, input: Vec<crate::types::Part>) -> Result<bool, LlmError>;
}

#[cfg(feature = "client")]
#[derive(Clone, Debug, Default)]
pub struct CallCx {
    pub cancel: CancellationToken,
    /// Session id, for cache keys and provider state.
    pub session: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelInfo {
    pub id: String,
    pub provider: ProviderId,
    #[serde(default)]
    pub name: String,
    pub caps: Caps,
    #[serde(default)]
    pub pricing: Option<Pricing>,
}

/// USD per million tokens.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolFamily {
    /// `str_replace` editing, Claude's bash shape.
    Anthropic,
    /// `apply_patch` (freeform), Codex shell shape.
    Openai,
    Gemini,
    #[default]
    Generic,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[allow(clippy::struct_excessive_bools)]
pub struct Caps {
    pub reasoning: bool,
    /// Reasoning must/can be replayed (signatures, encrypted content).
    pub reasoning_replay: bool,
    pub vision: bool,
    pub pdf: bool,
    pub audio_in: bool,
    pub parallel_tools: bool,
    pub strict_tools: bool,
    pub custom_tools: bool,
    pub async_tools: bool,
    #[serde(default)]
    pub hosted_tools: Vec<String>,
    pub deferred_tools: bool,
    pub inline_tool_changes: bool,
    pub mid_system: bool,
    pub steering: bool,
    pub native_compaction: bool,
    pub server_state: bool,
    pub prompt_cache: bool,
    pub context_window: u64,
    pub max_output: u64,
    pub tool_family: ToolFamily,
    /// Provider-defined client tools this model and auth mode accept
    /// (`text_editor_20250728`, `bash_20250124`, `apply_patch`, `shell`).
    #[serde(default)]
    pub native_tools: Vec<String>,
}
