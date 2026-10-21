//! Native Responses wire types. Unknown item kinds retain their complete JSON.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Vec<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default)]
    pub store: bool,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, flatten)]
    pub options: Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Item {
    Known(KnownItem),
    Other(Value),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnownItem {
    Message {
        role: String,
        content: Value,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    FunctionCallOutput {
        call_id: String,
        output: Value,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    CustomToolCall {
        call_id: String,
        name: String,
        input: String,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    CustomToolCallOutput {
        call_id: String,
        output: Value,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    Reasoning {
        id: String,
        summary: Vec<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    WebSearchCall {
        id: String,
        status: String,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
}
pub type InputItem = Item;
pub type OutputItem = Item;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub status: String,
    #[serde(default)]
    pub output: Vec<Item>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub input_tokens_details: Value,
    #[serde(default)]
    pub output_tokens_details: Value,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub sequence_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Response>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}
