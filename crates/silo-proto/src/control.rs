//! JSON control plane: requests, responses and sequenced events.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Axis, BlockId, BlockInfo, ClientId, SessionId, SessionSummary, SessionView, TermSize, WindowId};

/// First frame on a control stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol: u32,
    /// Human-readable client name, e.g. "Silo 0.1.0 (macOS)" or "silo cli".
    pub client_name: String,
    /// Shared secret proving the client may adopt this server (local only).
    #[serde(default)]
    pub token: Option<String>,
    /// Monitoring clients (the menu-bar agent) do not keep an auto-started
    /// server from exiting when idle.
    #[serde(default)]
    pub passive: bool,
}

/// Reply to [`ClientHello`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol: u32,
    pub client_id: ClientId,
    pub server: ServerInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub ghostty_rev: String,
    pub snapshot_version: u16,
    pub pid: u32,
    pub server_id: String,
    pub started_at: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwnMode {
    /// Become the owner, displacing any current owner.
    #[default]
    Take,
    /// Become the owner only if the block has none.
    Request,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SpawnSpec {
    #[serde(default)]
    pub cwd: Option<String>,
    /// Program and arguments; empty means the user's login shell.
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub size: Option<TermSize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Method {
    #[serde(rename = "server.info")]
    ServerInfo {},
    #[serde(rename = "server.shutdown")]
    ServerShutdown {},
    #[serde(rename = "api.list")]
    ApiList {},
    #[serde(rename = "events.subscribe")]
    EventsSubscribe {},

    #[serde(rename = "session.list")]
    SessionList {},
    #[serde(rename = "session.create")]
    SessionCreate {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        spawn: SpawnSpec,
    },
    #[serde(rename = "session.view")]
    SessionView { session_id: SessionId },
    #[serde(rename = "session.rename")]
    SessionRename { session_id: SessionId, name: String },
    #[serde(rename = "session.close")]
    SessionClose { session_id: SessionId },

    #[serde(rename = "window.create")]
    WindowCreate {
        session_id: SessionId,
        #[serde(default)]
        spawn: SpawnSpec,
    },
    #[serde(rename = "window.select")]
    WindowSelect { session_id: SessionId, window_id: WindowId },
    #[serde(rename = "window.close")]
    WindowClose { window_id: WindowId },
    #[serde(rename = "window.rename")]
    WindowRename { window_id: WindowId, name: Option<String> },
    #[serde(rename = "window.move")]
    WindowMove { window_id: WindowId, index: usize },

    #[serde(rename = "block.split")]
    BlockSplit {
        block_id: BlockId,
        axis: Axis,
        #[serde(default)]
        spawn: SpawnSpec,
    },
    #[serde(rename = "block.close")]
    BlockClose { block_id: BlockId },
    #[serde(rename = "block.focus")]
    BlockFocus { block_id: BlockId },
    #[serde(rename = "block.zoom")]
    BlockZoom { block_id: BlockId, zoomed: bool },
    #[serde(rename = "layout.ratio")]
    LayoutRatio { window_id: WindowId, path: Vec<usize>, ratio: f32 },

    #[serde(rename = "terminal.send")]
    TerminalSend { block_id: BlockId, text: String },
    #[serde(rename = "terminal.capture")]
    TerminalCapture {
        block_id: BlockId,
        /// Include scrollback, not just the visible screen.
        #[serde(default)]
        history: bool,
    },
    #[serde(rename = "terminal.size")]
    TerminalSize { block_id: BlockId },
    #[serde(rename = "terminal.resize")]
    TerminalResize { block_id: BlockId, size: TermSize },
    /// Clear the screen and scrollback for every client.
    #[serde(rename = "terminal.clear")]
    TerminalClear { block_id: BlockId },
    /// Full terminal reset (RIS) for every client.
    #[serde(rename = "terminal.reset")]
    TerminalReset { block_id: BlockId },

    #[serde(rename = "session.own_block")]
    OwnBlock {
        block_ids: Vec<BlockId>,
        #[serde(default)]
        mode: OwnMode,
    },
    #[serde(rename = "session.disown_block")]
    DisownBlock { block_ids: Vec<BlockId> },

    #[serde(rename = "data.connect")]
    DataConnect { block_id: BlockId },

    /// Ask attached UI clients (the app) to show a session.
    #[serde(rename = "ui.open_session")]
    UiOpenSession { session_id: SessionId },
}

/// Method names and one-line descriptions, for `api.list`.
pub const METHODS: &[(&str, &str)] = &[
    ("server.info", "Server version, Ghostty revision and identity."),
    ("server.shutdown", "Stop the server and every process it runs."),
    ("api.list", "List control methods."),
    ("events.subscribe", "Start receiving sequenced events on this connection."),
    ("session.list", "List sessions."),
    ("session.create", "Create a session with one window running a shell or command."),
    ("session.view", "Full session tree and block metadata."),
    ("session.rename", "Rename a session."),
    ("session.close", "Close a session and terminate its processes."),
    ("window.create", "Add a window (tab) to a session."),
    ("window.select", "Make a window the session's active window."),
    ("window.close", "Close a window and its blocks."),
    ("window.rename", "Set or clear a window's name."),
    ("window.move", "Move a window to a new index."),
    ("block.split", "Split a block, starting a new terminal next to it."),
    ("block.close", "Close a block and terminate its process."),
    ("block.focus", "Focus a block within its window."),
    ("block.zoom", "Zoom or unzoom a block within its window."),
    ("layout.ratio", "Change the ratio of a split in a window layout."),
    ("terminal.send", "Write text to a terminal as if typed."),
    ("terminal.capture", "Plain-text contents of a terminal."),
    ("terminal.size", "Applied size, owner and every client's desired size."),
    ("terminal.resize", "Report this client's desired size; applied if owner or unowned."),
    ("terminal.clear", "Clear screen and scrollback, spliced at a parser-ground boundary."),
    ("terminal.reset", "Reset the terminal (RIS), spliced at a parser-ground boundary."),
    ("session.own_block", "Take or request size/theme ownership of blocks."),
    ("session.disown_block", "Release ownership or candidacy of blocks."),
    ("data.connect", "Single-use ticket for a block data stream."),
    ("ui.open_session", "Ask subscribed UI clients to show a session."),
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "RawRequest")]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub method: Method,
}

