//! Session tree, clients, tickets and the sequenced event bus.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use silo_proto::{
    Frame,
    access::{Rights, Scope},
    compat::feature,
    control::{
        BrowserCommand, Capture, ChangesSetParams, ClientRole, Created, DataTicket, Dock, Event, FilesSetParams, OwnMode, Reveal, RpcError,
        ServerMessage, SpawnSpec,
    },
    data::ticket_hex,
    kind,
    model::{
        AgentLink, AgentState, AttentionCounts, Axis, BlockId, BlockInfo, BlockKind, BlockStatus, BrowserState, ChangesState, ClientId, FilesState,
        Float, FloatRect, Layout, Lifecycle, Parked, SessionId, SessionInfo, SessionSummary, SessionView, TermSize, Watcher, WindowId, WindowInfo,
        browser_url, short_client_name,
    },
    text::{MAX_TITLE, untrusted},
};
use silo_pty::SpawnConfig;
use tokio::sync::{Notify, mpsc::UnboundedSender};

use crate::{
    access::{On, Principal, device_grant, rights_on, sees_block, sees_session},
    agent::AgentBlock,
    block::{self, BlockHandle, BlockMsg, Note, Sink, now_ms},
    browser::{Block, BrowserBlock, Report},
    changes::ChangesBlock,
    files::FilesBlock,
};

/// A control connection whose unsent event backlog exceeds this is closed;
/// the client reconnects and re-reads state (events are never dropped).
const EVENT_BACKLOG: usize = 8 * 1024 * 1024;
const TICKET_TTL: Duration = Duration::from_secs(60);
/// How long a closed command block's final capture stays readable.
const ENDED_TTL: Duration = Duration::from_secs(300);
/// Most final captures kept.
const ENDED_MAX: usize = 64;
/// A parked block's size when the caller gives none: wide, so few lines wrap.
const PARKED_SIZE: TermSize = TermSize { columns: 200, rows: 50, cell_width: 0, cell_height: 0 };

#[derive(Clone, Serialize, Deserialize)]
pub struct Window {
    pub id: WindowId,
    pub name: Option<String>,
    pub layout: Layout,
    pub focused: BlockId,
    /// Blocks by most recent focus, `focused` first.
    #[serde(default)]
    pub history: Vec<BlockId>,
    pub zoomed: bool,
    /// Back to front; the last is topmost.
    pub floats: Vec<Float>,
}

impl Window {
    /// A window holding the one tiled block `block`.
    pub fn new(id: WindowId, block: &str) -> Self {
        Self {
            id,
            name: None,
            layout: Layout::block(block),
            focused: block.to_owned(),
            history: vec![block.to_owned()],
            zoomed: false,
            floats: Vec::new(),
        }
    }

    pub fn focus(&mut self, block: &str) {
        block.clone_into(&mut self.focused);
        self.history.retain(|b| b != block);
        self.history.insert(0, block.to_owned());
    }

    /// Tiled blocks in reading order, then floats back to front.
    pub fn blocks(&self) -> Vec<BlockId> {
        let mut blocks = self.layout.blocks();
        blocks.extend(self.floats.iter().map(|f| f.id.clone()));
        blocks
    }

    pub fn contains(&self, block: &str) -> bool {
        self.layout.contains(block) || self.floating(block).is_some()
    }

    /// Index of `block` in `floats`.
    pub fn floating(&self, block: &str) -> Option<usize> {
        self.floats.iter().position(|f| f.id == block)
    }
}

/// A login shell that exited with status 0: its block closes. A block
/// running an explicit command keeps its output readable instead.
fn exited_cleanly(block: &Block) -> bool {
    let Block::Terminal(handle) = block else { return false };
    let info = handle.info.lock().unwrap();
    !handle.command && info.lifecycle == Lifecycle::Exited && info.exited == Some(0)
}

/// The leaf of `layout` nearest to `target` in the subtree that takes over
/// its space when it is removed.
fn sibling(layout: &Layout, target: &str) -> Option<BlockId> {
    let Layout::Split { first, second, .. } = layout else {
        return None;
    };
    match (&**first, &**second) {
        (Layout::Block { id }, other) if id == target => other.blocks().into_iter().next(),
        (other, Layout::Block { id }) if id == target => other.blocks().pop(),
        _ => sibling(first, target).or_else(|| sibling(second, target)),
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub windows: Vec<Window>,
    pub active_window: WindowId,
    pub revision: u64,
    pub created_at: u64,
    /// Blocks in no layout, oldest first.
    #[serde(default)]
    pub parked: Vec<Parked>,
}

impl Session {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            windows: self
                .windows
                .iter()
                .map(|w| WindowInfo {
                    id: w.id.clone(),
                    name: w.name.clone(),
                    layout: w.layout.clone(),
                    focused: w.focused.clone(),
                    zoomed: w.zoomed,
                    floats: w.floats.clone(),
                })
                .collect(),
            active_window: self.active_window.clone(),
            revision: self.revision,
            created_at: self.created_at,
            parked: self.parked.clone(),
        }
    }

    /// Every block: windows in order, then parked ones.
    pub fn blocks(&self) -> Vec<BlockId> {
        self.windows.iter().flat_map(Window::blocks).chain(self.parked.iter().map(|p| p.id.clone())).collect()
    }

    pub fn is_parked(&self, block: &str) -> bool {
        self.parked.iter().any(|p| p.id == block)
    }
}

pub struct ClientEntry {
    pub name: String,
    /// Short name shown as a block's `owner_name`.
    pub label: String,
    pub passive: bool,
    pub events: Option<Sink>,
    pub close: Option<tokio::sync::oneshot::Sender<()>>,
    pub attached: usize,
    /// The remote connection this client's control stream runs on; data
    /// tickets it is issued are redeemable only there (`None`: unix).
    pub origin: Option<u64>,
    /// What the client advertised in its hello (see `silo_proto::compat`).
    pub features: Vec<String>,
    /// Who it acts as.
    pub principal: Principal,
    pub role: ClientRole,
    /// The pane (block) the client's process runs in, if any.
    pub pane: Option<BlockId>,
    /// Its process has a controlling terminal (unix clients).
    pub tty: bool,
    /// Its open data streams by stream id: block, rights, and what ends
    /// the stream.
    pub streams: HashMap<u64, (BlockId, Rights, Option<tokio::sync::oneshot::Sender<()>>)>,
    /// The user's last input, ownership take or focus through it: forwarded
    /// `app.*` requests go to the most recently active app.
    pub active_at: Option<Instant>,
}

impl ClientEntry {
    pub fn new(name: String, passive: bool) -> Self {
        let label = short_client_name(&name).to_owned();
        Self {
            name,
            label,
            passive,
            events: None,
            close: None,
            attached: 0,
            origin: None,
            features: Vec::new(),
            principal: Principal::Owner,
            role: ClientRole::Cli,
            pane: None,
            tty: false,
            streams: HashMap::new(),
            active_at: None,
        }
    }

    /// Whether the client advertised `feature` (`None`: needs none).
    pub fn supports(&self, feature: Option<&str>) -> bool {
        feature.is_none_or(|f| self.features.iter().any(|x| x == f))
    }

    /// Close the client's control connection and data streams.
    fn close_all(&mut self) {
        self.events = None;
        for (_, _, end) in self.streams.values_mut() {
            if let Some(end) = end.take() {
                let _ = end.send(());
            }
        }
        if let Some(close) = self.close.take() {
            let _ = close.send(());
        }
    }
}

/// The feature (`silo_proto::compat::feature`) a client must advertise to
/// receive `event`.
fn needed_feature(event: &Event) -> Option<&'static str> {
    match event {
        Event::GrantsChanged { .. } | Event::ApprovalRequested { .. } | Event::ApprovalResolved { .. } | Event::SessionWatchers { .. } => {
            Some(feature::ACCESS)
        }
        Event::AppRequest { request, .. } if request.method.starts_with("browser.") => Some(feature::BROWSER_ACTIONS),
        Event::AppRequest { .. } => Some(feature::APP_REQUESTS),
        Event::BlockStatus { .. } | Event::BlockAttention { .. } | Event::CommandFinished { .. } => Some(feature::STATUS),
        Event::FilesChanged { .. } => Some(feature::FILES),
        Event::ChangesChanged { .. } => Some(feature::CHANGES),
        Event::ChangesProgress { .. } => Some(feature::CHANGES_SYNC),
        Event::SessionsChanged { .. }
        | Event::SessionChanged { .. }
        | Event::SessionClosed { .. }
        | Event::BlockChanged { .. }
        | Event::BlockBell { .. }
        | Event::OpenSession { .. }
        | Event::BrowserCommand { .. }
        | Event::HostOffered { .. } => None,
    }
}

/// Which clients receive an event, by their principal.
#[derive(Clone, Debug)]
pub enum Visibility {
    /// Every client; session summaries limited to the visible sessions.
    Sessions,
    Session(SessionId),
    Block {
        block: BlockId,
        session: Option<SessionId>,
    },
    /// The owner only: the event carries secrets.
    Owner,
    Clients(Vec<ClientId>),
}

impl Visibility {
    fn admits(&self, id: ClientId, client: &ClientEntry) -> bool {
        match self {
            Self::Sessions => true,
            Self::Session(session) => sees_session(&client.principal, session),
            Self::Block { block, session } => sees_block(&client.principal, block, session.as_deref()),
            Self::Owner => client.principal == Principal::Owner,
            Self::Clients(ids) => ids.contains(&id),
        }
    }
}

struct Ticket {
    block_id: BlockId,
    client_id: ClientId,
    origin: Option<u64>,
    rights: Rights,
    expires: Instant,
}

pub struct Registry {
    pub sessions: Vec<Session>,
    pub blocks: HashMap<BlockId, Block>,
    pub clients: HashMap<ClientId, ClientEntry>,
    tickets: HashMap<String, Ticket>,
    next_client: ClientId,
    seq: u64,
    notes: UnboundedSender<Note>,
    /// Blocks whose checkpoint may be stale, and whether the tree changed,
    /// since the last commit.
    dirty: HashSet<BlockId>,
    tree_dirty: bool,
    /// Wakes the persistence worker after a mutation.
    pub persist_kick: Arc<Notify>,
    /// Open PTYs of blocks whose re-adoption after an aborted upgrade
    /// failed: the block shows its restored screen, and the program lives
    /// on (no hangup) until the next upgrade carries it or the block closes.
    pub parked: HashMap<BlockId, silo_pty::Detached>,
    /// Start a session whenever none is left (see `keep_session`).
    keep_session: bool,
    /// Scopes of sessions and blocks closed since last taken: their grants
    /// go (see `Server::settle_grants`).
    pub gone: Vec<Scope>,
    /// Attention counts per session as last sent in `sessions.changed`.
    attention: HashMap<SessionId, AttentionCounts>,
    /// Final captures of command blocks that exited, for `terminal.capture`
    /// after a parked one closed.
    ended: HashMap<BlockId, (Capture, Instant)>,
    /// Told the otto session of each agent block that closes.
    pub agent_closed: Option<crate::AgentClosed>,
}

fn new_id(prefix: char) -> String {
    format!("{prefix}{:08x}", rand::random::<u32>())
}

