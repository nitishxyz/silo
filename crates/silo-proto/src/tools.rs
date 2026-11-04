//! Tools through the server (`tools.*`, `docs/AGENTS.md` §10): Silo's own
//! tools, grouped by the kind of block they work on, and tools clients
//! provide for a block or the server. Agents list and call both alike; the
//! server runs its own as the caller and forwards the rest to their
//! provider as `tools.request`, answered with `tools.reply`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    control::{Caller, RpcError},
    model::{BlockId, BlockKind, ClientId},
};

/// Longest tool name.
pub const NAME_MAX: usize = 64;
/// Most tools one provider offers for one scope.
pub const TOOLS_MAX: usize = 128;
/// Longest description.
pub const DESCRIPTION_MAX: usize = 4000;
/// Largest input schema, serialized.
pub const SCHEMA_MAX: usize = 64 * 1024;
/// How long a call waits for its provider by default.
pub const CALL_TIMEOUT_MS: u64 = 120_000;
/// Longest wait a call may ask for.
pub const CALL_TIMEOUT_MAX_MS: u64 = 600_000;

/// A tool as its provider describes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProvidedTool {
    /// `[a-z][a-z0-9_]*`, at most [`NAME_MAX`] bytes: safe for MCP and
    /// every model provider.
    pub name: String,
    pub description: String,
    /// JSON Schema of the input object.
    #[serde(default = "object_schema")]
    pub input_schema: Value,
    /// It only reads: callers need view rights (else control).
    #[serde(default)]
    pub read_only: bool,
    /// A human approves every call that is not from the Silo app.
    #[serde(default)]
    pub approval: bool,
}

fn object_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

impl ProvidedTool {
    /// Why this tool cannot be offered, if it cannot.
    ///
    /// # Errors
    /// A bad name, an overlong description or schema, or a schema that is
    /// not an object's.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_name(&self.name) {
            return Err(format!("tool name {:?} must be [a-z][a-z0-9_]* and at most {NAME_MAX} bytes", self.name));
        }
        if self.description.len() > DESCRIPTION_MAX {
            return Err(format!("{}: the description is longer than {DESCRIPTION_MAX} bytes", self.name));
        }
        if self.input_schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(format!("{}: input_schema must be an object schema (\"type\": \"object\")", self.name));
        }
        if serde_json::to_string(&self.input_schema).map_or(true, |s| s.len() > SCHEMA_MAX) {
            return Err(format!("{}: input_schema is larger than {SCHEMA_MAX} bytes", self.name));
        }
        Ok(())
    }
}

/// Whether `name` is a valid tool name.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= NAME_MAX
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `tools.provide`: replace what the caller provides for `block_id` (or,
/// without one, for the whole server) with `tools`; empty withdraws them.
/// Block tools go when the block closes, all of them when the caller
/// disconnects.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolsProvideParams {
    #[serde(default)]
    pub block_id: Option<BlockId>,
    pub tools: Vec<ProvidedTool>,
}

/// A tool, as `tools.list` shows it: Silo's own (provider `silo`, id 0)
/// or one a client provides.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolEntry {
    #[serde(flatten)]
    pub tool: ProvidedTool,
    /// The block it belongs to; `None`: the server's.
    #[serde(default)]
    pub block_id: Option<BlockId>,
    /// The kind of block it works on (Silo's own `browser_*`: browser), or
    /// its block's kind; `None`: none in particular.
    #[serde(default)]
    pub block_kind: Option<BlockKind>,
    /// The providing client's label (`silo` for Silo's own).
    pub provider: String,
    /// 0 for Silo's own.
    pub provider_id: ClientId,
}

/// [`ToolEntry::provider`] of Silo's own tools.
pub const SILO_PROVIDER: &str = "silo";

/// `tools.list` result.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolList {
    pub tools: Vec<ToolEntry>,
}

/// `tools.call`: run tool `name` (of `block_id` when several blocks
/// provide it) with `input`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolsCallParams {
    pub name: String,
    #[serde(default)]
    pub block_id: Option<BlockId>,
    #[serde(default = "empty_object")]
    pub input: Value,
    /// How long to wait for the provider (default
    /// [`CALL_TIMEOUT_MS`], at most [`CALL_TIMEOUT_MAX_MS`]).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// One part of a tool's result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolContent {
    Text { text: String },
    Image { mime: String, data_base64: String },
    Json { value: Value },
}

/// What a tool returns (`tools.call`'s result, `tools.reply`'s `result`).
/// A reply whose result is not of this shape is passed on as one
/// [`ToolContent::Json`] part.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolOutcome {
    pub content: Vec<ToolContent>,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolOutcome {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self { content: vec![ToolContent::Text { text: text.into() }], is_error: false }
    }

    #[must_use]
    pub fn json(value: Value) -> Self {
        Self { content: vec![ToolContent::Json { value }], is_error: false }
    }

    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self { content: vec![ToolContent::Text { text: message.into() }], is_error: true }
    }

    /// A provider's raw result: this shape, or anything else as JSON.
    #[must_use]
    pub fn from_value(value: Value) -> Self {
        match serde_json::from_value::<Self>(value.clone()) {
            Ok(outcome) if value.get("content").is_some_and(Value::is_array) => outcome,
            _ => Self::json(value),
        }
    }
}

/// A forwarded call (`tools.request` event), sent to its provider only.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolRequest {
    pub request_id: String,
    pub name: String,
    #[serde(default)]
    pub block_id: Option<BlockId>,
    pub input: Value,
    pub caller: Caller,
}

/// `tools.reply`: a provider's answer to a `tools.request`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolsReplyParams {
    pub request_id: String,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ProvidedTool {
        ProvidedTool { name: name.into(), description: "d".into(), input_schema: object_schema(), read_only: true, approval: false }
    }

    #[test]
    fn names() {
        assert!(valid_name("agent_save"));
        assert!(valid_name("a1"));
        for bad in ["", "Agent", "1a", "a-b", "a.b", "a b", &"a".repeat(NAME_MAX + 1)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn validation() {
        assert!(tool("ok").validate().is_ok());
        assert!(tool("Bad").validate().is_err());
        let mut array = tool("ok");
        array.input_schema = serde_json::json!({"type": "array"});
        assert!(array.validate().is_err());
    }

    #[test]
    fn outcomes_from_values() {
        let shaped = serde_json::json!({"content": [{"type": "text", "text": "hi"}]});
        assert_eq!(ToolOutcome::from_value(shaped), ToolOutcome::text("hi"));
        let raw = serde_json::json!({"agents": []});
        assert_eq!(ToolOutcome::from_value(raw.clone()), ToolOutcome::json(raw));
        // A default-looking object without content is data, not an empty result.
        assert_eq!(ToolOutcome::from_value(serde_json::json!({})), ToolOutcome::json(serde_json::json!({})));
    }

    #[test]
    fn a_tool_without_schema_takes_an_object() {
        let tool: ProvidedTool = serde_json::from_value(serde_json::json!({"name": "x", "description": "y"})).unwrap();
        assert!(tool.validate().is_ok());
    }
}
