//! Status reports and browser actions for connected agents.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{BlockId, StatusKind, StatusState};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BlockReportParams {
    pub block_id: BlockId,
    #[serde(default)]
    pub id: String,
    pub state: StatusState,
    #[serde(default)]
    pub kind: Option<StatusKind>,
    #[serde(default)]
    #[schemars(range(min = 0, max = 100))]
    pub progress: Option<u8>,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BlockNotifyParams {
    pub block_id: BlockId,
    #[serde(default)]
    pub title: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserSnapshotParams {
    pub block_id: BlockId,
    #[serde(default)]
    pub within: Option<String>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub boxes: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserFindParams {
    pub block_id: BlockId,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub regex: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserClickParams {
    pub block_id: BlockId,
    pub target: String,
    #[serde(default)]
    pub double: Option<bool>,
    #[serde(default)]
    pub button: Option<MouseButton>,
    #[serde(default)]
    pub include_snapshot: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserHoverParams {
    pub block_id: BlockId,
    pub target: String,
    #[serde(default)]
    pub include_snapshot: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserField {
    pub target: String,
    pub value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserFillParams {
    pub block_id: BlockId,
    pub target: String,
    pub value: String,
    #[serde(default)]
    pub include_snapshot: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserFillFormParams {
    pub block_id: BlockId,
    pub fields: Vec<BrowserField>,
    #[serde(default)]
    pub include_snapshot: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserTypeParams {
    pub block_id: BlockId,
    pub text: String,
    #[serde(default)]
    pub submit: Option<String>,
    #[serde(default)]
    pub include_snapshot: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserPressParams {
    pub block_id: BlockId,
    pub key: String,
    #[serde(default)]
    pub include_snapshot: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserScrollParams {
    pub block_id: BlockId,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub dx: Option<f64>,
    #[serde(default)]
    pub dy: Option<f64>,
    #[serde(default)]
    pub include_snapshot: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserWaitForParams {
    pub block_id: BlockId,
    #[serde(default)]
    pub text: Option<Vec<String>>,
    #[serde(default)]
    pub gone: Option<bool>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserDialogParams {
    pub block_id: BlockId,
    pub accept: bool,
    #[serde(default)]
    pub prompt_text: Option<String>,
    #[serde(default)]
    pub include_snapshot: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserEvalParams {
    pub block_id: BlockId,
    pub function: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserConsoleParams {
    pub block_id: BlockId,
    #[serde(default)]
    pub since: Option<u64>,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserNetworkParams {
    pub block_id: BlockId,
    #[serde(default)]
    pub since: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub failed_only: Option<bool>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotFormat {
    Png,
    Jpeg,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserScreenshotParams {
    pub block_id: BlockId,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub full_page: Option<bool>,
    #[serde(default)]
    pub format: Option<ScreenshotFormat>,
    #[serde(default)]
    pub quality: Option<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserSnapshotResult {
    pub url: String,
    pub title: String,
    pub snapshot: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserFindResult {
    pub matches: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserDialog {
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserAction {
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog: Option<BrowserDialog>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigating_to: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserWaitForResult {
    pub found: bool,
    pub url: String,
    pub title: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserEvalResult {
    pub value: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserConsoleMessage {
    pub id: u64,
    pub level: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserConsoleResult {
    pub messages: Vec<BrowserConsoleMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserNetworkRequest {
    pub id: u64,
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
    pub at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserNetworkResult {
    pub requests: Vec<BrowserNetworkRequest>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrowserScreenshotResult {
    pub mime: String,
    pub data_base64: String,
    pub width: u32,
    pub height: u32,
}