/// What a new block runs: a program (`spawn`) or a browser at `url`.
#[derive(Clone, Copy)]
pub struct New<'a> {
    pub spawn: &'a SpawnSpec,
    pub kind: BlockKind,
    pub url: Option<&'a str>,
    /// Agent blocks: the otto session shown from the start.
    pub agent: Option<&'a AgentState>,
}

impl Registry {
    pub fn new(notes: UnboundedSender<Note>) -> Self {
        Self {
            sessions: Vec::new(),
            blocks: HashMap::new(),
            clients: HashMap::new(),
            tickets: HashMap::new(),
            next_client: 1,
            seq: 0,
            notes,
            dirty: HashSet::new(),
            tree_dirty: false,
            persist_kick: Arc::new(Notify::new()),
            parked: HashMap::new(),
            keep_session: false,
            gone: Vec::new(),
            attention: HashMap::new(),
            ended: HashMap::new(),
            agent_closed: None,
        }
    }

    /// A registry holding a restored tree, without the blocks whose login
    /// shell had already exited cleanly; nothing is spawned.
    pub fn restored(notes: UnboundedSender<Note>, sessions: Vec<Session>, blocks: HashMap<BlockId, Block>) -> Self {
        let mut reg = Self { sessions, blocks, ..Self::new(notes) };
        let notes = reg.notes.clone();
        for block in reg.blocks.values_mut() {
            match block {
                Block::Files(files) => files.watch(notes.clone()),
                Block::Changes(changes) => changes.watch(notes.clone()),
                _ => {}
            }
        }
        let done: Vec<BlockId> = reg.blocks.iter().filter(|(_, b)| exited_cleanly(b)).map(|(id, _)| id.clone()).collect();
        for id in done {
            reg.kill_block(&id);
            reg.remove_block(&id);
        }
        reg
    }

    /// From now on never sit at zero sessions; start one now if none.
    pub fn keep_session(&mut self) {
        self.keep_session = true;
        self.keep_one();
    }

    /// With `keep_session`, a fresh session (login shell in the home
    /// directory) when the last one is gone.
    fn keep_one(&mut self) {
        if !self.keep_session || !self.sessions.is_empty() {
            return;
        }
        let spawn = SpawnSpec::default();
        if let Err(error) = self.create_session(None, &New { spawn: &spawn, kind: BlockKind::Terminal, url: None, agent: None }) {
            tracing::error!("start default session: {}", error.message);
        }
    }

    /// The channel block actors report to.
    pub fn notes(&self) -> UnboundedSender<Note> {
        self.notes.clone()
    }

    /// Take the tree flag and, when `blocks` is set or the tree changed, the
    /// dirty blocks accumulated since they were last taken.
    pub fn take_dirty(&mut self, blocks: bool) -> (HashSet<BlockId>, bool) {
        let tree = std::mem::take(&mut self.tree_dirty);
        let dirty = if blocks || tree { std::mem::take(&mut self.dirty) } else { HashSet::new() };
        (dirty, tree)
    }

    /// Give back what [`Self::take_dirty`] took but could not be committed;
    /// the next commit retries it.
    pub fn restore_dirty(&mut self, blocks: impl IntoIterator<Item = BlockId>, tree: bool) {
        self.dirty.extend(blocks);
        self.tree_dirty |= tree;
    }

    /// A block's terminal changed; its checkpoint is stale but the tree is not.
    fn mark_block_dirty(&mut self, block: &str) {
        self.dirty.insert(block.to_owned());
        self.persist_kick.notify_one();
    }

    fn mark_dirty(&mut self, block: Option<&str>) {
        if let Some(block) = block {
            self.dirty.insert(block.to_owned());
        }
        self.tree_dirty = true;
        self.persist_kick.notify_one();
    }

    /// A random id not used by any session, window or block.
    fn fresh_id(&self, prefix: char) -> String {
        loop {
            let id = new_id(prefix);
            let taken =
                self.blocks.contains_key(&id) || self.sessions.iter().any(|s| s.id == id || s.windows.iter().any(|w| w.id == id || w.contains(&id)));
            if !taken {
                return id;
            }
        }
    }

    pub fn add_client(&mut self, name: String, passive: bool) -> ClientId {
        let id = self.next_client;
        self.next_client += 1;
        self.clients.insert(id, ClientEntry::new(name, passive));
        id
    }

    pub fn remove_client(&mut self, id: ClientId) {
        let watched: HashSet<BlockId> = self.clients.remove(&id).map(|c| c.streams.into_values().map(|(b, _, _)| b).collect()).unwrap_or_default();
        self.tickets.retain(|_, t| t.client_id != id);
        self.watchers_changed(watched);
        let mut disowned = Vec::new();
        for (block_id, block) in &mut self.blocks {
            match block {
                Block::Terminal(handle) => handle.send(BlockMsg::ClientGone { client_id: id }),
                Block::Browser(browser) => {
                    if browser.disown(id) {
                        disowned.push(block_id.clone());
                    }
                }
                Block::Agent(_) | Block::Files(_) | Block::Changes(_) => {}
            }
        }
        for block_id in disowned {
            self.block_changed(&block_id);
        }
    }

    pub fn client_name(&self, id: ClientId) -> String {
        self.clients.get(&id).map(|c| c.name.clone()).unwrap_or_default()
    }

    pub fn client_label(&self, id: ClientId) -> String {
        self.clients.get(&id).map(|c| c.label.clone()).unwrap_or_default()
    }

    /// The session of a session id or name.
    pub fn session_id(&self, id: &str) -> Option<SessionId> {
        self.session(id).ok().map(|s| s.id.clone())
    }

    /// The session holding window `window`.
    pub fn window_session(&self, window: &str) -> Option<SessionId> {
        self.locate_window(window).ok().map(|(si, _)| self.sessions[si].id.clone())
    }

    /// The session holding block `block`, in its tree or parked.
    pub fn block_session(&self, block: &str) -> Option<SessionId> {
        self.session_index(block).map(|si| self.sessions[si].id.clone())
    }

    /// Who sees `event`.
    fn visibility(&self, event: &Event) -> Visibility {
        let block = |id: &str| Visibility::Block { block: id.to_owned(), session: self.block_session(id) };
        match event {
            Event::SessionsChanged { .. } => Visibility::Sessions,
            Event::SessionChanged { view } => Visibility::Session(view.session.id.clone()),
            Event::SessionClosed { session_id } | Event::OpenSession { session_id } | Event::SessionWatchers { session_id, .. } => {
                Visibility::Session(session_id.clone())
            }
            Event::BlockChanged { block } => Visibility::Block { block: block.id.clone(), session: Some(block.session_id.clone()) },
            Event::BlockAttention { block_id, session_id, .. } | Event::CommandFinished { block_id, session_id, .. } => {
                Visibility::Block { block: block_id.clone(), session: Some(session_id.clone()) }
            }
            Event::BlockBell { block_id }
            | Event::BrowserCommand { block_id, .. }
            | Event::FilesChanged { block_id, .. }
            | Event::ChangesChanged { block_id }
            | Event::ChangesProgress { block_id, .. } => block(block_id),
            Event::BlockStatus { status } => block(&status.block_id),
            Event::HostOffered { .. } | Event::GrantsChanged { .. } => Visibility::Owner,
            // Sent with `emit_clients` to their recipients only.
            Event::ApprovalRequested { .. } | Event::ApprovalResolved { .. } | Event::AppRequest { .. } => Visibility::Clients(Vec::new()),
        }
    }

    pub fn emit(&mut self, event: Event) {
        let visibility = self.visibility(&event);
        self.emit_to(event, &visibility);
    }

    /// Emit to the owner only: the event carries secrets.
    pub fn emit_owner(&mut self, event: Event) {
        self.emit_to(event, &Visibility::Owner);
    }

    /// Emit to `clients` only.
    pub fn emit_clients(&mut self, event: Event, clients: Vec<ClientId>) {
        self.emit_to(event, &Visibility::Clients(clients));
    }

    fn emit_to(&mut self, event: Event, visibility: &Visibility) {
        let needs = needed_feature(&event);
        self.seq += 1;
        let seq = self.seq;
        // Session summaries are limited per principal; the full list is
        // shared by every principal that sees the whole server.
        let summaries = match &event {
            Event::SessionsChanged { sessions } => Some(sessions.clone()),
            _ => None,
        };
        if let Some(sessions) = &summaries {
            // What clients were last told, whichever change sent it.
            self.attention = sessions.iter().map(|s| (s.id.clone(), s.attention)).collect();
        }
        let message = ServerMessage::Event { seq: self.seq, event: Box::new(event) };
        let Ok(frame) = Frame::json(kind::CONTROL, &message) else {
            return;
        };
        for (&id, client) in self.clients.iter_mut().filter(|(id, c)| visibility.admits(**id, c) && c.supports(needs)) {
            let Some(sink) = &client.events else { continue };
            let frame = match &summaries {
                Some(sessions) if rights_on(&client.principal, On::Server).is_none() => {
                    let sessions = sessions.iter().filter(|s| sees_session(&client.principal, &s.id)).cloned().collect();
                    let message = ServerMessage::Event { seq, event: Box::new(Event::SessionsChanged { sessions }) };
                    let Ok(frame) = Frame::json(kind::CONTROL, &message) else { continue };
                    frame
                }
                _ => frame.clone(),
            };
            let overflow = sink.queued.load(std::sync::atomic::Ordering::Acquire) > EVENT_BACKLOG;
            if overflow || !sink.send(frame) {
                tracing::debug!(client = id, "event backlog exceeded; closing");
                client.events = None;
                if let Some(close) = client.close.take() {
                    let _ = close.send(());
                }
            }
        }
    }

    /// Close every client admitted with one of `grants`.
    pub fn close_grant_clients(&mut self, grants: &[String]) {
        for client in self.clients.values_mut() {
            if client.principal.grant_id().is_some_and(|g| grants.iter().any(|x| x == g)) {
                client.close_all();
            }
        }
    }

    /// Who may answer an approval: subscribed app clients that show
    /// approval cards (`approvals`). `local`: the owner's apps on the unix
    /// socket, outside every pane (never a program in a pane, nor a local
    /// token, claiming to be an app: either could approve its own call).
    /// `device`: when the caller is the paired device whose grant is
    /// `device`, that device's remote app connections, with the remote
    /// connection each runs on (`approval` keeps the ones binding the
    /// device key itself). No other remote client approves anything.
    pub fn approvers(&self, device: Option<&str>) -> (Vec<ClientId>, Vec<(ClientId, u64)>) {
        let apps = || self.clients.iter().filter(|(_, c)| c.role == ClientRole::App && c.events.is_some() && c.supports(Some(feature::APPROVALS)));
        let mut local: Vec<ClientId> =
            apps().filter(|(_, c)| c.origin.is_none() && c.pane.is_none() && c.principal == Principal::Owner).map(|(id, _)| *id).collect();
        local.sort_unstable();
        let mut remote: Vec<(ClientId, u64)> = device
            .map(|device| {
                apps()
                    .filter(|(_, c)| device_grant(&c.principal) == Some(device) && rights_on(&c.principal, On::Server).is_some())
                    .filter_map(|(id, c)| Some((*id, c.origin?)))
                    .collect()
            })
            .unwrap_or_default();
        remote.sort_unstable();
        (local, remote)
    }

