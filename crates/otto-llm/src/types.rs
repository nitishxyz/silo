//! The canonical transcript (docs/OTTO.md §4.2).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type ItemId = String;

/// A provider id: `"anthropic"`, `"openai"`, `"google"`, …
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub const ANTHROPIC: &str = "anthropic";
    pub const OPENAI: &str = "openai";
    pub const GOOGLE: &str = "google";
    pub const XAI: &str = "xai";

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Mid-conversation instructions ([`Part::System`], [`Part::ToolsChanged`]).
    System,
    User,
    Assistant,
    /// Tool results ([`Part::ToolResult`]); providers map it to their shape
    /// (an Anthropic user message, OpenAI `function_call_output` items).
    Tool,
}

/// One transcript entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Item {
    pub id: ItemId,
    pub role: Role,
    pub parts: Vec<Part>,
    /// Which provider/model produced it (assistant items).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// `@` mentions in a user item's text (`Item::text` byte ranges); the
    /// model gets a note naming them, not these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Mention>,
    /// Set only on a client's copy that was clipped to fit the wire: the
    /// item's serialized size in bytes before clipping. Stored items and what
    /// providers see never carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipped: Option<u64>,
}

impl Item {
    pub fn new(id: impl Into<ItemId>, role: Role, parts: Vec<Part>) -> Self {
        Self { id: id.into(), role, parts, origin: None, mentions: Vec::new(), clipped: None }
    }
    pub fn user_text(id: impl Into<ItemId>, text: impl Into<String>) -> Self {
        Self::new(id, Role::User, vec![Part::text(text)])
    }
    /// The concatenated text parts.
    #[must_use]
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
    /// The tool calls in this item, in order.
    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.parts.iter().filter_map(|p| match p {
            Part::ToolCall { call_id, name, input, .. } => Some((call_id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MentionKind {
    Agent,
    /// A project file or folder (`target` ends in `/`).
    File,
}

/// An `@` mention in a user item: `@target` at `range` of [`Item::text`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Mention {
    pub kind: MentionKind,
    /// An agent name, or a path relative to the session's folder.
    pub target: String,
    pub range: [usize; 2],
    /// No such agent or path when it was sent (left out of the model's note).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Origin {
    pub provider: ProviderId,
    pub model: String,
    /// Provider response id (`previous_response_id` cache, §7.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

/// Provider-owned data replayed byte-exact to the same provider/model:
/// thinking signatures, redacted thinking, `encrypted_content`, Gemini
/// thought signatures, compaction blocks, server tool payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Opaque {
    pub provider: ProviderId,
    pub model: String,
    pub data: Value,
}

impl Opaque {
    /// Whether it may be replayed to `provider`/`model` (same provider; the
    /// provider decides how strict the model match must be).
    #[must_use]
    pub fn matches_provider(&self, provider: &str) -> bool {
        self.provider.as_str() == provider
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    /// JSON-schema function tool.
    #[default]
    Function,
    /// Freeform (OpenAI custom tool): `input` is a JSON string.
    Custom,
    /// Executed by the provider (web search, code execution, …).
    Hosted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        citations: Vec<Citation>,
    },
    Image(Media),
    Document(Media),
    Audio(Media),
    Reasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opaque: Option<Opaque>,
    },
    ToolCall {
        call_id: String,
        name: String,
        input: Value,
        #[serde(default)]
        kind: CallKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opaque: Option<Opaque>,
    },
    ToolResult {
        call_id: String,
        content: Vec<ResultPart>,
        #[serde(default)]
        is_error: bool,
    },
    /// A provider-executed tool's result (`web_search_tool_result`, …).
    HostedResult {
        call_id: String,
        tool: String,
        payload: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opaque: Option<Opaque>,
    },
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opaque: Option<Opaque>,
    },
    /// Mid-conversation instructions.
    System {
        text: String,
    },
    ToolsChanged {
        #[serde(default)]
        added: Vec<String>,
        #[serde(default)]
        removed: Vec<String>,
    },
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into(), citations: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Media {
    pub mime: String,
    pub source: MediaSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Media {
    /// What a provider that can't read this document gets instead.
    #[must_use]
    pub fn unsupported(&self, provider: &str) -> String {
        format!(
            "[{} ({}) not sent: {provider} doesn't accept documents here. Extract its text instead, e.g. `pdftotext file.pdf -` in the shell.]",
            self.name.as_deref().unwrap_or("document"),
            self.mime
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaSource {
    Base64 {
        data: String,
    },
    Url {
        url: String,
    },
    /// A provider Files API id.
    File {
        provider: ProviderId,
        id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Citation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_text: Option<String>,
    /// The provider's citation object, replayed as-is to that provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Opaque>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultPart {
    Text { text: String },
    Image(Media),
    Document(Media),
}

impl ResultPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}
