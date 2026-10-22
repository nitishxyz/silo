//! Native Messages wire types. Unknown blocks and extension fields are retained.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Beta(pub String);
impl Beta {
    pub const INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
    pub const INLINE_TOOLS: &str = "inline-tools-2026-09-15";
    pub const OAUTH: &str = "oauth-2025-04-20";
    /// On-demand compaction (`compaction` parameter, signed blocks).
    pub const COMPACT: &str = "compact-2026-09-04";
    /// Threshold compaction (`context_management.edits[compact_20260112]`).
    pub const COMPACT_THRESHOLD: &str = "compact-2026-01-12";
    /// Context editing (`clear_tool_uses_20250919`, `clear_thinking_20251015`).
    pub const CONTEXT_MANAGEMENT: &str = "context-management-2025-06-27";
}
impl From<&str> for Beta {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ContentBlock {
    Known(KnownBlock),
    Other(Value),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnownBlock {
    Text {
        text: String,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    Thinking {
        thinking: String,
        signature: String,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    RedactedThinking {
        data: String,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    Image {
        source: Value,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    Document {
        source: Value,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    WebFetchToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    CodeExecutionToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
    Compaction {
        content: String,
        #[serde(default, flatten)]
        extra: BTreeMap<String, Value>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamEvent {
    Known(KnownEvent),
    Other(Value),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnownEvent {
    MessageStart {
        message: MessagesResponse,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: Value,
        #[serde(default)]
        usage: Value,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiError,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CountTokensResponse {
    pub input_tokens: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Delta {
    Known(KnownDelta),
    Other(Value),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnownDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
    CitationsDelta { citation: Value },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Thinking {
    Enabled {
        budget_tokens: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<String>,
    },
    Adaptive {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<String>,
    },
    Disabled,
    BetweenTools,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Any {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Tool {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    None,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tool {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}