    /// A data stream of `client` opened on `block`.
    pub fn add_stream(&mut self, client: ClientId, stream: u64, block: &str, rights: Rights, end: tokio::sync::oneshot::Sender<()>) {
        if let Some(entry) = self.clients.get_mut(&client) {
            entry.streams.insert(stream, (block.to_owned(), rights, Some(end)));
            self.watchers_changed([block.to_owned()].into());
        }
    }

    pub fn remove_stream(&mut self, client: ClientId, stream: u64) {
        let removed = self.clients.get_mut(&client).and_then(|c| c.streams.remove(&stream));
        if let Some((block, _, _)) = removed {
            self.watchers_changed([block].into());
        }
    }

    /// Clients with data streams on blocks of `session`.
    pub fn watchers(&self, session: &str) -> Vec<Watcher> {
        let Ok(session) = self.session(session) else { return Vec::new() };
        let blocks: HashSet<BlockId> = session.blocks().into_iter().collect();
        let mut watchers: Vec<Watcher> = self
            .clients
            .iter()
            .filter_map(|(&client_id, c)| {
                let mut watched: Vec<BlockId> = c.streams.values().filter(|(b, _, _)| blocks.contains(b)).map(|(b, _, _)| b.clone()).collect();
                if watched.is_empty() {
                    return None;
                }
                watched.sort();
                watched.dedup();
                let rights = c.streams.values().filter(|(b, _, _)| blocks.contains(b)).map(|(_, r, _)| *r).max().unwrap_or(Rights::View);
                Some(Watcher { client_id, label: c.label.clone(), principal: c.principal.watcher_kind(), rights, blocks: watched })
            })
            .collect();
        watchers.sort_by_key(|w| w.client_id);
        watchers
    }

    fn watchers_changed(&mut self, blocks: HashSet<BlockId>) {
        let sessions: HashSet<SessionId> = blocks.iter().filter_map(|b| self.block_session(b)).collect();
        for session_id in sessions {
            let watchers = self.watchers(&session_id);
            self.emit(Event::SessionWatchers { session_id, watchers });
        }
    }

    pub fn subscribe(&mut self, client: ClientId, sink: Sink) {
        if let Some(entry) = self.clients.get_mut(&client) {
            entry.events = Some(sink);
        }
    }

    pub fn summaries(&self) -> Vec<SessionSummary> {
        self.sessions
            .iter()
            .map(|s| SessionSummary {
                id: s.id.clone(),
                name: s.name.clone(),
                windows: s.windows.len(),
                blocks: s.blocks().len(),
                attached_clients: 0,
                created_at: s.created_at,
                attention: self.attention_counts(s),
            })
            .collect()
    }

    /// Status records per state across `session`'s blocks.
    fn attention_counts(&self, session: &Session) -> AttentionCounts {
        let statuses: Vec<BlockStatus> = session.blocks().iter().filter_map(|b| self.block_status(b)).collect();
        crate::status::counts(statuses.iter().flat_map(|s| &s.records))
    }

    /// A terminal or agent block's status, when it has any.
    pub fn block_status(&self, id: &str) -> Option<BlockStatus> {
        let status = self.blocks.get(id)?.status()?;
        (!status.records.is_empty() || status.progress.is_some() || status.busy.is_some()).then_some(status)
    }

    /// A block published a new status: send it, and the session summaries
    /// when its counts changed (never on progress alone; no revision bump).
    fn status_changed(&mut self, id: &str) {
        let Some(status) = self.blocks.get(id).and_then(Block::status) else { return };
        self.emit(Event::BlockStatus { status });
        let Some(si) = self.session_index(id) else { return };
        let counts = self.attention_counts(&self.sessions[si]);
        let session = self.sessions[si].id.clone();
        if self.attention.get(&session).copied().unwrap_or_default() != counts {
            self.attention.insert(session, counts);
            let sessions = self.summaries();
            self.emit(Event::SessionsChanged { sessions });
        }
    }

    /// [`Self::summaries`] of the sessions `principal` sees.
    pub fn summaries_for(&self, principal: &Principal) -> Vec<SessionSummary> {
        self.summaries().into_iter().filter(|s| sees_session(principal, &s.id)).collect()
    }

    fn session(&self, id: &str) -> Result<&Session, RpcError> {
        self.sessions.iter().find(|s| s.id == id || s.name == id).ok_or_else(|| RpcError::not_found(format!("session {id}")))
    }

    fn session_mut(&mut self, id: &str) -> Result<&mut Session, RpcError> {
        self.sessions.iter_mut().find(|s| s.id == id || s.name == id).ok_or_else(|| RpcError::not_found(format!("session {id}")))
    }

    pub fn view(&self, id: &str) -> Result<SessionView, RpcError> {
        let session = self.session(id)?;
        let blocks = session.blocks().iter().filter_map(|b| self.block_info(b)).collect();
        let status = session.blocks().iter().filter_map(|b| self.block_status(b)).collect();
        Ok(SessionView { session: session.info(), blocks, status, watchers: self.watchers(&session.id) })
    }

    pub fn block_info(&self, id: &str) -> Option<BlockInfo> {
        self.blocks.get(id).and_then(Block::info)
    }

    /// A terminal block; `bad_request` for a browser.
    pub fn block(&self, id: &str) -> Result<&BlockHandle, RpcError> {
        match self.blocks.get(id) {
            Some(Block::Terminal(handle)) => Ok(handle),
            Some(Block::Browser(_)) => Err(RpcError::bad_request(format!("block {id} is a browser"))),
            Some(Block::Agent(_)) => Err(RpcError::bad_request(format!("block {id} is an agent"))),
            Some(Block::Files(_)) => Err(RpcError::bad_request(format!("block {id} is a files block"))),
            Some(Block::Changes(_)) => Err(RpcError::bad_request(format!("block {id} is a changes block"))),
            None => Err(RpcError::not_found(format!("block {id}"))),
        }
    }

    /// An agent block; `bad_request` for another kind.
    fn agent(&mut self, id: &str) -> Result<&mut AgentBlock, RpcError> {
        match self.blocks.get_mut(id) {
            Some(Block::Agent(agent)) => Ok(agent),
            Some(_) => Err(RpcError::bad_request(format!("block {id} is not an agent"))),
            None => Err(RpcError::not_found(format!("block {id}"))),
        }
    }

    /// A browser block; `bad_request` for a terminal.
    fn browser(&mut self, id: &str) -> Result<&mut BrowserBlock, RpcError> {
        match self.blocks.get_mut(id) {
            Some(Block::Browser(browser)) => Ok(browser),
            Some(_) => Err(RpcError::bad_request(format!("block {id} is not a browser"))),
            None => Err(RpcError::not_found(format!("block {id}"))),
        }
    }

    fn block_changed(&mut self, id: &str) {
        self.mark_dirty(Some(id));
        if let Some(block) = self.block_info(id) {
            self.emit(Event::BlockChanged { block });
        }
    }

    /// Session and window containing `block`.
    fn locate(&self, block: &str) -> Result<(usize, usize), RpcError> {
        for (si, session) in self.sessions.iter().enumerate() {
            for (wi, window) in session.windows.iter().enumerate() {
                if window.contains(block) {
                    return Ok((si, wi));
                }
            }
        }
        Err(RpcError::not_found(format!("block {block}")))
    }

    /// Like [`Self::locate`]; `bad_request` for a float.
    fn locate_tiled(&self, block: &str) -> Result<(usize, usize), RpcError> {
        let (si, wi) = self.locate(block)?;
        if self.sessions[si].windows[wi].floating(block).is_some() {
            return Err(RpcError::bad_request(format!("block {block} is floating")));
        }
        Ok((si, wi))
    }

    fn locate_window(&self, window: &str) -> Result<(usize, usize), RpcError> {
        for (si, session) in self.sessions.iter().enumerate() {
            if let Some(wi) = session.windows.iter().position(|w| w.id == window) {
                return Ok((si, wi));
            }
        }
        Err(RpcError::not_found(format!("window {window}")))
    }

    fn spawn_block(&mut self, session_id: &str, new: &New<'_>, fallback: TermSize) -> Result<BlockId, RpcError> {
        // A newer client's kind: never stand in a shell for it.
        if new.kind == BlockKind::Unknown {
            return Err(RpcError::bad_request("unsupported block kind"));
        }
        let id = self.fresh_id('b');
        let spawn = new.spawn;
        let size = spawn.size.filter(|s| s.columns > 0 && s.rows > 0).unwrap_or(fallback);
        let info = BlockInfo {
            id: id.clone(),
            session_id: session_id.to_owned(),
            kind: BlockKind::Terminal,
            title: "shell".into(),
            cwd: spawn.cwd.clone(),
            exited: None,
            lifecycle: Lifecycle::Running,
            size,
            owner: None,
            owner_name: None,
            owner_block: None,
            pid: None,
            browser: None,
            agent: None,
            files: None,
            changes: None,
        };
        if new.kind == BlockKind::Files {
            let root = crate::files::paths::new_root(None, spawn.cwd.as_deref().unwrap_or("~"))?;
            let mut files = FilesBlock::new(info, FilesState::new(root.to_string_lossy().into_owned()));
            files.watch(self.notes.clone());
            self.blocks.insert(id.clone(), Block::Files(Box::new(files)));
            return Ok(id);
        }
        if new.kind == BlockKind::Changes {
            let dir = crate::files::paths::new_root(None, spawn.cwd.as_deref().unwrap_or("~"))?;
            let root = crate::changes::find_root(&dir);
            let mut changes = ChangesBlock::new(info, ChangesState::new(root.to_string_lossy().into_owned()));
            changes.watch(self.notes.clone());
            self.blocks.insert(id.clone(), Block::Changes(Box::new(changes)));
            return Ok(id);
        }
        if new.kind == BlockKind::Agent {
            if new.agent.is_some_and(|a| a.session_id.as_deref() == Some("")) {
                return Err(RpcError::bad_request("empty agent session_id"));
            }
            let info = BlockInfo { size: TermSize::default(), ..info };
            self.blocks.insert(id.clone(), Block::Agent(Box::new(AgentBlock::new(info, new.agent.cloned().unwrap_or_default()))));
            return Ok(id);
        }
        if new.kind == BlockKind::Browser {
            let url = browser_url(new.url.unwrap_or("about:blank"))
                .ok_or_else(|| RpcError::bad_request("empty url or unsupported scheme (http, https, ws, wss, data, about:blank, view-source)"))?;
            let info = BlockInfo { size: TermSize::default(), ..info };
            self.blocks.insert(id.clone(), Block::Browser(Box::new(BrowserBlock::new(info, BrowserState::new(url)))));
            return Ok(id);
        }
        let config = spawn_config(&id, &spawn.command, spawn.cwd.as_deref());
        let handle = block::spawn(info, &config, self.notes.clone()).map_err(RpcError::internal)?;
        self.blocks.insert(id.clone(), Block::Terminal(handle));
        Ok(id)
    }

    fn session_changed(&mut self, si: usize) {
        self.sessions[si].revision += 1;
        self.mark_dirty(None);
        let id = self.sessions[si].id.clone();
        if let Ok(view) = self.view(&id) {
            self.emit(Event::SessionChanged { view });
        }
    }

    fn sessions_changed(&mut self) {
        self.mark_dirty(None);
        let sessions = self.summaries();
        self.emit(Event::SessionsChanged { sessions });
    }

