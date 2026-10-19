//! JSON control plane: requests, responses and sequenced events.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{
    Axis, BlockId, BlockInfo, BlockKind, ClientId, FloatRect, HistoryEntry, SessionId, SessionSummary, SessionView, TermSize, WindowId,
};

/// First frame on a control stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientHello {
    /// The client's [`crate::BUILD`]; the server rejects any other build.
    pub build: String,
    /// Human-readable client name, e.g. "Silo 0.1.0 (macOS)" or "silo cli".
    pub client_name: String,
    /// Shared secret proving the client may adopt this server (local only).
    #[serde(default)]
    pub token: Option<String>,
    /// Monitoring clients (the menu-bar agent) do not keep an auto-started
    /// server from exiting when idle.
    #[serde(default)]
    pub passive: bool,
    /// Remote only: a pairing invite (`silo1…` PairTicket string from
    /// `pair.create`) redeemed on first contact. Paired devices omit it; they
    /// are authenticated by their endpoint id.
    #[serde(default)]
    pub invite: Option<String>,
}

/// Reply to [`ClientHello`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerHello {
    /// The server's [`crate::BUILD`].
    pub build: String,
    pub client_id: ClientId,
    pub server: ServerInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServerInfo {
    pub version: String,
    pub pid: u32,
    pub server_id: String,
    pub started_at: u64,
    /// The server's iroh EndpointId.
    pub endpoint_id: String,
    /// Why remote (iroh) access is unavailable; `None` while it serves.
    #[serde(default)]
    pub remote_error: Option<String>,
    /// The server's current reachable address: iroh `EndpointAddr` (id,
    /// home relay URL, direct addresses) in its serde JSON form, as of this
    /// reply. Clients refresh stored host addresses from it. `None` without
    /// remote access.
    #[serde(default)]
    pub endpoint_addr: Option<Value>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OwnMode {
    /// Become the owner, displacing any current owner.
    #[default]
    Take,
    /// Become the owner only if the block has none.
    Request,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct SpawnSpec {
    #[serde(default)]
    pub cwd: Option<String>,
    /// Program and arguments; empty means the user's login shell.
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub size: Option<TermSize>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum Method {
    #[serde(rename = "server.info")]
    ServerInfo {},
    #[serde(rename = "server.shutdown")]
    ServerShutdown {},
    #[serde(rename = "api.list")]
    ApiList {},
    /// Params and result JSON Schemas and an example request.
    #[serde(rename = "api.describe")]
    ApiDescribe { method: String },
    /// Liveness probe; answers only once the server's state is reachable.
    #[serde(rename = "ping")]
    Ping {},
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
        #[serde(default)]
        kind: BlockKind,
        /// Browser blocks: the start URL (default `about:blank`).
        #[serde(default)]
        url: Option<String>,
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
        #[serde(default)]
        kind: BlockKind,
        /// Browser blocks: the start URL (default `about:blank`).
        #[serde(default)]
        url: Option<String>,
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
        #[serde(default)]
        kind: BlockKind,
        /// Browser blocks: the start URL (default `about:blank`).
        #[serde(default)]
        url: Option<String>,
    },
    #[serde(rename = "block.close")]
    BlockClose { block_id: BlockId },
    #[serde(rename = "block.focus")]
    BlockFocus { block_id: BlockId },
    #[serde(rename = "block.zoom")]
    BlockZoom { block_id: BlockId, zoomed: bool },
    /// Exchange two blocks' positions in their (shared) window layout.
    #[serde(rename = "block.swap")]
    BlockSwap { block_id: BlockId, other_block_id: BlockId },
    /// Move a tiled block out of the layout into the window's floats, on
    /// top (`rect` defaults to centred 60% × 60%).
    #[serde(rename = "block.float")]
    BlockFloat { block_id: BlockId, rect: Option<FloatRect> },
    /// Set a float's `rect` and raise it, or `dock` it into the layout.
    /// Exactly one of `rect` and `dock`.
    #[serde(rename = "block.move")]
    BlockMove { block_id: BlockId, rect: Option<FloatRect>, dock: Option<Dock> },
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

    #[serde(rename = "browser.navigate")]
    BrowserNavigate { block_id: BlockId, url: String },
    #[serde(rename = "browser.back")]
    BrowserBack { block_id: BlockId },
    #[serde(rename = "browser.forward")]
    BrowserForward { block_id: BlockId },
    #[serde(rename = "browser.reload")]
    BrowserReload {
        block_id: BlockId,
        #[serde(default)]
        ignore_cache: bool,
    },
    #[serde(rename = "browser.zoom")]
    BrowserZoom { block_id: BlockId, zoom: f32 },
    /// Owner only: the engine's committed state.
    #[serde(rename = "browser.report")]
    BrowserReport {
        block_id: BlockId,
        url: String,
        title: String,
        #[serde(default)]
        favicon: Option<String>,
        loading: bool,
        history: Vec<HistoryEntry>,
        index: usize,
    },

    /// Ask attached UI clients (the app) to show a session.
    #[serde(rename = "ui.open_session")]
    UiOpenSession { session_id: SessionId },

    /// Owner only: a single-use pairing invite valid for `ttl_secs`.
    #[serde(rename = "pair.create")]
    PairCreate { ttl_secs: u64 },
    /// Owner only: paired devices.
    #[serde(rename = "pair.list")]
    PairList {},
    /// Owner only: forget a device and close its connections.
    #[serde(rename = "pair.revoke")]
    PairRevoke { id: String },
}

/// Method names and one-line descriptions, for `api.list`.
pub const METHODS: &[(&str, &str)] = &[
    ("server.info", "Server version, Ghostty revision and identity."),
    ("server.shutdown", "Stop the server and every process it runs."),
    ("api.list", "List control methods."),
    ("api.describe", "A method's params and result JSON Schemas and an example request."),
    ("ping", "Liveness probe; answers once the server's state is reachable."),
    ("events.subscribe", "Start receiving sequenced events on this connection."),
    ("session.list", "List sessions."),
    ("session.create", "Create a session with one window running a shell, command or browser."),
    ("session.view", "Full session tree and block metadata."),
    ("session.rename", "Rename a session."),
    ("session.close", "Close a session and terminate its processes."),
    ("window.create", "Add a window (tab) with a terminal or browser block to a session."),
    ("window.select", "Make a window the session's active window."),
    ("window.close", "Close a window and its blocks."),
    ("window.rename", "Set or clear a window's name."),
    ("window.move", "Move a window to a new index."),
    ("block.split", "Split a block, starting a new terminal or browser next to it."),
    ("block.close", "Close a block and terminate its process."),
    ("block.focus", "Focus a block within its window."),
    ("block.zoom", "Zoom or unzoom a block within its window."),
    ("block.swap", "Swap two blocks' positions within the same window layout."),
    ("block.float", "Float a tiled block above its window's layout."),
    ("block.move", "Move and raise a float (rect), or dock it next to a tiled block (dock)."),
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
    ("browser.navigate", "Load a URL in a browser block (bare hosts get a scheme)."),
    ("browser.back", "Go back in a browser block's history."),
    ("browser.forward", "Go forward in a browser block's history."),
    ("browser.reload", "Reload a browser block, optionally bypassing the cache."),
    ("browser.zoom", "Set a browser block's zoom level (0 = 100%)."),
    ("browser.report", "Report a browser block's engine state (owner only)."),
    ("ui.open_session", "Ask subscribed UI clients to show a session."),
    ("pair.create", "Create a single-use pairing invite (owner only)."),
    ("pair.list", "List paired devices (owner only)."),
    ("pair.revoke", "Forget a paired device and close its connections (owner only)."),
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
        let method = serde_json::from_value(serde_json::json!({ "method": raw.method, "params": params })).map_err(|e| e.to_string())?;
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
    /// Hello rejection: the client is a different build than the server.
    pub fn different_build(server: &str, client: &str) -> Self {
        Self::new(Self::DIFFERENT_BUILD, format!("different build: server {server}, client {client}"))
    }
    /// Code of [`RpcError::different_build`].
    pub const DIFFERENT_BUILD: &str = "different_build";
    /// Code of [`RpcError::unauthorized`].
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// Code of [`RpcError::forbidden`].
    pub const FORBIDDEN: &str = "forbidden";
    /// Hello rejection: an unknown device without a valid invite.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(Self::UNAUTHORIZED, message)
    }
    /// The authenticated peer may not call this method.
    pub fn forbidden(method: &str) -> Self {
        Self::new(Self::FORBIDDEN, format!("{method} is not permitted for this client"))
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
        event: Box<Event>,
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
    /// A browser command for the owner's engine; viewers follow it too.
    #[serde(rename = "browser.command")]
    BrowserCommand { block_id: BlockId, command: BrowserCommand },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BrowserCommand {
    Navigate { url: String },
    Back,
    Forward,
    Reload { ignore_cache: bool },
    Zoom { zoom: f32 },
}

// Typed results.

/// `block.move` docking target: split `target` along `axis`, the docked
/// block second.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Dock {
    pub target: BlockId,
    pub axis: Axis,
}

/// The `{}` result of methods that only acknowledge.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// Element of `api.list`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ApiMethod {
    pub name: String,
    pub description: String,
}

/// Result of `api.describe`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MethodDescription {
    pub description: String,
    pub params_schema: Value,
    pub result_schema: Value,
    /// A request, `{method, params}`, whose params match `params_schema`.
    pub example: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Created {
    pub session_id: SessionId,
    pub window_id: WindowId,
    pub block_id: BlockId,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DataTicket {
    /// Hex-encoded 16-byte single-use ticket.
    pub ticket: String,
    pub expires_in_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DesiredSize {
    pub client_id: ClientId,
    pub client_name: String,
    pub size: TermSize,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SizeInfo {
    pub size: TermSize,
    pub owner: Option<ClientId>,
    pub desired: Vec<DesiredSize>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Capture {
    pub text: String,
    pub size: TermSize,
}

/// Result of `pair.create`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PairInvite {
    /// Encoded `PairTicket` (`silo1…`).
    pub ticket: String,
    pub expires_in_secs: u64,
}

/// Element of `pair.list`'s `devices`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct PairedDevice {
    /// The device's iroh EndpointId.
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub paired_at: u64,
    /// Unix seconds of the last authenticated control hello.
    pub last_seen: u64,
}

/// Result of `pair.list`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PairedDevices {
    pub devices: Vec<PairedDevice>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_messages_round_trip() {
        let method =
            Method::BlockSplit { block_id: "b".into(), axis: Axis::Vertical, spawn: SpawnSpec::default(), kind: BlockKind::Browser, url: None };
        let request = Request { id: 3, method };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"method\":\"block.split\""), "{json}");
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 3);
        assert!(matches!(back.method, Method::BlockSplit { kind: BlockKind::Browser, .. }), "{json}");
        let json = r#"{"id":1,"method":"session.list"}"#;
        assert!(matches!(serde_json::from_str::<Request>(json).unwrap().method, Method::SessionList {}));

        let response: ServerMessage = serde_json::from_str(r#"{"id":4,"result":{"x":1}}"#).unwrap();
        assert!(matches!(response, ServerMessage::Response { id: 4, .. }));
        let event = ServerMessage::Event { seq: 9, event: Box::new(Event::BlockBell { block_id: "b".into() }) };
        let json = serde_json::to_string(&event).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ServerMessage::Event { seq: 9, event } if matches!(*event, Event::BlockBell { .. })), "{json}");
    }
}
