//! The one stream event enum for every provider (docs/OTTO.md §4.2).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::Part;

#[cfg(feature = "client")]
pub type EventStream = std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent, crate::error::LlmError>> + Send>>;

/// Deltas are for live display; [`StreamEvent::PartDone`] carries each
/// complete part (with its opaque data) in transcript order. The harness
/// builds the assistant item from `PartDone`s only, so a provider must emit
/// one for every part it wants persisted and replayed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Start {
        #[serde(default)]
        response_id: Option<String>,
        #[serde(default)]
        model: Option<String>,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
        /// A reasoning summary rather than raw reasoning.
        #[serde(default)]
        summary: bool,
    },
    ToolInputStart {
        call_id: String,
        name: String,
    },
    ToolInputDelta {
        call_id: String,
        delta: String,
    },
    /// A hosted tool's status (`searching`, `in_progress`, …).
    HostedProgress {
        call_id: String,
        tool: String,
        status: String,
    },
    /// A complete part. `Part::ToolCall` here means the call is ready to run.
    PartDone {
        part: Part,
    },
    /// Cumulative usage so far; the last one wins.
    Usage(Usage),
    Finish {
        reason: FinishReason,
        #[serde(default)]
        response_id: Option<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    /// Uncached input tokens.
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// Reasoning tokens, included in `output`.
    #[serde(default)]
    pub reasoning: u64,
}

impl Usage {
    #[must_use]
    pub fn total_input(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolUse,
    MaxTokens,
    ContentFilter,
    Refusal,
    /// Anthropic `pause_turn`: a long server tool turn paused; send it back.
    PauseTurn,
    Other(String),
}