    pub fn create_session(&mut self, name: Option<String>, new: &New<'_>) -> Result<Created, RpcError> {
        let name = name.filter(|n| !n.is_empty()).unwrap_or_else(|| crate::names::generate(|n| self.sessions.iter().any(|s| s.name == n)));
        // Before the block exists: a conflict closes nothing (an agent block's close hands its sub-agent back).
        if self.sessions.iter().any(|s| s.name == name) {
            return Err(RpcError::new("conflict", format!("session {name} already exists")));
        }
        let session_id = self.fresh_id('s');
        let block_id = self.spawn_block(&session_id, new, TermSize::new(80, 24))?;
        let window_id = self.fresh_id('w');
        self.sessions.push(Session {
            id: session_id.clone(),
            name,
            windows: vec![Window::new(window_id.clone(), &block_id)],
            active_window: window_id.clone(),
            revision: 1,
            created_at: now_ms(),
            parked: Vec::new(),
        });
        self.sessions_changed();
        let si = self.sessions.len() - 1;
        self.session_changed(si);
        Ok(Created { session_id, window_id, block_id })
    }

    /// A new window (tab) of one block, made active unless `background`.
    pub fn create_window(&mut self, session_id: &str, new: &New<'_>, background: bool) -> Result<Created, RpcError> {
        let session = self.session(session_id)?;
        let sid = session.id.clone();
        let fallback = session.windows.iter().find(|w| w.id == session.active_window).map(|w| self.window_size(w)).unwrap_or(TermSize::new(80, 24));
        // A files or changes block opens in the folder of the block in focus.
        let mut spawn = new.spawn.clone();
        if matches!(new.kind, BlockKind::Files | BlockKind::Changes) && spawn.cwd.is_none() {
            let focused = session.windows.iter().find(|w| w.id == session.active_window).map(|w| w.focused.clone());
            spawn.cwd = focused.and_then(|b| self.block_info(&b)).and_then(|i| i.cwd);
        }
        let block_id = self.spawn_block(&sid, &New { spawn: &spawn, ..*new }, fallback)?;
        let window_id = self.fresh_id('w');
        let session = self.session_mut(&sid)?;
        session.windows.push(Window::new(window_id.clone(), &block_id));
        if !background {
            session.active_window = window_id.clone();
        }
        let si = self.sessions.iter().position(|s| s.id == sid).unwrap_or(0);
        self.session_changed(si);
        self.sessions_changed();
        Ok(Created { session_id: sid, window_id, block_id })
    }

    /// Approximate full-window grid from the window's blocks.
    fn window_size(&self, window: &Window) -> TermSize {
        fn size(layout: &Layout, reg: &Registry) -> Option<TermSize> {
            match layout {
                Layout::Block { id } => reg.block_info(id).filter(|i| i.kind == BlockKind::Terminal).map(|i| i.size),
                Layout::Unknown => None,
                Layout::Split { axis, first, second, .. } => {
                    let (a, b) = (size(first, reg)?, size(second, reg)?);
                    Some(match axis {
                        Axis::Horizontal => TermSize { columns: a.columns + b.columns + 1, rows: a.rows.max(b.rows), ..a },
                        Axis::Vertical => TermSize { columns: a.columns.max(b.columns), rows: a.rows + b.rows + 1, ..a },
                    })
                }
            }
        }
        let float = || window.floats.iter().rev().find_map(|f| size(&Layout::block(&f.id), self));
        size(&window.layout, self).or_else(float).unwrap_or(TermSize::new(80, 24))
    }

    pub fn split(&mut self, block_id: &str, axis: Axis, new: &New<'_>) -> Result<Created, RpcError> {
        let (si, wi) = self.locate_tiled(block_id)?;
        let sid = self.sessions[si].id.clone();
        let base = self.block_info(block_id).map(|i| i.size).filter(|s| s.columns > 0 && s.rows > 0).unwrap_or(TermSize::new(80, 24));
        let fallback = match axis {
            Axis::Horizontal => TermSize { columns: (base.columns / 2).max(2), ..base },
            Axis::Vertical => TermSize { rows: (base.rows / 2).max(2), ..base },
        };
        let mut spawn = new.spawn.clone();
        if spawn.cwd.is_none() {
            spawn.cwd = self.block_info(block_id).and_then(|i| i.cwd);
        }
        let new = self.spawn_block(&sid, &New { spawn: &spawn, ..*new }, fallback)?;
        // A review split off an agent goes back to it.
        let agent = match self.blocks.get(block_id) {
            Some(Block::Agent(agent)) => agent.info().agent.and_then(|a| a.session_id),
            _ => None,
        };
        if let (Some(otto_session), Some(Block::Changes(changes))) = (agent, self.blocks.get_mut(&new)) {
            changes.link(AgentLink { otto_session, block_id: Some(block_id.to_owned()) });
        }
        let window = &mut self.sessions[si].windows[wi];
        window.layout.split(block_id, new.clone(), axis);
        window.focus(&new);
        window.zoomed = false;
        let window_id = window.id.clone();
        self.session_changed(si);
        Ok(Created { session_id: sid, window_id, block_id: new })
    }

    /// `block.spawn`: a parked terminal block in the session of `owner`,
    /// owned by it; a `command` makes it a command block.
    pub fn spawn_parked(
        &mut self,
        owner: &str,
        command: &[String],
        cwd: Option<String>,
        env: impl IntoIterator<Item = (String, String)>,
        size: Option<TermSize>,
    ) -> Result<Created, RpcError> {
        let si = self.session_index(owner).ok_or_else(|| RpcError::not_found(format!("block {owner}")))?;
        let session_id = self.sessions[si].id.clone();
        let id = self.fresh_id('b');
        let cwd = cwd.or_else(|| self.block_info(owner).and_then(|i| i.cwd));
        let info = BlockInfo {
            id: id.clone(),
            session_id: session_id.clone(),
            kind: BlockKind::Terminal,
            title: "shell".into(),
            cwd: cwd.clone(),
            exited: None,
            lifecycle: Lifecycle::Running,
            size: size.filter(|s| s.columns > 0 && s.rows > 0).unwrap_or(PARKED_SIZE),
            owner: None,
            owner_name: None,
            owner_block: Some(owner.to_owned()),
            pid: None,
            browser: None,
            agent: None,
            files: None,
            changes: None,
        };
        let mut config = spawn_config(&id, command, cwd.as_deref());
        config.env.extend(env.into_iter().map(|(k, v)| (k.into(), v.into())));
        let handle = block::spawn(info, &config, self.notes.clone()).map_err(RpcError::internal)?;
        self.blocks.insert(id.clone(), Block::Terminal(handle));
        self.sessions[si].parked.push(Parked { id: id.clone(), owner: Some(owner.to_owned()) });
        self.session_changed(si);
        self.sessions_changed();
        Ok(Created { session_id, window_id: String::new(), block_id: id })
    }

    /// `block.spawn` with `kind: browser`: a parked browser at `url` in the
    /// session of `owner`, owned by it. No app runs its page until an
    /// action reaches it (the off-screen host) or it is revealed.
    pub fn spawn_parked_browser(&mut self, owner: &str, url: Option<&str>) -> Result<Created, RpcError> {
        let si = self.session_index(owner).ok_or_else(|| RpcError::not_found(format!("block {owner}")))?;
        let session_id = self.sessions[si].id.clone();
        let new = New { spawn: &SpawnSpec::default(), kind: BlockKind::Browser, url, agent: None };
        let id = self.spawn_block(&session_id, &new, PARKED_SIZE)?;
        if let Some(Block::Browser(browser)) = self.blocks.get_mut(&id) {
            browser.set_owner_block(owner);
        }
        self.sessions[si].parked.push(Parked { id: id.clone(), owner: Some(owner.to_owned()) });
        self.session_changed(si);
        self.sessions_changed();
        Ok(Created { session_id, window_id: String::new(), block_id: id })
    }

    /// `block.park`: take a block out of its window; it keeps running. The
    /// only block of a window stays.
    pub fn park(&mut self, block_id: &str) -> Result<(), RpcError> {
        let (si, wi) = self.locate(block_id)?;
        if self.sessions[si].windows[wi].blocks().len() == 1 {
            return Err(RpcError::new("conflict", "the only block of a window cannot be parked"));
        }
        // An owned block (`block.spawn`) keeps its owner when revealed and
        // parked again.
        let owner = self.block_info(block_id).and_then(|i| i.owner_block);
        self.sessions[si].parked.push(Parked { id: block_id.to_owned(), owner });
        self.detach(si, wi, block_id);
        Ok(())
    }

    /// `block.reveal`: show a parked block in its owner's window (else the
    /// active one), floating or split next to the owner (else the focused
    /// tiled block), focused.
    pub fn reveal(&mut self, block_id: &str, reveal: Reveal) -> Result<(), RpcError> {
        let si = self
            .sessions
            .iter()
            .position(|s| s.is_parked(block_id))
            .ok_or_else(|| RpcError::bad_request(format!("block {block_id} is not parked")))?;
        let session = &mut self.sessions[si];
        let index = session.parked.iter().position(|p| p.id == block_id).expect("parked");
        // The owner stays on the block (`BlockInfo::owner_block`).
        let owner = session.parked.remove(index).owner;
        let wi = owner
            .as_deref()
            .and_then(|o| session.windows.iter().position(|w| w.contains(o)))
            .or_else(|| session.windows.iter().position(|w| w.id == session.active_window))
            .unwrap_or(0);
        let window = &mut session.windows[wi];
        match reveal {
            Reveal::Float => window.floats.push(Float { id: block_id.to_owned(), rect: FloatRect::default() }),
            Reveal::Split => {
                let tiled = window.layout.blocks();
                let target = owner
                    .filter(|o| tiled.contains(o))
                    .or_else(|| tiled.contains(&window.focused).then(|| window.focused.clone()))
                    .unwrap_or_else(|| tiled[0].clone());
                window.layout.split(&target, block_id.to_owned(), Axis::Vertical);
                window.zoomed = false;
            }
        }
        window.focus(block_id);
        session.active_window = window.id.clone();
        self.session_changed(si);
        Ok(())
    }

    /// Index of the session holding `block`, in a window or parked.
    fn session_index(&self, block: &str) -> Option<usize> {
        self.locate(block).ok().map(|(si, _)| si).or_else(|| self.sessions.iter().position(|s| s.is_parked(block)))
    }

    /// Close the blocks `owner` started (`block.spawn`): every parked one,
    /// and shown terminals still running their program, so nothing an agent
    /// ran outlives it, even after the user revealed it. A shown block that
    /// exited (its output) or a shown browser stays for the user.
    fn close_owned(&mut self, owner: &str) {
        let parked = self.sessions.iter().flat_map(|s| &s.parked).filter(|p| p.owner.as_deref() == Some(owner)).map(|p| p.id.clone());
        let running = |b: &Block| {
            b.terminal()
                .and_then(|h| h.info.lock().ok().map(|i| i.owner_block.as_deref() == Some(owner) && i.lifecycle.is_running()))
                .unwrap_or(false)
        };
        let shown = self.blocks.iter().filter(|(_, b)| running(b)).map(|(id, _)| id.clone());
        let mut owned: Vec<BlockId> = parked.chain(shown).collect();
        owned.sort();
        owned.dedup();
        for id in owned {
            self.kill_block(&id);
            self.remove_block(&id);
        }
    }

