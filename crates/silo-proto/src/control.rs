//! JSON control plane: requests, responses and sequenced events.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::agent::*;
pub use crate::changes::*;
pub use crate::files::*;
pub use crate::permissions::{PermissionId, PermissionInfo, PermissionStatus, Permissions};
pub use crate::search::*;

use crate::{
    access::{Grant, Granted, Rights, Scope},
    data::{Clip, ClipKind, ClipboardLocation},
    model::{
        AgentState, Axis, BlockId, BlockInfo, BlockKind, BlockStatus, Busy, ClientId, CommandRecord, FloatRect, HistoryEntry, Lifecycle, SessionId,
        SessionSummary, SessionView, TermSize, Watcher, WindowId,
    },
};

/// First frame on a control stream.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ClientHello {
    /// The client's [`crate::PROTOCOL`]; admitted only when equal (0: a
    /// client from before protocol numbers).
    #[serde(default)]
    pub protocol: u32,
    /// The client's [`crate::VT`]; admitted only when equal.
    #[serde(default)]
    pub vt: String,
    /// The client's crate version, for display.
    #[serde(default)]
    pub version: String,
    /// The client's [`crate::BUILD`], for display.
    pub build: String,
    /// The client's [`crate::compat::FEATURES`].
    #[serde(default)]
    pub features: Vec<String>,
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
    /// Remote only: the paired device this endpoint speaks for. Without
    /// it the endpoint itself must be the paired device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<crate::device::DeviceProof>,
    /// A bearer secret this client is admitted with: an agent token
    /// (`silot1…`) on the unix socket, a share link's secret remotely. The
    /// connection gets exactly that grant's access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    /// What kind of client this is. Calls from a Silo app (`app`) are the
    /// human's own: they need no approval.
    #[serde(default)]
    pub role: ClientRole,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClientRole {
    /// The CLI, scripts and agents.
    #[default]
    Cli,
    /// A Silo app: forwarded `app.*` requests go to it, and it approves.
    App,
    #[serde(other)]
    Unknown,
}

impl ClientHello {
    /// This build's hello.
    #[must_use]
    pub fn ours(client_name: &str, passive: bool, invite: Option<String>) -> Self {
        Self {
            protocol: crate::PROTOCOL,
            vt: crate::VT.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build: crate::BUILD.to_owned(),
            features: crate::compat::features(),
            client_name: client_name.to_owned(),
            token: None,
            passive,
            invite,
            device: None,
            grant: None,
            role: ClientRole::Cli,
        }
    }
}

/// Reply to [`ClientHello`].
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServerHello {
    /// The server's [`crate::PROTOCOL`].
    #[serde(default)]
    pub protocol: u32,
    /// The server's [`crate::VT`].
    #[serde(default)]
    pub vt: String,
    /// The server's [`crate::BUILD`] (its version is in `server`).
    pub build: String,
    /// The server's [`crate::compat::server_features`].
    #[serde(default)]
    pub features: Vec<String>,
    pub client_id: ClientId,
    pub server: ServerInfo,
    /// What a connection admitted by a grant (a share link, an agent
    /// token) may do; `None` for the owner and paired devices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted: Option<Granted>,
    /// The server's [`crate::CHANNEL`]; absent (release) from older
    /// builds. A build never takes over or adopts a server of the other
    /// channel.
    #[serde(default)]
    pub channel: crate::Channel,
}