/// Wire form that lets `params` be omitted for parameterless methods.
#[derive(Deserialize)]
struct RawRequest {
    id: u64,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

impl TryFrom<RawRequest> for Request {
    type Error = String;

    fn try_from(raw: RawRequest) -> Result<Self, Self::Error> {
        let params = raw.params.unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        let method = serde_json::from_value(serde_json::json!({ "method": raw.method, "params": params }))
            .map_err(|e| e.to_string())?;
        Ok(Self { id: raw.id, method })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

impl RpcError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Self::new("not_found", format!("{what} not found"))
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }
    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new("internal", message.to_string())
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
    Event {
        seq: u64,
        #[serde(flatten)]
        event: Event,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum Event {
    #[serde(rename = "sessions.changed")]
    SessionsChanged { sessions: Vec<SessionSummary> },
    #[serde(rename = "session.changed")]
    SessionChanged { view: SessionView },
    #[serde(rename = "session.closed")]
    SessionClosed { session_id: SessionId },
    #[serde(rename = "block.changed")]
    BlockChanged { block: BlockInfo },
    #[serde(rename = "block.bell")]
    BlockBell { block_id: BlockId },
    #[serde(rename = "ui.open_session")]
    OpenSession { session_id: SessionId },
}

// Typed results.

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Created {
    pub session_id: SessionId,
    pub window_id: WindowId,
    pub block_id: BlockId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataTicket {
    /// Hex-encoded 16-byte single-use ticket.
    pub ticket: String,
    pub expires_in_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesiredSize {
    pub client_id: ClientId,
    pub client_name: String,
    pub size: TermSize,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SizeInfo {
    pub size: TermSize,
    pub owner: Option<ClientId>,
    pub desired: Vec<DesiredSize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capture {
    pub text: String,
    pub size: TermSize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_messages_round_trip() {
        let request = Request { id: 3, method: Method::BlockSplit { block_id: "b".into(), axis: Axis::Vertical, spawn: SpawnSpec::default() } };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"method\":\"block.split\""), "{json}");
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 3);
        let json = r#"{"id":1,"method":"session.list"}"#;
        assert!(matches!(serde_json::from_str::<Request>(json).unwrap().method, Method::SessionList {}));

        let response: ServerMessage = serde_json::from_str(r#"{"id":4,"result":{"x":1}}"#).unwrap();
        assert!(matches!(response, ServerMessage::Response { id: 4, .. }));
        let event = ServerMessage::Event { seq: 9, event: Event::BlockBell { block_id: "b".into() } };
        let json = serde_json::to_string(&event).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ServerMessage::Event { seq: 9, event: Event::BlockBell { .. } }), "{json}");
    }
}