    /// Keep the final capture of exited block `id`, if it has one; true when
    /// the block is parked.
    fn keep_ended(&mut self, id: &str) -> bool {
        let capture = match self.blocks.get(id) {
            Some(Block::Terminal(handle)) => handle.ended.lock().unwrap().take(),
            _ => None,
        };
        if let Some(capture) = capture {
            self.ended.retain(|_, (_, at)| at.elapsed() < ENDED_TTL);
            if self.ended.len() >= ENDED_MAX
                && let Some(oldest) = self.ended.iter().min_by_key(|(_, (_, at))| *at).map(|(id, _)| id.clone())
            {
                self.ended.remove(&oldest);
            }
            self.ended.insert(id.to_owned(), (capture, Instant::now()));
        }
        self.sessions.iter().any(|s| s.is_parked(id))
    }

    /// The final capture of a command block that exited.
    pub fn ended(&mut self, block_id: &str) -> Option<Capture> {
        self.ended.retain(|_, (_, at)| at.elapsed() < ENDED_TTL);
        self.ended.get(block_id).map(|(capture, _)| capture.clone())
    }

    pub fn focus(&mut self, block_id: &str) -> Result<(), RpcError> {
        let (si, wi) = self.locate(block_id)?;
        let session = &mut self.sessions[si];
        let window = &mut session.windows[wi];
        let mut changed = window.focused != block_id || session.active_window != window.id;
        if let Some(i) = window.floating(block_id).filter(|&i| i + 1 != window.floats.len()) {
            let float = window.floats.remove(i);
            window.floats.push(float);
            changed = true;
        }
        window.focus(block_id);
        session.active_window = window.id.clone();
        if changed {
            self.session_changed(si);
        }
        Ok(())
    }

    pub fn zoom(&mut self, block_id: &str, zoomed: bool) -> Result<(), RpcError> {
        let (si, wi) = self.locate_tiled(block_id)?;
        let window = &mut self.sessions[si].windows[wi];
        window.focus(block_id);
        window.zoomed = zoomed;
        self.session_changed(si);
        Ok(())
    }

    /// Exchange the positions of two blocks of one window. Ratios stay, so
    /// each block's grid changes only if the two slots differ in size.
    pub fn swap(&mut self, block_id: &str, other_block_id: &str) -> Result<(), RpcError> {
        let (si, wi) = self.locate_tiled(block_id)?;
        let other = self.locate_tiled(other_block_id)?;
        if other != (si, wi) {
            return Err(RpcError::bad_request(format!("blocks {block_id} and {other_block_id} are in different windows")));
        }
        if block_id == other_block_id {
            return Err(RpcError::bad_request("cannot swap a block with itself"));
        }
        self.sessions[si].windows[wi].layout.swap(block_id, other_block_id);
        self.session_changed(si);
        Ok(())
    }

    /// Move a tiled block out of the layout into its window's floats, on
    /// top and focused.
    pub fn float(&mut self, block_id: &str, rect: Option<FloatRect>) -> Result<(), RpcError> {
        let (si, wi) = self.locate_tiled(block_id)?;
        let window = &mut self.sessions[si].windows[wi];
        if matches!(window.layout, Layout::Block { .. }) {
            return Err(RpcError::new("conflict", "the last tiled block cannot float"));
        }
        let layout = std::mem::replace(&mut window.layout, Layout::block(""));
        window.layout = layout.remove(block_id).expect("a split keeps a leaf");
        window.floats.push(Float { id: block_id.to_owned(), rect: rect.unwrap_or_default().clamped() });
        window.focus(block_id);
        window.zoomed = false;
        self.session_changed(si);
        Ok(())
    }

    /// Set a float's rect and raise it, or dock a float or tiled block on a
    /// side of another tiled block of its window (a tiled one leaves its
    /// place first, its parent split collapsing). Docking focuses the block
    /// and unzooms.
    pub fn move_block(&mut self, block_id: &str, rect: Option<FloatRect>, dock: Option<Dock>) -> Result<(), RpcError> {
        let (si, wi) = self.locate(block_id)?;
        let window = &mut self.sessions[si].windows[wi];
        let floating = window.floating(block_id);
        match (rect, dock) {
            (Some(rect), None) => {
                let index = floating.ok_or_else(|| RpcError::bad_request(format!("block {block_id} is not floating")))?;
                window.floats.remove(index);
                window.floats.push(Float { id: block_id.to_owned(), rect: rect.clamped() });
            }
            (None, Some(Dock { target, axis, before })) => {
                if target == block_id {
                    return Err(RpcError::bad_request("cannot dock a block next to itself"));
                }
                if !window.layout.contains(&target) {
                    return Err(RpcError::bad_request(format!("block {target} is not tiled in this window")));
                }
                if let Some(index) = floating {
                    window.floats.remove(index);
                    window.layout.split_at(&target, block_id.to_owned(), axis, before);
                } else if !window.layout.dock(block_id, &target, axis, before) {
                    return Err(RpcError::bad_request(format!("block {block_id} is not tiled in this window")));
                }
                window.focus(block_id);
                window.zoomed = false;
            }
            _ => return Err(RpcError::bad_request("exactly one of rect and dock")),
        }
        self.session_changed(si);
        Ok(())
    }

    pub fn set_ratio(&mut self, window_id: &str, path: &[usize], ratio: f32) -> Result<(), RpcError> {
        let (si, wi) = self.locate_window(window_id)?;
        if !self.sessions[si].windows[wi].layout.set_ratio(path, ratio) {
            return Err(RpcError::bad_request("path does not name a split"));
        }
        self.session_changed(si);
        Ok(())
    }

    pub fn select_window(&mut self, session_id: &str, window_id: &str) -> Result<(), RpcError> {
        let session = self.session_mut(session_id)?;
        if !session.windows.iter().any(|w| w.id == window_id) {
            return Err(RpcError::not_found(format!("window {window_id}")));
        }
        session.active_window = window_id.to_owned();
        let sid = session.id.clone();
        let si = self.sessions.iter().position(|s| s.id == sid).unwrap_or(0);
        self.session_changed(si);
        Ok(())
    }

    pub fn rename_window(&mut self, window_id: &str, name: Option<String>) -> Result<(), RpcError> {
        let (si, wi) = self.locate_window(window_id)?;
        self.sessions[si].windows[wi].name = name.filter(|n| !n.is_empty());
        self.session_changed(si);
        Ok(())
    }

    pub fn move_window(&mut self, window_id: &str, index: usize) -> Result<(), RpcError> {
        let (si, wi) = self.locate_window(window_id)?;
        let windows = &mut self.sessions[si].windows;
        let window = windows.remove(wi);
        let index = index.min(windows.len());
        windows.insert(index, window);
        self.session_changed(si);
        Ok(())
    }

    /// Move every block of window `source` into window `target` of the same
    /// session and close `source`: its layout becomes the second side of a
    /// split with the target's along `axis` (arrangement and ratios kept),
    /// its floats are added on top, and the block it had focused takes
    /// focus. The target becomes the active window. No block restarts.
    pub fn merge_window(&mut self, target: &str, source: &str, axis: Axis) -> Result<(), RpcError> {
        if target == source {
            return Err(RpcError::bad_request("cannot merge a window into itself"));
        }
        let (si, ti) = self.locate_window(target)?;
        let (source_si, wi) = self.locate_window(source)?;
        if source_si != si {
            return Err(RpcError::bad_request("windows of different sessions cannot merge"));
        }
        let session = &mut self.sessions[si];
        let moved = session.windows.remove(wi);
        let ti = if wi < ti { ti - 1 } else { ti };
        let window = &mut session.windows[ti];
        let layout = std::mem::replace(&mut window.layout, Layout::Unknown);
        window.layout = Layout::Split { axis, ratio: 0.5, first: Box::new(layout), second: Box::new(moved.layout) };
        window.floats.extend(moved.floats);
        window.zoomed = false;
        // Its focus history after the target's, the block it had focused in front.
        window.history.extend(moved.history.into_iter().filter(|b| b != &moved.focused));
        window.focus(&moved.focused);
        session.active_window = target.to_owned();
        self.session_changed(si);
        self.sessions_changed();
        Ok(())
    }

    pub fn rename_session(&mut self, session_id: &str, name: String) -> Result<(), RpcError> {
        if name.is_empty() {
            return Err(RpcError::bad_request("empty name"));
        }
        if self.sessions.iter().any(|s| s.name == name && s.id != session_id) {
            return Err(RpcError::new("conflict", format!("session {name} already exists")));
        }
        self.session_mut(session_id)?.name = name;
        let sid = self.session(session_id)?.id.clone();
        let si = self.sessions.iter().position(|s| s.id == sid).unwrap_or(0);
        self.session_changed(si);
        self.sessions_changed();
        Ok(())
    }

    pub fn close_window(&mut self, window_id: &str) -> Result<(), RpcError> {
        let (si, wi) = self.locate_window(window_id)?;
        let blocks = self.sessions[si].windows[wi].blocks();
        for block in &blocks {
            self.close_owned(block);
        }
        let (si, wi) = self.locate_window(window_id)?;
        for block in blocks {
            self.kill_block(&block);
        }
        self.drop_window(si, wi);
        Ok(())
    }

    pub fn close_session(&mut self, session_id: &str) -> Result<(), RpcError> {
        let sid = self.session(session_id)?.id.clone();
        let si = self.sessions.iter().position(|s| s.id == sid).unwrap_or(0);
        for block in self.sessions[si].blocks() {
            self.kill_block(&block);
        }
        self.sessions.remove(si);
        self.gone.push(Scope::Session { session_id: sid.clone() });
        self.emit(Event::SessionClosed { session_id: sid });
        self.sessions_changed();
        self.keep_one();
        Ok(())
    }

    pub fn close_block(&mut self, block_id: &str) -> Result<(), RpcError> {
        self.session_index(block_id).ok_or_else(|| RpcError::not_found(format!("block {block_id}")))?;
        self.close_owned(block_id);
        self.kill_block(block_id);
        self.remove_block(block_id);
        Ok(())
    }

    fn kill_block(&mut self, block_id: &str) {
        // Closing a block hangs up its program, parked or not.
        self.parked.remove(block_id);
        self.gone.push(Scope::Block { block_id: block_id.to_owned() });
        if let Some(Block::Terminal(handle)) = self.take_block(block_id) {
            handle.send(BlockMsg::Shutdown);
        }
    }

    /// Take `block_id` out of the blocks; a closed agent block reports the
    /// otto session it showed ([`crate::Config::agent_closed`]) unless
    /// another agent block still shows it.
    fn take_block(&mut self, block_id: &str) -> Option<Block> {
        let block = self.blocks.remove(block_id);
        if let (Some(Block::Agent(agent)), Some(closed)) = (&block, &self.agent_closed)
            && let Some(session) = agent.session_id()
            && !self.blocks.values().any(|b| matches!(b, Block::Agent(a) if a.session_id() == Some(session)))
        {
            closed(session.to_owned());
        }
        block
    }

    /// Remove a closed block from its session's parked blocks, or from the
    /// layout or floats (see [`Self::detach`]).
    pub fn remove_block(&mut self, block_id: &str) {
        self.take_block(block_id);
        let scope = Scope::Block { block_id: block_id.to_owned() };
        if !self.gone.contains(&scope) {
            self.gone.push(scope);
        }
        if let Some(si) = self.sessions.iter().position(|s| s.is_parked(block_id)) {
            self.sessions[si].parked.retain(|p| p.id != block_id);
            self.session_changed(si);
            self.sessions_changed();
            return;
        }
        let Ok((si, wi)) = self.locate(block_id) else {
            return;
        };
        self.detach(si, wi, block_id);
    }