impl ServerHello {
    /// This build's reply.
    #[must_use]
    pub fn ours(client_id: ClientId, server: ServerInfo) -> Self {
        Self {
            protocol: crate::PROTOCOL,
            vt: crate::VT.to_owned(),
            build: crate::BUILD.to_owned(),
            features: crate::compat::server_features(),
            client_id,
            server,
            granted: None,
            channel: crate::CHANNEL,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServerInfo {
    pub version: String,
    pub pid: u32,
    pub server_id: String,
    pub started_at: u64,
    /// The server's iroh EndpointId.
    pub endpoint_id: String,
    /// The machine's hostname (empty if it has none): the default name
    /// of the host on devices that pair with it.
    #[serde(default)]
    pub hostname: String,
    /// Why remote (iroh) access is unavailable; `None` while it serves.
    #[serde(default)]
    pub remote_error: Option<String>,
    /// The server's current reachable address: iroh `EndpointAddr` (id,
    /// home relay URL, direct addresses) in its serde JSON form, as of this
    /// reply. Clients refresh stored host addresses from it. `None` without
    /// remote access.
    #[serde(default)]
    pub endpoint_addr: Option<Value>,
    /// Seconds without clients after which the server exits; 0 = never.
    pub idle_exit_secs: u64,
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
    #[serde(rename = "block.report")]
    BlockReport(BlockReportParams),
    #[serde(rename = "block.notify")]
    BlockNotify(BlockNotifyParams),
    #[serde(rename = "browser.snapshot")]
    BrowserSnapshot(BrowserSnapshotParams),
    #[serde(rename = "browser.find")]
    BrowserFind(BrowserFindParams),
    #[serde(rename = "browser.click")]
    BrowserClick(BrowserClickParams),
    #[serde(rename = "browser.hover")]
    BrowserHover(BrowserHoverParams),
    #[serde(rename = "browser.fill")]
    BrowserFill(BrowserFillParams),
    #[serde(rename = "browser.fill_form")]
    BrowserFillForm(BrowserFillFormParams),
    #[serde(rename = "browser.type")]
    BrowserType(BrowserTypeParams),
    #[serde(rename = "browser.press")]
    BrowserPress(BrowserPressParams),
    #[serde(rename = "browser.scroll")]
    BrowserScroll(BrowserScrollParams),
    #[serde(rename = "browser.wait_for")]
    BrowserWaitFor(BrowserWaitForParams),
    #[serde(rename = "browser.dialog")]
    BrowserDialog(BrowserDialogParams),
    #[serde(rename = "browser.eval")]
    BrowserEval(BrowserEvalParams),
    #[serde(rename = "browser.console")]
    BrowserConsole(BrowserConsoleParams),
    #[serde(rename = "browser.network")]
    BrowserNetwork(BrowserNetworkParams),
    #[serde(rename = "browser.screenshot")]
    BrowserScreenshot(BrowserScreenshotParams),
    #[serde(rename = "browser.fetch")]
    BrowserFetch(BrowserFetchParams),
    #[serde(rename = "files.list")]
    FilesList(FilesListParams),
    #[serde(rename = "files.read")]
    FilesRead(FilesReadParams),
    #[serde(rename = "files.read_image")]
    FilesReadImage(FilesReadImageParams),
    #[serde(rename = "files.find")]
    FilesFind(FilesFindParams),
    #[serde(rename = "files.set")]
    FilesSet(FilesSetParams),
    #[serde(rename = "files.root")]
    FilesRoot(FilesRootParams),
    #[serde(rename = "files.write")]
    FilesWrite(FilesWriteParams),
    #[serde(rename = "files.create")]
    FilesCreate(FilesCreateParams),
    #[serde(rename = "files.mkdir")]
    FilesMkdir(FilesMkdirParams),
    #[serde(rename = "files.rename")]
    FilesRename(FilesRenameParams),
    #[serde(rename = "files.copy")]
    FilesCopy(FilesTransferParams),
    #[serde(rename = "files.move")]
    FilesMove(FilesTransferParams),
    #[serde(rename = "files.trash")]
    FilesTrash(FilesTrashParams),
    #[serde(rename = "files.search")]
    FilesSearch(FilesSearchParams),
    #[serde(rename = "changes.status")]
    ChangesStatus(ChangesStatusParams),
    #[serde(rename = "changes.diff")]
    ChangesDiff(ChangesDiffParams),
    #[serde(rename = "changes.set")]
    ChangesSet(ChangesSetParams),
    #[serde(rename = "changes.stage")]
    ChangesStage(ChangesHunksParams),
    #[serde(rename = "changes.unstage")]
    ChangesUnstage(ChangesHunksParams),
    #[serde(rename = "changes.revert")]
    ChangesRevert(ChangesHunksParams),
    #[serde(rename = "changes.commit")]
    ChangesCommit(ChangesCommitParams),
    #[serde(rename = "changes.root")]
    ChangesRoot(ChangesRootParams),
    #[serde(rename = "search.files")]
    SearchFiles(SearchFilesParams),
    #[serde(rename = "search.grep")]
    SearchGrep(SearchGrepParams),
    #[serde(rename = "search.opened")]
    SearchOpened(SearchOpenedParams),
    #[serde(rename = "server.info")]
    ServerInfo {},
    #[serde(rename = "server.shutdown")]
    ServerShutdown {
        /// Close even when programs are running (see `blocks.busy`).
        #[serde(default)]
        force: bool,
    },
    /// Stop, then start this server's own binary again; ends every session.
    #[serde(rename = "server.restart")]
    ServerRestart {
        /// Close even when programs are running (see `blocks.busy`).
        #[serde(default)]
        force: bool,
    },
    /// The end of the server log.
    #[serde(rename = "server.log")]
    ServerLog {
        #[serde(default)]
        lines: Option<u32>,
    },
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
        /// Files blocks: `cwd` is the root (default: the cwd of the block
        /// split or of the active window's focused block, else `$HOME`).
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
    SessionClose {
        session_id: SessionId,
        /// Close even when programs are running (see `blocks.busy`).
        #[serde(default)]
        force: bool,
    },

    #[serde(rename = "window.create")]
    WindowCreate {
        session_id: SessionId,
        /// Files blocks: `cwd` is the root (default: the cwd of the block
        /// split or of the active window's focused block, else `$HOME`).
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
    /// Select the window `delta` places after (negative: before) the
    /// active one, wrapping.
    #[serde(rename = "window.step")]
    WindowStep { session_id: SessionId, delta: i32 },
    #[serde(rename = "window.close")]
    WindowClose {
        window_id: WindowId,
        /// Close even when programs are running (see `blocks.busy`).
        #[serde(default)]
        force: bool,
    },
    #[serde(rename = "window.rename")]
    WindowRename { window_id: WindowId, name: Option<String> },
    #[serde(rename = "window.move")]
    WindowMove { window_id: WindowId, index: usize },

    /// One block's metadata and status.
    #[serde(rename = "block.view")]
    BlockView { block_id: BlockId },
    #[serde(rename = "block.split")]
    BlockSplit {
        block_id: BlockId,
        axis: Axis,
        /// Files blocks: `cwd` is the root (default: the cwd of the block
        /// split or of the active window's focused block, else `$HOME`).
        #[serde(default)]
        spawn: SpawnSpec,
        #[serde(default)]
        kind: BlockKind,
        /// Browser blocks: the start URL (default `about:blank`).
        #[serde(default)]
        url: Option<String>,
        /// Agent blocks: the otto session it shows from the start (a sub-agent).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<AgentState>,
    },
    /// A parked terminal block in the session of `block_id`, its owner
    /// (e.g. an agent block): in no layout until revealed, closed with its
    /// owner. With a `command` it is a command block: it ends when the
    /// command does, closing at once unless revealed (`terminal.capture`
    /// still answers with its final screen and scrollback for a while).
    /// Without one it runs the login shell. `window_id` of the result is
    /// empty. With `kind: browser` it is a parked browser at `url` instead
    /// (servers with feature `parked_browsers`; older ones spawn a shell),
    /// run off screen by the app's browser host until revealed.
    #[serde(rename = "block.spawn")]
    BlockSpawn {
        block_id: BlockId,
        /// Program and arguments; empty means the user's login shell.
        #[serde(default)]
        command: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        /// Extra environment for the program.
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        size: Option<TermSize>,
        /// `terminal` (default) or `browser`.
        #[serde(default)]
        kind: BlockKind,
        /// Browser blocks: the start URL (default `about:blank`).
        #[serde(default)]
        url: Option<String>,
    },
    /// Take a block out of its layout into its session's parked blocks; it
    /// keeps running.
    #[serde(rename = "block.park")]
    BlockPark { block_id: BlockId },
    /// Show a parked block: floating over (default) or split next to its
    /// owner's window, else the session's active window.
    #[serde(rename = "block.reveal")]
    BlockReveal {
        block_id: BlockId,
        #[serde(default, rename = "as")]
        reveal: Reveal,
    },
    #[serde(rename = "block.close")]
    BlockClose {
        block_id: BlockId,
        /// Close even when programs are running (see `blocks.busy`).
        #[serde(default)]
        force: bool,
    },
    #[serde(rename = "block.focus")]
    BlockFocus { block_id: BlockId },
    /// Focus the block `delta` places after (negative: before) `block_id`
    /// in its window's reading order, wrapping.
    #[serde(rename = "block.focus_step")]
    BlockFocusStep { block_id: BlockId, delta: i32 },
    /// Focus the block next to `block_id` in `direction`.
    #[serde(rename = "block.focus_dir")]
    BlockFocusDir { block_id: BlockId, direction: Direction },
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
    /// Exactly one of `rect` and `dock`. Servers with feature
    /// `dock_sides` also dock a tiled block (taking it out of its place
    /// first) and honour `Dock.before`; older ones dock floats only.
    #[serde(rename = "block.move")]
    BlockMove { block_id: BlockId, rect: Option<FloatRect>, dock: Option<Dock> },
    #[serde(rename = "layout.ratio")]
    LayoutRatio { window_id: WindowId, path: Vec<usize>, ratio: f32 },
    /// Grow (negative: shrink) the split `block_id` sits in by `delta`.
    #[serde(rename = "layout.grow")]
    LayoutGrow { block_id: BlockId, delta: f32 },
    /// Move the edge of `block_id` facing `direction` that way.
    #[serde(rename = "layout.edge")]
    LayoutEdge { block_id: BlockId, direction: Direction },
    /// Give every split of a window equal space.
    #[serde(rename = "layout.equalize")]
    LayoutEqualize { window_id: WindowId },

    /// Blocks running a program a close would end, among those given (all
    /// omitted: the whole server), limited to those the caller can view.
    #[serde(rename = "blocks.busy")]
    BlocksBusy {
        #[serde(default)]
        session_id: Option<SessionId>,
        #[serde(default)]
        window_id: Option<WindowId>,
        #[serde(default)]
        block_ids: Option<Vec<BlockId>>,
    },
    /// Mark a block's `done`/`error` records seen (`ids` omitted: all).
    #[serde(rename = "block.status_seen")]
    BlockStatusSeen {
        block_id: BlockId,
        #[serde(default)]
        ids: Option<Vec<String>>,
    },
    /// The commands a block's shell ran, newest last.
    #[serde(rename = "block.commands")]
    BlockCommands {
        block_id: BlockId,
        #[serde(default)]
        limit: Option<u32>,
    },

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
    /// A finished command's output (`index` omitted: the last one).
    #[serde(rename = "terminal.command_output")]
    TerminalCommandOutput {
        block_id: BlockId,
        #[serde(default)]
        index: Option<u64>,
    },

    /// Read the clipboard of the block's attached clients: the first answer
    /// wins; `content` is `None` when none answers within 3 s.
    #[serde(rename = "clipboard.read")]
    ClipboardRead { block_id: BlockId, kind: ClipKind },
    /// Set the clipboard of the block's attached clients (as OSC 52).
    #[serde(rename = "clipboard.write")]
    ClipboardWrite { block_id: BlockId, location: ClipboardLocation, text: String },

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

    /// Agent blocks: the shown otto session and its title, or the block's
    /// folder (`cwd`; a new folder clears the session unless one is given).
    #[serde(rename = "agent.set")]
    AgentSet {
        block_id: BlockId,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },

    /// Ask attached UI clients (the app) to show a session.
    #[serde(rename = "ui.open_session")]
    UiOpenSession { session_id: SessionId },

    /// Owner only: a single-use invite valid for `ttl_secs`. A `device`
    /// invite pairs the redeeming device; a `join` invite only lets it call
    /// `host.offer` once.
    #[serde(rename = "pair.create")]
    PairCreate { ttl_secs: u64, purpose: InvitePurpose },
    /// Owner only: paired devices.
    #[serde(rename = "pair.list")]
    PairList {},
    /// Owner only: forget a device and close its connections.
    #[serde(rename = "pair.revoke")]
    PairRevoke { id: String },
    /// Join-invite connections only (and only this): offer the caller's
    /// server, `ticket` being its `silo1…` pairing ticket. Emits
    /// `host.offered`.
    #[serde(rename = "host.offer")]
    HostOffer { ticket: String, name: String },
    /// Owner only: the unix socket of registered service `name` (`"otto"`),
    /// started on demand; what a tunnel to it reaches, for this machine's
    /// clients.
    #[serde(rename = "service.socket")]
    ServiceSocket { name: String },

    /// A grant with a fresh bearer secret, shown once: an agent token or a
    /// share link. `ttl_secs` defaults to a day for links and to none for
    /// tokens; at most 30 days.
    #[serde(rename = "grant.create")]
    GrantCreate {
        scope: Scope,
        rights: Rights,
        kind: GrantKind,
        #[serde(default)]
        ttl_secs: Option<u64>,
        #[serde(default)]
        label: Option<String>,
    },
    #[serde(rename = "grant.list")]
    GrantList {},
    /// Remove a grant and close its connections.
    #[serde(rename = "grant.revoke")]
    GrantRevoke { id: String },
    /// Allow or deny a call waiting for approval (`approval.requested`).
    #[serde(rename = "approval.answer")]
    ApprovalAnswer { id: String, allow: bool },

    /// Local only: whether Silo Server holds each macOS privacy permission
    /// (checked as the server), and the bundle to drag into System
    /// Settings' lists.
    #[serde(rename = "permissions.status")]
    PermissionsStatus {},
    /// Local only: help the user grant `id`. Screen Recording asks macOS
    /// once (its own dialog); then the app shows its permission helper
    /// (`app.permissions_show`). Fails with `no_app` when no Silo app is
    /// connected; a granted permission returns at once.
    #[serde(rename = "permissions.request")]
    PermissionsRequest { id: PermissionId },

    /// An app's answer to an `app.request`.
    #[serde(rename = "app.reply")]
    AppReply {
        request_id: String,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<RpcError>,
    },
    #[serde(rename = "app.window_new")]
    AppWindowNew {
        #[serde(default)]
        session_id: Option<SessionId>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.window_cycle")]
    AppWindowCycle {
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.switcher")]
    AppSwitcher {
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.session_step")]
    AppSessionStep {
        delta: i32,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.hosts_add")]
    AppHostsAdd {
        #[serde(default)]
        ticket: Option<String>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.hosts_rename")]
    AppHostsRename {
        host: String,
        name: String,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.hosts_remove")]
    AppHostsRemove {
        host: String,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.hosts_list")]
    AppHostsList {
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.copy")]
    AppCopy {
        #[serde(default)]
        block_id: Option<BlockId>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.paste")]
    AppPaste {
        #[serde(default)]
        block_id: Option<BlockId>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.select_all")]
    AppSelectAll {
        #[serde(default)]
        block_id: Option<BlockId>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.appearance")]
    AppAppearance {
        density: Density,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.pane_headers")]
    AppPaneHeaders {
        visible: bool,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// Enter, leave (`on`) or toggle (`on` omitted) full screen.
    #[serde(rename = "app.full_screen")]
    AppFullScreen {
        #[serde(default)]
        on: Option<bool>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.settings_open")]
    AppSettingsOpen {
        #[serde(default)]
        section: Option<String>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.settings_reload")]
    AppSettingsReload {
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.settings_open_file")]
    AppSettingsOpenFile {
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// One setting (`id`) or all of them.
    #[serde(rename = "app.settings_get")]
    AppSettingsGet {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.settings_set")]
    AppSettingsSet {
        id: String,
        value: Value,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.update_check")]
    AppUpdateCheck {
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.palette")]
    AppPalette {
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.hide")]
    AppHide {
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.hide_others")]
    AppHideOthers {
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.show_all")]
    AppShowAll {
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.quit")]
    AppQuit {
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.jump_prompt")]
    AppJumpPrompt {
        delta: i32,
        #[serde(default)]
        block_id: Option<BlockId>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.select_output")]
    AppSelectOutput {
        #[serde(default)]
        block_id: Option<BlockId>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    #[serde(rename = "app.copy_output")]
    AppCopyOutput {
        #[serde(default)]
        block_id: Option<BlockId>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// Open a `silos1…` share link (or the bare ticket).
    #[serde(rename = "app.open_share")]
    AppOpenShare {
        link: String,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// Local only: show the Permissions panel (a small floating window
    /// listing every permission, with Silo Server to drag into System
    /// Settings' lists), `id`'s row highlighted; without `id`, none.
    #[serde(rename = "app.permissions_show")]
    AppPermissionsShow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<PermissionId>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
}

/// Method names and one-line descriptions, for `api.list`.
pub const METHODS: &[(&str, &str)] = &[
    ("block.report", "Report a terminal program's status (as OSC 7501)."),
    ("block.notify", "Raise a block notification (as OSC 9/777)."),
    ("browser.snapshot", "Read an accessibility snapshot with element refs."),
    ("browser.find", "Find nodes by text or regular expression."),
    ("browser.click", "Click an element by ref or CSS selector."),
    ("browser.hover", "Move the pointer over an element."),
    ("browser.fill", "Set an input, textarea, select or checkbox value."),
    ("browser.fill_form", "Fill a set of form fields."),
    ("browser.type", "Type text into the focused element."),
    ("browser.press", "Press a key or key chord."),
    ("browser.scroll", "Scroll the page or an element into view."),
    ("browser.wait_for", "Wait for text to appear or disappear."),
    ("browser.dialog", "Accept or dismiss a page dialog."),
    ("browser.eval", "Evaluate a JavaScript function with optional element refs."),
    ("browser.console", "Read captured console messages."),
    ("browser.network", "Read captured network requests."),
    ("browser.screenshot", "Capture the page or an element as an image."),
    ("browser.fetch", "Load a URL in a hidden page of the app (agent profile) and return its rendered HTML."),
    ("files.list", "List a directory of a files block."),
    ("files.read", "Read a file of a files block as text, or describe a binary."),
    ("files.read_image", "Read an image (png, jpeg, gif, webp, svg) of a files block, base64."),
    ("files.find", "Fuzzy-find files under a files block's root, skipping ignored paths."),
    ("files.set", "Change a files block's tabs, tree, expanded folders or scroll."),
    ("files.root", "Show another directory in a files block."),
    ("files.write", "Save a text file of a files block atomically, refusing when it changed on disk since the edit's base."),
    ("files.create", "Create a new file in a files block."),
    ("files.mkdir", "Create a folder in a files block."),
    ("files.rename", "Rename or move one item of a files block, refusing to overwrite unless asked."),
    ("files.copy", "Copy items of a files block into one of its folders (recursively; taken names get the next free one)."),
    ("files.move", "Move items of a files block into one of its folders (taken names get the next free one)."),
    ("files.trash", "Move items of a files block to the host's Trash."),
    ("files.search", "Find items of a files block whose name contains a query, under one folder."),
    ("changes.status", "The changed files of a changes block's scope, with the branch and line counts."),
    ("changes.diff", "One file's diff hunks in a changes block's scope."),
    ("changes.set", "Change a changes block's scope, tabs, layout, review comments, agent or marks."),
    ("changes.stage", "Stage a file, or some of its hunks, in a changes block's repo."),
    ("changes.unstage", "Unstage a file, or some of its hunks, in a changes block's repo."),
    ("changes.revert", "Discard a file's work tree changes, or some hunks of them (untracked files go to the Trash)."),
    ("changes.commit", "Commit the staged changes of a changes block's repo, running its hooks."),
    ("changes.root", "Review the git repo holding another folder in a changes block."),
    ("search.files", "Fuzzy-find files under a folder (fff query syntax: globs, !excludes, git:modified), frecency-ranked."),
    ("search.grep", "Search the text of the files under a folder (plain, regex or fuzzy), within a time budget."),
    ("search.opened", "Record that a file under a folder was opened, so it ranks higher in later searches."),
    ("server.info", "Server version, Ghostty revision and identity."),
    ("server.shutdown", "Stop the server and every process it runs."),
    ("server.restart", "Restart the server from its own binary; ends every session."),
    ("server.log", "The last lines of the server log."),
    ("api.list", "List control methods."),
    ("api.describe", "A method's params and result JSON Schemas and an example request."),
    ("ping", "Liveness probe; answers once the server's state is reachable."),
    ("events.subscribe", "Start receiving sequenced events on this connection."),
    ("session.list", "List sessions."),
    ("session.create", "Create a session with one window running a shell, command, browser, agent or files."),
    ("session.view", "Full session tree and block metadata."),
    ("session.rename", "Rename a session."),
    ("session.close", "Close a session and terminate its processes."),
    ("window.create", "Add a window (tab) with a terminal, browser, agent or files block to a session."),
    ("window.select", "Make a window the session's active window."),
    ("window.step", "Select the window delta places from the active one, wrapping."),
    ("window.close", "Close a window and its blocks."),
    ("window.rename", "Set or clear a window's name."),
    ("window.move", "Move a window to a new index."),
    ("block.view", "One block's metadata and status."),
    ("block.split", "Split a block, starting a new terminal, browser, agent or files block next to it."),
    ("block.spawn", "Start a parked terminal or command block owned by a block."),
    ("block.park", "Take a block out of its layout; it keeps running, parked."),
    ("block.reveal", "Show a parked block as a float or a split."),
    ("block.close", "Close a block and terminate its process."),
    ("block.focus", "Focus a block within its window."),
    ("block.focus_step", "Focus the block delta places from a block in reading order, wrapping."),
    ("block.focus_dir", "Focus the block next to a block in a direction."),
    ("block.zoom", "Zoom or unzoom a block within its window."),
    ("block.swap", "Swap two blocks' positions within the same window layout."),
    ("block.float", "Float a tiled block above its window's layout."),
    ("block.move", "Move and raise a float (rect), or dock a float or tiled block on any side of another tiled block of its window (dock)."),
    ("layout.ratio", "Change the ratio of a split in a window layout."),
    ("layout.grow", "Grow or shrink the split a block sits in."),
    ("layout.edge", "Move a block's edge in a direction."),
    ("layout.equalize", "Give every split of a window equal space."),
    ("blocks.busy", "Blocks running a program a close would end."),
    ("block.status_seen", "Mark a block's done and error records seen."),
    ("block.commands", "The commands a block's shell ran."),
    ("terminal.send", "Write text to a terminal as if typed."),
    ("terminal.capture", "Plain-text contents of a terminal."),
    ("terminal.size", "Applied size, owner and every client's desired size."),
    ("terminal.resize", "Report this client's desired size; applied if owner or unowned."),
    ("terminal.clear", "Clear screen and scrollback, spliced at a parser-ground boundary."),
    ("terminal.reset", "Reset the terminal (RIS), spliced at a parser-ground boundary."),
    ("terminal.command_output", "The output of a finished command."),
    ("clipboard.read", "Read the clipboard of a block's attached clients (first answer wins)."),
    ("clipboard.write", "Set the clipboard of a block's attached clients."),
    ("session.own_block", "Take or request size/theme ownership of blocks."),
    ("session.disown_block", "Release ownership or candidacy of blocks."),
    ("data.connect", "Single-use ticket for a block data stream."),
    ("browser.navigate", "Load a URL in a browser block (bare hosts get a scheme)."),
    ("browser.back", "Go back in a browser block's history."),
    ("browser.forward", "Go forward in a browser block's history."),
    ("browser.reload", "Reload a browser block, optionally bypassing the cache."),
    ("browser.zoom", "Set a browser block's zoom level (0 = 100%)."),
    ("browser.report", "Report a browser block's engine state (owner only)."),
    ("agent.set", "Set an agent block's shown otto session and title, or its folder."),
    ("ui.open_session", "Ask subscribed UI clients to show a session."),
    ("pair.create", "Create a single-use device-pairing or join invite (owner only)."),
    ("pair.list", "List paired devices (owner only)."),
    ("pair.revoke", "Forget a paired device and close its connections (owner only)."),
    ("host.offer", "Offer the caller's server to this one (join-invite connections only)."),
    ("service.socket", "Start a registered service (otto) if needed and return its socket (owner only)."),
    ("grant.create", "Create an agent token or share link with scoped access (owner only)."),
    ("grant.list", "List grants (owner only)."),
    ("grant.revoke", "Remove a grant and close its connections (owner only)."),
    ("approval.answer", "Allow or deny a call waiting for approval."),
    ("permissions.status", "Which macOS privacy permissions Silo Server holds, and the app to drag into System Settings (local only)."),
    ("permissions.request", "Help grant a macOS privacy permission: ask macOS, then show the app's permission helper (local only)."),
    ("app.reply", "Answer an app.request (the app it went to only)."),
    ("app.window_new", "Open a new app window."),
    ("app.window_cycle", "Cycle through app windows."),
    ("app.switcher", "Show the session switcher."),
    ("app.session_step", "Show the session delta places from the current one."),
    ("app.hosts_add", "Add a host to the app."),
    ("app.hosts_rename", "Rename a host in the app."),
    ("app.hosts_remove", "Remove a host from the app."),
    ("app.hosts_list", "List the app's hosts."),
    ("app.copy", "Copy the selection."),
    ("app.paste", "Paste the clipboard."),
    ("app.select_all", "Select all of a block."),
    ("app.appearance", "Set the app's density."),
    ("app.pane_headers", "Show or hide pane headers."),
    ("app.full_screen", "Enter, leave or toggle full screen."),
    ("app.settings_open", "Open Settings."),
    ("app.settings_reload", "Reload the terminal settings file."),
    ("app.settings_open_file", "Open the terminal settings file."),
    ("app.settings_get", "Read one setting or all of them."),
    ("app.settings_set", "Change a setting."),
    ("app.update_check", "Check for updates."),
    ("app.palette", "Open the command palette."),
    ("app.hide", "Hide Silo."),
    ("app.hide_others", "Hide other apps."),
    ("app.show_all", "Show all apps."),
    ("app.quit", "Quit the app; sessions keep running."),
    ("app.jump_prompt", "Scroll to the prompt delta places away."),
    ("app.select_output", "Select the last command's output."),
    ("app.copy_output", "Copy the last command's output."),
    ("app.open_share", "Open a shared link."),
    ("app.permissions_show", "Show the Permissions panel, a macOS privacy permission highlighted (local only)."),
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    /// Details for the code, e.g. `busy`'s blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), data: None }
    }
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
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
    /// Hello rejection: the peer speaks an incompatible protocol (see
    /// [`crate::compat::check`]).
    pub fn incompatible(what: impl std::fmt::Display) -> Self {
        Self::new(Self::INCOMPATIBLE, format!("incompatible: the other side speaks {what}"))
    }
    /// Code of [`RpcError::incompatible`].
    pub const INCOMPATIBLE: &str = "incompatible";
    /// The server does not know the method (a newer client's).
    pub fn method_not_found(method: &str) -> Self {
        Self::new(Self::METHOD_NOT_FOUND, format!("{method} is not supported by this server"))
    }
    /// Code of [`RpcError::method_not_found`].
    pub const METHOD_NOT_FOUND: &str = "method_not_found";
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
    /// `forbidden` with its own message.
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(Self::FORBIDDEN, message)
    }
    /// A close would end running programs; `data.busy` lists them.
    pub const BUSY: &str = "busy";
    /// An `app.*` call with no Silo app connected.
    pub const NO_APP: &str = "no_app";
    /// Nobody answered in time.
    pub const TIMEOUT: &str = "timeout";
    /// A page did not load: DNS, TLS or connection failure.
    pub const NAVIGATION: &str = "navigation";
    /// No Silo app can approve the call, but this caller may confirm it on
    /// its own terminal: `data.approval` is the id to answer with
    /// `approval.answer`, then repeat the call.
    pub const CONFIRM: &str = "confirm";
    /// `files.write`: the file changed on disk since the edit's base;
    /// `data` is the file now (`mtime`, `size`). Write again with `force`.
    /// `changes.stage`/`unstage`/`revert`: a hunk named is no longer in
    /// the file, or no longer applies; review it again.
    pub const CONFLICT: &str = "conflict";
    /// `changes.*`: a git command failed; the message is its output.
    pub const GIT: &str = "git";
    /// `files.create`: the path is taken.
    pub const EXISTS: &str = "exists";
    /// The file or its folder can't be written (permissions).
    pub const READ_ONLY: &str = "read_only";
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

#[derive(Clone, Debug, Serialize)]
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
    /// An event this side does not know (a newer peer's); skipped.
    UnknownEvent { seq: u64 },
    /// A known event whose data does not parse (a newer peer's variant
    /// inside it): the receiver's view of it is stale; re-fetch.
    MalformedEvent { seq: u64, event: String, error: String },
}

impl<'de> Deserialize<'de> for ServerMessage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Response {
            id: u64,
            #[serde(default)]
            result: Option<Value>,
            #[serde(default)]
            error: Option<RpcError>,
        }
        let mut value = Value::deserialize(deserializer)?;
        if let Some(seq) = value.as_object_mut().and_then(|o| o.remove("seq")) {
            let seq = seq.as_u64().ok_or_else(|| serde::de::Error::custom("seq is not a number"))?;
            let name = value.get("event").and_then(Value::as_str).unwrap_or_default().to_owned();
            if !EVENTS.contains(&name.as_str()) {
                return Ok(Self::UnknownEvent { seq });
            }
            return Ok(match serde_json::from_value::<Event>(value) {
                Ok(event) => Self::Event { seq, event: Box::new(event) },
                Err(error) => Self::MalformedEvent { seq, event: name, error: error.to_string() },
            });
        }
        let Response { id, result, error } = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self::Response { id, result, error })
    }
}

/// The wire name of every [`Event`].
pub const EVENTS: &[&str] = &[
    "sessions.changed",
    "session.changed",
    "session.closed",
    "block.changed",
    "block.bell",
    "ui.open_session",
    "browser.command",
    "host.offered",
    "grants.changed",
    "approval.requested",
    "approval.resolved",
    "app.request",
    "block.status",
    "block.attention",
    "block.command_finished",
    "session.watchers",
    "files.changed",
    "changes.changed",
];

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
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
    /// Another machine's server offered to link: seen in a terminal's
    /// output (SSH) or sent over a join invite.
    #[serde(rename = "host.offered")]
    HostOffered { offer: HostOffer },
    /// Every grant, after one was created, revoked or expired (owner).
    #[serde(rename = "grants.changed")]
    GrantsChanged { grants: Vec<Grant> },
    /// A call waits for a human: answer with `approval.answer`.
    #[serde(rename = "approval.requested")]
    ApprovalRequested {
        id: String,
        /// The method waiting.
        action: String,
        /// What allowing does, written by the server.
        summary: String,
        caller: Caller,
        expires_in_ms: u64,
    },
    /// An approval was answered, denied or timed out: dismiss its card.
    #[serde(rename = "approval.resolved")]
    ApprovalResolved { id: String, allowed: bool },
    /// Run `request` and answer with `app.reply` (sent to one app).
    #[serde(rename = "app.request")]
    AppRequest { request_id: String, request: AppCall, caller: Caller },
    /// A block's whole status; never bumps `revision`.
    #[serde(rename = "block.status")]
    BlockStatus { status: BlockStatus },
    /// Something in a block wants attention (for notifications).
    #[serde(rename = "block.attention")]
    BlockAttention {
        block_id: BlockId,
        session_id: SessionId,
        session_name: String,
        block_title: String,
        reason: AttentionReason,
        #[serde(default)]
        title: String,
        #[serde(default)]
        body: String,
        /// Unix ms.
        at: u64,
    },
    #[serde(rename = "block.command_finished")]
    CommandFinished { block_id: BlockId, session_id: SessionId, record: CommandRecord },
    /// Who watches a session's blocks changed; no revision bump.
    #[serde(rename = "session.watchers")]
    SessionWatchers { session_id: SessionId, watchers: Vec<Watcher> },
    /// Files changed on disk under a files block's watched folders (the
    /// root, expanded folders and open files' folders), debounced. Paths
    /// are relative to the root (empty: the root); `truncated` when there
    /// were too many to list: re-read everything shown.
    #[serde(rename = "files.changed")]
    FilesChanged {
        block_id: BlockId,
        paths: Vec<String>,
        #[serde(default)]
        truncated: bool,
    },
    /// A changes block's repo changed (work tree, index or `HEAD`),
    /// debounced: re-read its status and the open file.
    #[serde(rename = "changes.changed")]
    ChangesChanged { block_id: BlockId },
}

/// Who made a call: a client's label and, for a process in one of this
/// server's panes, that pane (`session › block title`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Caller {
    pub client_id: ClientId,
    pub label: String,
    #[serde(default)]
    pub from: Option<String>,
    /// The call comes from the device this card is shown on (a paired
    /// device's own app asked about that device's call); `from` then
    /// reads "this device (CLI)" for apps that do not know this field.
    #[serde(default)]
    pub this_device: bool,
}

/// A forwarded request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AppCall {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AttentionReason {
    Blocked,
    Done,
    Error,
    Notification,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    Comfortable,
    Compact,
}

/// What `grant.create` makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GrantKind {
    /// An agent token for the unix socket (`SILO_TOKEN`).
    Token,
    /// A share link.
    Link,
    /// Another account of a login server (reserved: refused).
    Account,
}

/// What a `pair.create` invite grants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum InvitePurpose {
    /// Pair the redeeming device.
    Device,
    /// Let the redeeming peer offer its own server (`host.offer`) once.
    Join,
}

/// A server offered for linking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostOffer {
    /// The offering server's `silo1…` pairing ticket.
    pub ticket: String,
    /// Its hostname.
    pub name: String,
    /// Its iroh EndpointId (from the ticket).
    pub endpoint_id: String,
    pub source: OfferSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OfferSource {
    /// Printed (OSC 7717) in this terminal block, e.g. over SSH.
    Terminal { block_id: BlockId },
    /// Sent over the join invite `invite_id`.
    Join { invite_id: String },
    /// A newer source; such offers are ignored.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BrowserCommand {
    Navigate {
        url: String,
    },
    Back,
    Forward,
    Reload {
        ignore_cache: bool,
    },
    Zoom {
        zoom: f32,
    },
    /// A newer command; skipped.
    #[serde(other)]
    Unknown,
}

// Typed results.

/// How `block.reveal` shows a parked block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Reveal {
    /// Over the window, centred.
    #[default]
    Float,
    /// Split next to the owner (or the focused block).
    Split,
}

/// `block.move` docking target: split `target` along `axis`, the docked
/// block second (right or below), or first (left or above) with `before`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Dock {
    pub target: BlockId,
    pub axis: Axis,
    /// Feature `dock_sides`: older servers ignore it and dock second, so
    /// clients send it only to servers advertising that.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub before: bool,
}

/// The `{}` result of methods that only acknowledge.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
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
    /// What calling it needs.
    #[serde(default)]
    pub access: Option<crate::access::Access>,
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
    /// Whether the program still runs; a parked block that exited and
    /// closed answers from its final capture for a while.
    #[serde(default)]
    pub lifecycle: Lifecycle,
    /// Exit code once exited (`None` while running or when killed).
    #[serde(default)]
    pub exited: Option<i32>,
    /// A running program reads a line: the tty is canonical and the cursor
    /// ends a non-empty line (`Password:`, `[y/N] `).
    #[serde(default)]
    pub line_prompt: bool,
}

/// Result of `clipboard.read`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Pasted {
    pub content: Option<Clip>,
}

/// Result of `pair.create`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PairInvite {
    /// Identifies the invite in a later [`OfferSource::Join`].
    pub invite_id: String,
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

/// Result of `service.socket`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServiceSocket {
    pub path: std::path::PathBuf,
}

/// Result of `grant.create`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrantCreated {
    pub grant: Grant,
    /// The bearer secret's text (`silot1…` for a token); shown once.
    pub secret: String,
    /// The `silos1…` share ticket of a link.
    #[serde(default)]
    pub ticket: Option<String>,
}

/// Result of `grant.list`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Grants {
    pub grants: Vec<Grant>,
}

/// Result of `block.view`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BlockView {
    pub block: BlockInfo,
    #[serde(default)]
    pub status: Option<BlockStatus>,
}

/// A busy block, as `blocks.busy` and a `busy` refusal list it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BusyBlock {
    pub block_id: BlockId,
    pub session_id: SessionId,
    pub window_id: WindowId,
    pub session_name: String,
    pub block_title: String,
    pub busy: Busy,
}

/// Result of `blocks.busy`; also `data` of a `busy` error.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BusyBlocks {
    pub busy: Vec<BusyBlock>,
}

/// Result of `block.commands`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CommandRecords {
    pub records: Vec<CommandRecord>,
}

/// Most `terminal.command_output` text, in bytes: longer output keeps its
/// end.
pub const MAX_COMMAND_OUTPUT: usize = 1 << 20;

/// Result of `terminal.command_output`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CommandOutput {
    pub text: String,
    pub record: CommandRecord,
    /// Only the output's end: it was longer than [`MAX_COMMAND_OUTPUT`],
    /// or its start left the scrollback.
    #[serde(default)]
    pub truncated: bool,
}

/// Result of `server.log`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LogText {
    pub text: String,
}

/// The result of a forwarded `app.*` method: whatever the app answers.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct AppResult {
    #[serde(flatten)]
    pub fields: serde_json::Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_hello_carries_the_channel_and_old_peers_are_release() {
        let info = ServerInfo {
            version: "0.1.0".into(),
            pid: 1,
            server_id: "s".into(),
            started_at: 0,
            endpoint_id: String::new(),
            hostname: String::new(),
            remote_error: None,
            endpoint_addr: None,
            idle_exit_secs: 0,
        };
        let mut json = serde_json::to_value(ServerHello::ours(1, info)).unwrap();
        assert_eq!(json["channel"], crate::CHANNEL.name());
        json.as_object_mut().unwrap().remove("channel");
        let old: ServerHello = serde_json::from_value(json).unwrap();
        assert_eq!(old.channel, crate::Channel::Release, "a server from before channels is a release build");
    }

    #[test]
    fn request_and_messages_round_trip() {
        let method = Method::BlockSplit {
            block_id: "b".into(),
            axis: Axis::Vertical,
            spawn: SpawnSpec::default(),
            kind: BlockKind::Browser,
            url: None,
            agent: None,
        };
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

    /// [`EVENTS`] names exactly the variants of [`Event`].
    #[test]
    fn events_lists_every_event() {
        fn names(value: &Value, out: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    if let Some(name) = map.get("properties").and_then(|p| p.get("event")).and_then(|e| e.get("const").or_else(|| e.get("enum"))) {
                        match name {
                            Value::Array(all) => out.extend(all.iter().filter_map(Value::as_str).map(str::to_owned)),
                            other => out.extend(other.as_str().map(str::to_owned)),
                        }
                    }
                    map.values().for_each(|v| names(v, out));
                }
                Value::Array(items) => items.iter().for_each(|v| names(v, out)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        names(&schemars::schema_for!(Event).to_value(), &mut found);
        found.sort();
        found.dedup();
        let mut listed: Vec<String> = EVENTS.iter().map(|&e| e.to_owned()).collect();
        listed.sort();
        assert_eq!(found, listed);
    }
}
