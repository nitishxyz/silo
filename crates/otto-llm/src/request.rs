//! Request options (docs/OTTO.md §4.3).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::Item;

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    /// The provider's model id.
    pub model: String,
    /// System prompt blocks, stable first (cache breakpoints go after the
    /// last stable block).
    #[serde(default)]
    pub system: Vec<String>,
    pub items: Vec<Item>,
    #[serde(default)]
    pub tools: Vec<ToolDef>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// `None`: the provider default.
    #[serde(default)]
    pub parallel_tools: Option<bool>,
    #[serde(default)]
    pub max_output: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stop: Vec<String>,
    #[serde(default)]
    pub reasoning: Option<Reasoning>,
    /// Structured output JSON Schema.
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub cache: CacheOptions,
    /// End-user id for abuse monitoring.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub provider: ProviderOptions,
    /// Native compaction (`caps.native_compaction`); otto's own summarizer
    /// is used when this is `None`.
    #[serde(default)]
    pub compaction: Option<Compaction>,
    /// Raw JSON merged last into the provider's request body.
    #[serde(default)]
    pub extra: Option<Value>,
}

/// Provider-side compaction. The result arrives as a [`crate::Part::Compaction`]
/// whose opaque data is replayed only to the same provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Compaction {
    /// Summarize the request's items now instead of answering. The returned
    /// compaction part replaces those items in later requests.
    Now {
        #[serde(default)]
        instructions: Option<String>,
    },
    /// Let the provider compact inside ordinary requests once the input
    /// passes `threshold` tokens; keep the returned part in the history.
    Auto {
        threshold: u64,
        #[serde(default)]
        instructions: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema of the input (function tools); ignored for hosted tools.
    #[serde(default)]
    pub input_schema: Value,
    #[serde(default)]
    pub kind: ToolKind,
    /// Deferred: discoverable through tool search, not in the prefix.
    #[serde(default)]
    pub defer_loading: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolKind {
    #[default]
    Function,
    /// Strict JSON schema function (OpenAI `strict`, Anthropic strict tools).
    Strict,
    /// Freeform input (OpenAI custom tools); `format` is a grammar or `None`
    /// for free text.
    Custom {
        #[serde(default)]
        format: Option<Value>,
    },
    /// A provider-hosted tool: `tool` is the provider's type
    /// (`web_search_20250305`, `web_search`, `code_execution`, …) and
    /// `config` its parameters, merged into the tool object.
    Hosted {
        tool: String,
        #[serde(default)]
        config: Value,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "name", rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    /// Some tool must be called.
    Required,
    Tool(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    /// The effort in `supported` nearest to this one, the higher on a tie;
    /// itself when `supported` is empty (nothing known to clamp to).
    #[must_use]
    pub fn nearest(self, supported: &[Effort]) -> Effort {
        supported.iter().copied().min_by_key(|e| ((*e as i8 - self as i8).unsigned_abs(), std::cmp::Reverse(*e))).unwrap_or(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reasoning {
    Effort(Effort),
    /// A thinking token budget.
    Budget(u32),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CacheOptions {
    /// Disable automatic caching (breakpoints, cache keys).
    #[serde(default)]
    pub disabled: bool,
    /// OpenAI `prompt_cache_key`; usually the session id.
    #[serde(default)]
    pub key: Option<String>,
    /// Anthropic: use the 1h TTL instead of 5m.
    #[serde(default)]
    pub long_ttl: bool,
}

/// Typed per-provider options; each provider reads its own and ignores
/// the rest.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProviderOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic: Option<crate::anthropic::Options>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai: Option<crate::openai::Options>,
}