    /// Take `block_id` out of window `wi` of session `si`, collapsing empty
    /// windows and sessions. A tree emptied while floats remain takes the
    /// topmost float as its root. Focus returns to the most recently focused
    /// remaining block, else the block taking over the closed one's space.
    fn detach(&mut self, si: usize, wi: usize, block_id: &str) {
        let window = &mut self.sessions[si].windows[wi];
        let sibling = sibling(&window.layout, block_id);
        if let Some(i) = window.floating(block_id) {
            window.floats.remove(i);
        } else {
            let layout = std::mem::replace(&mut window.layout, Layout::block(""));
            match (layout.remove(block_id), window.floats.pop()) {
                (Some(layout), float) => {
                    window.layout = layout;
                    window.floats.extend(float);
                }
                (None, Some(float)) => window.layout = Layout::block(float.id),
                (None, None) => return self.drop_window(si, wi),
            }
        }
        window.history.retain(|b| b != block_id);
        let tiled = window.layout.blocks();
        if window.focused == block_id {
            let next = window
                .history
                .iter()
                .find(|b| window.contains(b))
                .cloned()
                .or(sibling)
                .or_else(|| window.floats.last().map(|f| f.id.clone()))
                .unwrap_or_else(|| tiled[0].clone());
            window.focus(&next);
        }
        window.zoomed &= tiled.len() > 1;
        self.session_changed(si);
    }

    fn drop_window(&mut self, si: usize, wi: usize) {
        let session = &mut self.sessions[si];
        let removed = session.windows.remove(wi);
        if session.windows.is_empty() {
            let sid = session.id.clone();
            let parked = std::mem::take(&mut session.parked);
            for parked in parked {
                self.kill_block(&parked.id);
            }
            self.sessions.remove(si);
            self.gone.push(Scope::Session { session_id: sid.clone() });
            self.emit(Event::SessionClosed { session_id: sid });
        } else {
            if session.active_window == removed.id {
                session.active_window = session.windows[wi.min(session.windows.len() - 1)].id.clone();
            }
            self.session_changed(si);
        }
        self.sessions_changed();
        self.keep_one();
    }

    /// A data ticket for `block_id`, whose streams get `rights`.
    pub fn issue_ticket(&mut self, client_id: ClientId, block_id: &str, rights: Rights) -> Result<DataTicket, RpcError> {
        // Terminals (output, uploads) and files blocks (uploads, downloads).
        if !matches!(self.blocks.get(block_id), Some(Block::Files(_))) {
            self.block(block_id)?;
        }
        let now = Instant::now();
        self.tickets.retain(|_, t| t.expires > now);
        let ticket = ticket_hex(&rand::random::<[u8; 16]>());
        let origin = self.clients.get(&client_id).and_then(|c| c.origin);
        self.tickets.insert(ticket.clone(), Ticket { block_id: block_id.to_owned(), client_id, origin, rights, expires: now + TICKET_TTL });
        Ok(DataTicket { ticket, expires_in_ms: TICKET_TTL.as_millis() as u64 })
    }

    /// Consume a ticket presented on `origin`: `(block, client, rights)`. A
    /// ticket issued to another connection is refused and left unconsumed.
    pub fn redeem(&mut self, ticket: &str, origin: Option<u64>) -> Option<(BlockId, ClientId, Rights)> {
        if self.tickets.get(ticket)?.origin != origin {
            return None;
        }
        let t = self.tickets.remove(ticket)?;
        (t.expires > Instant::now() && self.clients.contains_key(&t.client_id)).then_some((t.block_id, t.client_id, t.rights))
    }

    pub fn handle_note(&mut self, note: Note) {
        match note {
            Note::Changed(id) => {
                self.mark_dirty(Some(&id));
                if let Some(block) = self.block_info(&id) {
                    self.emit(Event::BlockChanged { block });
                }
            }
            Note::Bell(id) => self.emit(Event::BlockBell { block_id: id }),
            Note::Status(id) => self.status_changed(&id),
            Note::Attention { block_id, reason, title, body, at } => {
                let Some(session_id) = self.block_session(&block_id) else { return };
                let session_name = self.session(&session_id).map(|s| s.name.clone()).unwrap_or_default();
                let block_title = self.block_info(&block_id).map(|i| untrusted(&i.title, MAX_TITLE)).unwrap_or_default();
                self.emit(Event::BlockAttention { block_id, session_id, session_name, block_title, reason, title, body, at });
            }
            Note::CommandFinished { block_id, record } => {
                if let Some(session_id) = self.block_session(&block_id) {
                    self.emit(Event::CommandFinished { block_id, session_id, record });
                }
            }
            Note::Offer(offer) => self.emit_owner(Event::HostOffered { offer }),
            Note::Dirty(id) => self.mark_block_dirty(&id),
            Note::FilesChanged { block_id, root, paths } => {
                let Some(Block::Files(files)) = self.blocks.get(&block_id) else { return };
                if files.root() != root {
                    return;
                }
                let (paths, truncated) = crate::files::changed(&root, paths);
                if !paths.is_empty() || truncated {
                    self.emit(Event::FilesChanged { block_id, paths, truncated });
                }
            }
            Note::ChangesChanged { block_id, root } => {
                if matches!(self.blocks.get(&block_id), Some(Block::Changes(c)) if c.root() == root) {
                    self.emit(Event::ChangesChanged { block_id });
                }
            }
            Note::Exited(id) if self.keep_ended(&id) => {
                // Nobody sees a parked block: it closes when its program
                // ends, after its exit went out.
                if let Some(block) = self.block_info(&id) {
                    self.emit(Event::BlockChanged { block });
                }
                let _ = self.close_block(&id);
            }
            Note::Exited(id) if self.blocks.get(&id).is_some_and(exited_cleanly) => {
                let _ = self.close_block(&id);
            }
            // A failed program stays in its layout, read-only, until closed.
            Note::Exited(id) => {
                self.mark_dirty(Some(&id));
                if let Some(block) = self.block_info(&id) {
                    self.emit(Event::BlockChanged { block });
                }
            }
        }
    }

    pub fn shutdown(&mut self) {
        for (_, block) in self.blocks.drain() {
            if let Block::Terminal(handle) = block {
                handle.send(BlockMsg::Shutdown);
            }
        }
        self.parked.clear();
        self.sessions.clear();
    }

    /// Take or request (`Some(mode)`) or release (`None`) ownership of
    /// blocks; a terminal also drops this client's candidacy on release.
    pub fn own(&mut self, client_id: ClientId, block_ids: &[BlockId], mode: Option<OwnMode>) -> Result<(), RpcError> {
        let label = self.client_label(client_id);
        for id in block_ids {
            let changed = match (self.blocks.get_mut(id), mode) {
                (Some(Block::Terminal(handle)), mode) => {
                    handle.send(mode.map_or(BlockMsg::Disown { client_id }, |mode| BlockMsg::Own { client_id, mode }));
                    false
                }
                (Some(Block::Browser(browser)), Some(mode)) => browser.own(client_id, label.clone(), mode),
                (Some(Block::Browser(browser)), None) => browser.disown(client_id),
                (Some(Block::Agent(_) | Block::Files(_) | Block::Changes(_)), _) => false,
                (None, _) => return Err(RpcError::not_found(format!("block {id}"))),
            };
            if changed {
                self.block_changed(id);
            }
        }
        Ok(())
    }

    /// Apply a browser command to the stored state and send it to every
    /// subscriber (the owner's engine runs it; viewers follow).
    pub fn browser_command(&mut self, block_id: &str, mut command: BrowserCommand) -> Result<(), RpcError> {
        if let BrowserCommand::Navigate { url } = &mut command {
            *url = browser_url(url)
                .ok_or_else(|| RpcError::bad_request("empty url or unsupported scheme (http, https, ws, wss, data, about:blank, view-source)"))?;
        }
        self.browser(block_id)?.command(&command)?;
        self.emit(Event::BrowserCommand { block_id: block_id.to_owned(), command });
        self.block_changed(block_id);
        Ok(())
    }

    /// Store engine state reported by the block's owner.
    pub fn browser_report(&mut self, client_id: ClientId, block_id: &str, report: Report) -> Result<(), RpcError> {
        self.browser(block_id)?.report(client_id, report)?;
        self.block_changed(block_id);
        Ok(())
    }

    /// `agent.set`: the shown otto session and its title, or the block's folder.
    pub fn agent_set(&mut self, block_id: &str, session_id: Option<String>, title: Option<String>, cwd: Option<String>) -> Result<(), RpcError> {
        if self.agent(block_id)?.set(session_id, title, cwd)? {
            self.block_changed(block_id);
            if let Ok((si, _)) = self.locate(block_id) {
                self.session_changed(si);
            }
        }
        Ok(())
    }

    /// `block.report` on an agent block.
    pub fn agent_report(&mut self, block_id: &str, report: crate::status::Report) -> Result<(), RpcError> {
        if let Some((reason, title, body)) = self.agent(block_id)?.report(report, block::now_ms()) {
            self.handle_note(Note::Attention { block_id: block_id.to_owned(), reason, title, body, at: block::now_ms() });
        }
        self.status_changed(block_id);
        Ok(())
    }

    /// `block.status_seen` on an agent block.
    pub fn agent_seen(&mut self, block_id: &str, ids: Option<&[String]>) -> Result<(), RpcError> {
        if self.agent(block_id)?.seen(ids) {
            self.status_changed(block_id);
        }
        Ok(())
    }

    /// A files block; `bad_request` for another kind.
    fn files(&mut self, id: &str) -> Result<&mut FilesBlock, RpcError> {
        match self.blocks.get_mut(id) {
            Some(Block::Files(files)) => Ok(files),
            Some(_) => Err(RpcError::bad_request(format!("block {id} is not a files block"))),
            None => Err(RpcError::not_found(format!("block {id}"))),
        }
    }

    /// `files.set`: the block's new view state.
    pub fn files_set(&mut self, params: FilesSetParams) -> Result<FilesState, RpcError> {
        let block_id = params.block_id.clone();
        let files = self.files(&block_id)?;
        let changed = files.set(params)?;
        let state = files.state().clone();
        if changed {
            self.block_changed(&block_id);
        }
        Ok(state)
    }

    /// A changes block; `bad_request` for another kind.
    fn changes(&mut self, id: &str) -> Result<&mut ChangesBlock, RpcError> {
        match self.blocks.get_mut(id) {
            Some(Block::Changes(changes)) => Ok(changes),
            Some(_) => Err(RpcError::bad_request(format!("block {id} is not a changes block"))),
            None => Err(RpcError::not_found(format!("block {id}"))),
        }
    }

    /// `changes.set`: the block's new view state.
    pub fn changes_set(&mut self, params: ChangesSetParams) -> Result<ChangesState, RpcError> {
        let block_id = params.block_id.clone();
        let changes = self.changes(&block_id)?;
        let changed = changes.set(params)?;
        let state = changes.state().clone();
        if changed {
            self.block_changed(&block_id);
        }
        Ok(state)
    }

    /// `changes.root`: review the repo at canonical `root`, afresh.
    pub fn changes_root(&mut self, block_id: &str, root: &std::path::Path) -> Result<ChangesState, RpcError> {
        let notes = self.notes.clone();
        let changes = self.changes(block_id)?;
        changes.reroot(root, notes);
        let state = changes.state().clone();
        self.block_changed(block_id);
        if let Ok((si, _)) = self.locate(block_id) {
            self.session_changed(si);
        }
        Ok(state)
    }

    /// `files.root`: show canonical directory `root`, afresh.
    pub fn files_root(&mut self, block_id: &str, root: &std::path::Path) -> Result<FilesState, RpcError> {
        let notes = self.notes.clone();
        let files = self.files(block_id)?;
        files.reroot(root, notes);
        let state = files.state().clone();
        self.block_changed(block_id);
        if let Ok((si, _)) = self.locate(block_id) {
            self.session_changed(si);
        }
        Ok(state)
    }
}

/// How terminal block `block_id` runs `command` (the login shell if empty) in `cwd`.
pub fn spawn_config(block_id: &str, command: &[String], cwd: Option<&str>) -> SpawnConfig {
    let mut env = crate::bundle::shell_env(block_id);
    let mut command: Vec<std::ffi::OsString> = command.iter().map(Into::into).collect();
    if command.is_empty() {
        let program = silo_pty::user_shell();
        if let Some(shell) = crate::shell::Shell::of(&program) {
            let (args, hook_env) = crate::shell::hook(shell, &crate::bundle::path_lead().0);
            env.extend(hook_env);
            if !args.is_empty() {
                command = std::iter::once(program.into_os_string()).chain(args).collect();
            }
        }
    }
    SpawnConfig { command, cwd: cwd.map(|c| std::path::PathBuf::from(expand_home(c))).filter(|p| p.is_dir()), env }
}

fn expand_home(path: &str) -> String {
    match path.strip_prefix('~') {
        Some(rest) => format!("{}{rest}", silo_pty::home_dir().display()),
        None => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry with session `s` holding window `w1` (`a | b / c`) and
    /// window `w2` (`d`), without spawning processes.
    fn registry() -> (Registry, tokio::sync::mpsc::UnboundedReceiver<Frame>) {
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        let mut reg = Registry::new(notes);
        let mut layout = Layout::block("a");
        layout.split("a", "b".into(), Axis::Horizontal);
        layout.split("b", "c".into(), Axis::Vertical);
        layout.set_ratio(&[], 0.3);
        let window = |id: &str, layout: Layout, focused: &str| Window {
            id: id.into(),
            name: None,
            layout,
            focused: focused.into(),
            history: vec![focused.into()],
            zoomed: false,
            floats: Vec::new(),
        };
        reg.sessions.push(Session {
            id: "s".into(),
            name: "s".into(),
            windows: vec![window("w1", layout, "a"), window("w2", Layout::block("d"), "d"), window("w3", Layout::block("e"), "e")],
            active_window: "w1".into(),
            revision: 1,
            created_at: 0,
            parked: Vec::new(),
        });
        let client = reg.add_client("test".into(), false);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        reg.subscribe(client, Sink::new(tx));
        (reg, rx)
    }

    fn changed(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Frame>) -> SessionView {
        let frame = rx.try_recv().expect("event");
        match frame.parse::<ServerMessage>().unwrap() {
            ServerMessage::Event { event, .. } => match *event {
                Event::SessionChanged { view } => view,
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn swap_exchanges_leaves_bumps_revision_and_emits() {
        let (mut reg, mut rx) = registry();
        reg.swap("a", "c").unwrap();
        let view = changed(&mut rx);
        assert_eq!(view.session.revision, 2);
        let window = &view.session.windows[0];
        assert_eq!(window.layout.blocks(), ["c", "b", "a"]);
        assert!(matches!(window.layout, Layout::Split { ratio, .. } if ratio == 0.3));
        assert_eq!(window.focused, "a", "focus follows the block, not the slot");

        let err = reg.swap("a", "d").unwrap_err();
        assert_eq!(err.code, "bad_request");
        assert!(err.message.contains("different windows"), "{err}");
        assert_eq!(reg.swap("a", "a").unwrap_err().code, "bad_request");
        assert_eq!(reg.swap("a", "zz").unwrap_err().code, "not_found");
        assert!(rx.try_recv().is_err(), "rejected swaps emit nothing");
        assert_eq!(reg.sessions[0].revision, 2);
    }

    #[test]
    fn move_window_reorders_and_emits() {
        let (mut reg, mut rx) = registry();
        reg.move_window("w1", 2).unwrap();
        let view = changed(&mut rx);
        let order: Vec<_> = view.session.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(order, ["w2", "w3", "w1"]);
        reg.move_window("w1", 0).unwrap();
        let order: Vec<_> = changed(&mut rx).session.windows.iter().map(|w| w.id.clone()).collect();
        assert_eq!(order, ["w1", "w2", "w3"]);
        assert_eq!(reg.sessions[0].revision, 3);
    }

    #[test]
    fn float_move_dock_round_trip() {
        let (mut reg, mut rx) = registry();
        reg.float("b", None).unwrap();
        let view = changed(&mut rx);
        assert_eq!(view.session.revision, 2);
        let w = &view.session.windows[0];
        assert_eq!((w.layout.blocks(), w.focused.as_str()), (vec!["a".to_owned(), "c".to_owned()], "b"));
        assert_eq!(w.floats, [Float { id: "b".into(), rect: FloatRect::default() }]);

        let rect = FloatRect { x: 0.9, y: 0.0, w: 0.5, h: 0.01 };
        reg.move_block("b", Some(rect), None).unwrap();
        assert_eq!(changed(&mut rx).session.windows[0].floats[0].rect, rect.clamped());
        let dock = || Some(Dock { target: "a".into(), axis: Axis::Vertical, before: false });
        assert_eq!(reg.move_block("b", None, None).unwrap_err().code, "bad_request");
        assert_eq!(reg.move_block("b", Some(rect), dock()).unwrap_err().code, "bad_request");
        assert_eq!(reg.move_block("a", Some(rect), None).unwrap_err().code, "bad_request", "a is tiled");
        assert_eq!(
            reg.move_block("b", None, Some(Dock { target: "d".into(), axis: Axis::Vertical, before: false })).unwrap_err().code,
            "bad_request"
        );
        assert!(rx.try_recv().is_err(), "rejected moves emit nothing");

        reg.move_block("b", None, dock()).unwrap();
        let view = changed(&mut rx);
        let w = &view.session.windows[0];
        assert_eq!(w.layout.blocks(), ["a", "b", "c"]);
        assert!(w.floats.is_empty());
        assert_eq!((w.focused.as_str(), view.session.revision), ("b", 4));
    }

    fn tiled_layout(reg: &Registry) -> &Layout {
        &reg.sessions[0].windows[0].layout
    }

    #[test]
    fn move_docks_a_tiled_block_on_each_side() {
        let split = |axis, ratio, first: Layout, second: Layout| Layout::Split { axis, ratio, first: Box::new(first), second: Box::new(second) };
        let (a, b, c) = (Layout::block("a"), Layout::block("b"), Layout::block("c"));
        for (axis, before) in [(Axis::Horizontal, true), (Axis::Horizontal, false), (Axis::Vertical, true), (Axis::Vertical, false)] {
            let (mut reg, mut rx) = registry();
            reg.move_block("c", None, Some(Dock { target: "a".into(), axis, before })).unwrap();
            let view = changed(&mut rx);
            let w = &view.session.windows[0];
            let docked = if before { split(axis, 0.5, c.clone(), a.clone()) } else { split(axis, 0.5, a.clone(), c.clone()) };
            // c's old split (b / c) collapsed into b.
            assert_eq!(w.layout, split(Axis::Horizontal, 0.3, docked, b.clone()), "{axis:?} {before}");
            assert_eq!((w.focused.as_str(), view.session.revision), ("c", 2));
        }
    }

    #[test]
    fn move_docks_under_a_block_in_a_nested_split_and_unzooms() {
        // a | (b / c): drag c under a, giving (a / c) | b.
        let (mut reg, mut rx) = registry();
        reg.zoom("b", true).unwrap();
        changed(&mut rx);
        reg.move_block("c", None, Some(Dock { target: "a".into(), axis: Axis::Vertical, before: false })).unwrap();
        let view = changed(&mut rx);
        let w = &view.session.windows[0];
        let Layout::Split { axis: Axis::Horizontal, ratio, first, second } = &w.layout else { panic!("{:?}", w.layout) };
        assert_eq!(*ratio, 0.3);
        assert!(matches!(&**first, Layout::Split { axis: Axis::Vertical, ratio, .. } if *ratio == 0.5));
        assert_eq!((first.blocks(), second.blocks()), (vec!["a".to_owned(), "c".to_owned()], vec!["b".to_owned()]));
        assert_eq!((w.focused.as_str(), w.zoomed), ("c", false));

        // Down to one block beside another: fine.
        reg.move_block("a", None, Some(Dock { target: "b".into(), axis: Axis::Horizontal, before: false })).unwrap();
        reg.move_block("c", None, Some(Dock { target: "b".into(), axis: Axis::Vertical, before: true })).unwrap();
        assert_eq!(tiled_layout(&reg).blocks(), ["c", "b", "a"]);
    }

    #[test]
    fn move_docks_a_float_before_its_target() {
        let (mut reg, mut rx) = registry();
        reg.float("b", None).unwrap();
        changed(&mut rx);
        reg.move_block("b", None, Some(Dock { target: "a".into(), axis: Axis::Horizontal, before: true })).unwrap();
        let w = &changed(&mut rx).session.windows[0];
        assert_eq!((w.layout.blocks(), w.floats.is_empty(), w.focused.as_str()), (vec!["b".to_owned(), "a".into(), "c".into()], true, "b"));
    }

    #[test]
    fn move_refuses_docking_on_itself_another_window_or_a_float() {
        let (mut reg, mut rx) = registry();
        let dock = |target: &str| Some(Dock { target: target.into(), axis: Axis::Vertical, before: true });
        let err = reg.move_block("a", None, dock("a")).unwrap_err();
        assert!(err.code == "bad_request" && err.message.contains("itself"), "{err}");
        let err = reg.move_block("a", None, dock("d")).unwrap_err();
        assert!(err.code == "bad_request" && err.message.contains("not tiled in this window"), "{err}");
        assert_eq!(reg.move_block("d", None, dock("a")).unwrap_err().code, "bad_request", "from another window");
        assert_eq!(reg.move_block("zz", None, dock("a")).unwrap_err().code, "not_found");
        assert_eq!(reg.move_block("a", None, dock("zz")).unwrap_err().code, "bad_request");
        assert!(rx.try_recv().is_err(), "rejected moves emit nothing");
        reg.float("b", None).unwrap();
        changed(&mut rx);
        assert_eq!(reg.move_block("a", None, dock("b")).unwrap_err().code, "bad_request", "floats are no target");
        assert!(rx.try_recv().is_err());
        assert_eq!(tiled_layout(&reg).blocks(), ["a", "c"]);
    }

    #[test]
    fn floats_raise_refuse_and_promote() {
        let (mut reg, mut rx) = registry();
        assert_eq!(reg.float("d", None).unwrap_err().code, "conflict", "the last tiled block cannot float");
        reg.float("b", None).unwrap();
        reg.float("c", None).unwrap();
        assert_eq!(reg.float("a", None).unwrap_err().code, "conflict");
        assert_eq!(reg.float("b", None).unwrap_err().code, "bad_request", "already floating");
        assert_eq!(reg.zoom("b", true).unwrap_err().code, "bad_request");
        assert_eq!(reg.swap("a", "b").unwrap_err().code, "bad_request");
        let new = New { spawn: &SpawnSpec::default(), kind: BlockKind::Terminal, url: None, agent: None };
        assert_eq!(reg.split("b", Axis::Vertical, &new).unwrap_err().code, "bad_request");
        assert_eq!(reg.sessions[0].blocks(), ["a", "b", "c", "d", "e"], "floats count as session blocks");
        changed(&mut rx);
        changed(&mut rx);

        reg.focus("b").unwrap();
        let view = changed(&mut rx);
        let ids = |w: &WindowInfo| w.floats.iter().map(|f| f.id.clone()).collect::<Vec<_>>();
        assert_eq!((ids(&view.session.windows[0]), view.session.windows[0].focused.as_str()), (vec!["c".into(), "b".into()], "b"));

        reg.remove_block("a");
        let w = changed(&mut rx).session.windows[0].clone();
        assert_eq!(
            (w.layout.clone(), ids(&w), w.focused.as_str()),
            (Layout::block("b"), vec!["c".into()], "b"),
            "the topmost float becomes the tree"
        );
        reg.remove_block("c");
        let w = changed(&mut rx).session.windows[0].clone();
        assert_eq!((w.layout, w.floats.len()), (Layout::block("b"), 0));
        reg.remove_block("b");
        assert_eq!(reg.sessions[0].windows.len(), 2, "an empty window closes");
    }

    fn focused(reg: &Registry, wi: usize) -> (&str, Vec<&str>) {
        let w = &reg.sessions[0].windows[wi];
        (w.focused.as_str(), w.history.iter().map(String::as_str).collect())
    }

    #[test]
    fn closing_focus_returns_to_the_previous_block() {
        let (mut reg, _rx) = registry();
        let new = New { spawn: &SpawnSpec::default(), kind: BlockKind::Browser, url: None, agent: None };
        let mut chain = vec!["d".to_owned()];
        for _ in 0..3 {
            let last = chain.last().unwrap().clone();
            chain.push(reg.split(&last, Axis::Horizontal, &new).unwrap().block_id);
        }
        let [d, x, y, z] = [&chain[0], &chain[1], &chain[2], &chain[3]].map(String::as_str);
        assert_eq!(focused(&reg, 1), (z, vec![z, y, x, d]));
        reg.close_block(z).unwrap();
        assert_eq!(focused(&reg, 1), (y, vec![y, x, d]), "focus returns to the block it was split from");
        reg.focus(d).unwrap();
        reg.close_block(d).unwrap();
        assert_eq!(focused(&reg, 1), (y, vec![y, x]), "focus returns to the block used before");
        reg.close_block(x).unwrap();
        assert_eq!(focused(&reg, 1), (y, vec![y]), "closing an unfocused block prunes history only");
    }

    #[test]
    fn feature_events_reach_only_clients_advertising_it() {
        let client =
            |features: &[&str]| ClientEntry { features: features.iter().map(|&f| f.to_owned()).collect(), ..ClientEntry::new(String::new(), false) };
        assert!(client(&[]).supports(None));
        assert!(!client(&[]).supports(Some("synthetic")));
        assert!(client(&["synthetic"]).supports(Some("synthetic")));
        assert_eq!(needed_feature(&Event::BlockBell { block_id: "b".into() }), None);
        assert_eq!(needed_feature(&Event::GrantsChanged { grants: Vec::new() }), Some(feature::ACCESS));
    }

    #[test]
    fn unknown_block_kinds_are_rejected() {
        let (mut reg, _rx) = registry();
        let new = New { spawn: &SpawnSpec::default(), kind: BlockKind::Unknown, url: None, agent: None };
        let blocks = reg.blocks.len();
        assert_eq!(reg.split("a", Axis::Horizontal, &new).unwrap_err().code, "bad_request");
        assert_eq!(reg.create_session(None, &new).unwrap_err().code, "bad_request");
        assert_eq!(reg.blocks.len(), blocks);
    }

    #[test]
    fn closing_a_focused_float_returns_to_the_previous_block() {
        let (mut reg, _rx) = registry();
        reg.focus("c").unwrap();
        reg.float("b", None).unwrap();
        assert_eq!(focused(&reg, 0), ("b", vec!["b", "c", "a"]));
        reg.remove_block("b");
        assert_eq!(focused(&reg, 0), ("c", vec!["c", "a"]));
    }

    #[test]
    fn closing_focus_without_history_takes_the_sibling() {
        let (mut reg, _rx) = registry();
        reg.focus("c").unwrap();
        reg.sessions[0].windows[0].history.clear();
        reg.remove_block("c");
        assert_eq!(focused(&reg, 0), ("b", vec!["b"]), "b takes over c's space");
        reg.sessions[0].windows[0].history.clear();
        reg.remove_block("b");
        assert_eq!(focused(&reg, 0), ("a", vec!["a"]));

        let (mut reg, _rx) = registry();
        reg.sessions[0].windows[0].history.clear();
        reg.remove_block("a");
        assert_eq!(focused(&reg, 0), ("b", vec!["b"]), "the nearest leaf of the subtree taking a's space");
    }

    /// Run `script` in block `id`; `shell` makes it count as the login shell.
    fn run(reg: &mut Registry, id: &str, script: &str, shell: bool, notes: &UnboundedSender<Note>) {
        let info = BlockInfo {
            id: id.into(),
            session_id: "s".into(),
            kind: BlockKind::Terminal,
            title: "sh".into(),
            cwd: None,
            exited: None,
            lifecycle: Lifecycle::Running,
            size: TermSize::new(80, 24),
            owner: None,
            owner_name: None,
            owner_block: None,
            pid: None,
            browser: None,
            agent: None,
            files: None,
            changes: None,
        };
        let config = spawn_config("b", &["/bin/sh".into(), "-c".into(), script.into()], None);
        let mut handle = block::spawn(info, &config, notes.clone()).unwrap();
        assert!(handle.command);
        handle.command = !shell;
        reg.blocks.insert(id.into(), Block::Terminal(handle));
    }

    async fn exited(reg: &mut Registry, notes: &mut tokio::sync::mpsc::UnboundedReceiver<Note>) {
        let wait = async {
            loop {
                let note = notes.recv().await.expect("note");
                let done = matches!(note, Note::Exited(_));
                reg.handle_note(note);
                if done {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait).await.expect("exit note");
    }

    fn events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Frame>) -> Vec<Event> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|frame| match frame.parse::<ServerMessage>().unwrap() {
                ServerMessage::Event { event, .. } => *event,
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn a_clean_exit_closes_the_block_a_failed_one_stays() {
        let (mut reg, mut rx) = registry();
        let (notes, mut notes_rx) = tokio::sync::mpsc::unbounded_channel();

        run(&mut reg, "d", "exit 3", true, &notes);
        exited(&mut reg, &mut notes_rx).await;
        let info = reg.block_info("d").unwrap();
        assert_eq!((info.lifecycle, info.exited), (Lifecycle::Exited, Some(3)));
        assert!(reg.sessions[0].windows[1].contains("d"));

        run(&mut reg, "d", "exit 0", false, &notes);
        exited(&mut reg, &mut notes_rx).await;
        let info = reg.block_info("d").unwrap();
        assert_eq!((info.lifecycle, info.exited), (Lifecycle::Exited, Some(0)), "an explicit command stays readable");

        reg.focus("b").unwrap();
        reg.focus("c").unwrap();
        run(&mut reg, "c", "exit 0", true, &notes);
        exited(&mut reg, &mut notes_rx).await;
        assert!(reg.block_info("c").is_none());
        assert_eq!(focused(&reg, 0), ("b", vec!["b", "a"]), "focus returns to the previous block");
        assert_eq!(reg.sessions[0].windows[0].layout.blocks(), ["a", "b"]);

        reg.close_window("w1").unwrap();
        reg.close_window("w2").unwrap();
        events(&mut rx);
        run(&mut reg, "e", "exit 0", true, &notes);
        exited(&mut reg, &mut notes_rx).await;
        assert!(reg.sessions.is_empty());
        let events = events(&mut rx);
        assert!(matches!(&events[..], [.., Event::SessionClosed { session_id }, Event::SessionsChanged { .. }] if session_id == "s"), "{events:?}");
    }

    #[test]
    fn closing_the_last_block_closes_the_session() {
        let (mut reg, mut rx) = registry();
        reg.close_window("w2").unwrap();
        reg.close_window("w3").unwrap();
        for block in ["a", "b"] {
            reg.close_block(block).unwrap();
        }
        while rx.try_recv().is_ok() {}
        reg.close_block("c").unwrap();
        assert!(reg.sessions.is_empty());
        let events: Vec<Event> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|frame| match frame.parse::<ServerMessage>().unwrap() {
                ServerMessage::Event { event, .. } => *event,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert!(
            matches!(&events[..], [Event::SessionClosed { session_id }, Event::SessionsChanged { sessions }] if session_id == "s" && sessions.is_empty())
        );
        let (_, tree_dirty) = reg.take_dirty(false);
        assert!(tree_dirty, "the emptied tree is committed");
        assert_eq!(reg.close_block("c").unwrap_err().code, "not_found");
    }

    #[tokio::test]
    async fn keep_session_replaces_the_last_one() {
        let (mut reg, _rx) = registry();
        reg.keep_session();
        assert_eq!(reg.sessions.len(), 1, "a session already exists");
        reg.close_session("s").unwrap();
        assert_eq!(reg.sessions.len(), 1);
        let fresh = reg.sessions[0].id.clone();
        assert_ne!(fresh, "s");
        let block = reg.sessions[0].blocks()[0].clone();
        reg.close_block(&block).unwrap();
        assert_eq!(reg.sessions.len(), 1, "closing its last block starts another");
        assert_ne!(reg.sessions[0].id, fresh);
    }
    #[test]
    fn closing_the_last_agent_block_showing_a_session_reports_it_once() {
        let (mut reg, _rx) = registry();
        let closed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = closed.clone();
        reg.agent_closed = Some(Arc::new(move |s| sink.lock().unwrap().push(s)));
        let agent = |id: &str, session: &str| {
            let info: BlockInfo = serde_json::from_value(serde_json::json!({
                "id": id, "session_id": "s", "kind": "agent", "title": "", "size": {"columns": 80, "rows": 24},
                "agent": {"session_id": session, "title": ""},
            }))
            .unwrap();
            Block::restore(info).unwrap()
        };
        for (id, session) in [("a", "child"), ("b", "child"), ("d", "other"), ("e", "third")] {
            reg.blocks.insert(id.into(), agent(id, session));
        }
        let took = |closed: &Arc<std::sync::Mutex<Vec<String>>>| std::mem::take(&mut *closed.lock().unwrap());
        // Another block still shows it: not closed yet.
        reg.close_block("a").unwrap();
        assert!(took(&closed).is_empty());
        reg.close_block("b").unwrap();
        assert_eq!(took(&closed), ["child"]);
        // A whole tab, then the whole session.
        reg.close_window("w2").unwrap();
        assert_eq!(took(&closed), ["other"]);
        reg.close_session("s").unwrap();
        assert_eq!(took(&closed), ["third"]);
    }
}
