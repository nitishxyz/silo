//! Session tree, clients, tickets and the sequenced event bus.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use silo_proto::{
    Frame,
    control::{Created, DataTicket, Event, RpcError, ServerMessage, SpawnSpec},
    data::ticket_hex,
    kind,
    model::{
        Axis, BlockId, BlockInfo, BlockKind, ClientId, Layout, Lifecycle, SessionId, SessionInfo, SessionSummary, SessionView,
        TermSize, WindowId, WindowInfo,
    },
};
use silo_pty::SpawnConfig;
use tokio::sync::{Notify, mpsc::UnboundedSender};

use crate::block::{self, BlockHandle, BlockMsg, Note, Sink, now_ms};

/// A control connection whose unsent event backlog exceeds this is closed;
/// the client reconnects and re-reads state (events are never dropped).
const EVENT_BACKLOG: usize = 8 * 1024 * 1024;
const TICKET_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Serialize, Deserialize)]
pub struct Window {
    pub id: WindowId,
    pub name: Option<String>,
    pub layout: Layout,
    pub focused: BlockId,
    pub zoomed: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub windows: Vec<Window>,
    pub active_window: WindowId,
    pub revision: u64,
    pub created_at: u64,
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
                })
                .collect(),
            active_window: self.active_window.clone(),
            revision: self.revision,
            created_at: self.created_at,
        }
    }

    pub fn blocks(&self) -> Vec<BlockId> {
        self.windows.iter().flat_map(|w| w.layout.blocks()).collect()
    }
}

pub struct ClientEntry {
    pub name: String,
    pub passive: bool,
    pub events: Option<Sink>,
    pub close: Option<tokio::sync::oneshot::Sender<()>>,
    pub attached: usize,
}

struct Ticket {
    block_id: BlockId,
    client_id: ClientId,
    expires: Instant,
}

pub struct Registry {
    pub sessions: Vec<Session>,
    pub blocks: HashMap<BlockId, BlockHandle>,
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
}

fn new_id(prefix: char) -> String {
    format!("{prefix}{:08x}", rand::random::<u32>())
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
        }
    }

    /// A registry holding a restored tree; nothing is spawned.
    pub fn restored(notes: UnboundedSender<Note>, sessions: Vec<Session>, blocks: HashMap<BlockId, BlockHandle>) -> Self {
        Self { sessions, blocks, ..Self::new(notes) }
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
            let taken = self.blocks.contains_key(&id)
                || self.sessions.iter().any(|s| s.id == id || s.windows.iter().any(|w| w.id == id || w.layout.contains(&id)));
            if !taken {
                return id;
            }
        }
    }

    pub fn add_client(&mut self, name: String, passive: bool) -> ClientId {
        let id = self.next_client;
        self.next_client += 1;
        self.clients.insert(id, ClientEntry { name, passive, events: None, close: None, attached: 0 });
        id
    }

    pub fn remove_client(&mut self, id: ClientId) {
        self.clients.remove(&id);
        self.tickets.retain(|_, t| t.client_id != id);
        for block in self.blocks.values() {
            block.send(BlockMsg::ClientGone { client_id: id });
        }
    }

    pub fn client_name(&self, id: ClientId) -> String {
        self.clients.get(&id).map(|c| c.name.clone()).unwrap_or_default()
    }

    pub fn emit(&mut self, event: Event) {
        self.seq += 1;
        let message = ServerMessage::Event { seq: self.seq, event };
        let Ok(frame) = Frame::json(kind::CONTROL, &message) else { return };
        for client in self.clients.values_mut() {
            let Some(sink) = &client.events else { continue };
            let overflow = sink.queued.load(std::sync::atomic::Ordering::Acquire) > EVENT_BACKLOG;
            if overflow || !sink.send(frame.clone()) {
                client.events = None;
                if let Some(close) = client.close.take() {
                    let _ = close.send(());
                }
            }
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
            })
            .collect()
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
        Ok(SessionView { session: session.info(), blocks })
    }

    pub fn block_info(&self, id: &str) -> Option<BlockInfo> {
        self.blocks.get(id).and_then(|b| b.info.lock().ok().map(|i| i.clone()))
    }

    pub fn block(&self, id: &str) -> Result<&BlockHandle, RpcError> {
        self.blocks.get(id).ok_or_else(|| RpcError::not_found(format!("block {id}")))
    }

    /// Session and window containing `block`.
    fn locate(&self, block: &str) -> Result<(usize, usize), RpcError> {
        for (si, session) in self.sessions.iter().enumerate() {
            for (wi, window) in session.windows.iter().enumerate() {
                if window.layout.contains(block) {
                    return Ok((si, wi));
                }
            }
        }
        Err(RpcError::not_found(format!("block {block}")))
    }

    fn locate_window(&self, window: &str) -> Result<(usize, usize), RpcError> {
        for (si, session) in self.sessions.iter().enumerate() {
            if let Some(wi) = session.windows.iter().position(|w| w.id == window) {
                return Ok((si, wi));
            }
        }
        Err(RpcError::not_found(format!("window {window}")))
    }

    fn spawn_block(&mut self, session_id: &str, spawn: &SpawnSpec, fallback: TermSize) -> Result<BlockId, RpcError> {
        let id = self.fresh_id('b');
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
            pid: None,
        };
        let config = SpawnConfig {
            command: spawn.command.iter().map(Into::into).collect(),
            cwd: spawn.cwd.as_ref().map(|c| expand_home(c).into()).filter(|p: &std::path::PathBuf| p.is_dir()),
            env: Vec::new(),
        };
        let handle = block::spawn(info, &config, self.notes.clone()).map_err(RpcError::internal)?;
        self.blocks.insert(id.clone(), handle);
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

    pub fn create_session(&mut self, name: Option<String>, spawn: &SpawnSpec) -> Result<Created, RpcError> {
        let session_id = self.fresh_id('s');
        let block_id = self.spawn_block(&session_id, spawn, TermSize::new(80, 24))?;
        let window_id = self.fresh_id('w');
        let name = name
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| crate::names::generate(|n| self.sessions.iter().any(|s| s.name == n)));
        if self.sessions.iter().any(|s| s.name == name) {
            if let Some(block) = self.blocks.remove(&block_id) {
                block.send(BlockMsg::Shutdown);
            }
            return Err(RpcError::new("conflict", format!("session {name} already exists")));
        }
        self.sessions.push(Session {
            id: session_id.clone(),
            name,
            windows: vec![Window { id: window_id.clone(), name: None, layout: Layout::block(&block_id), focused: block_id.clone(), zoomed: false }],
            active_window: window_id.clone(),
            revision: 1,
            created_at: now_ms(),
        });
        self.sessions_changed();
        let si = self.sessions.len() - 1;
        self.session_changed(si);
        Ok(Created { session_id, window_id, block_id })
    }

    pub fn create_window(&mut self, session_id: &str, spawn: &SpawnSpec) -> Result<Created, RpcError> {
        let session = self.session(session_id)?;
        let sid = session.id.clone();
        let fallback = session
            .windows
            .iter()
            .find(|w| w.id == session.active_window)
            .map(|w| self.window_size(w))
            .unwrap_or(TermSize::new(80, 24));
        let block_id = self.spawn_block(&sid, spawn, fallback)?;
        let window_id = self.fresh_id('w');
        let session = self.session_mut(&sid)?;
        session.windows.push(Window { id: window_id.clone(), name: None, layout: Layout::block(&block_id), focused: block_id.clone(), zoomed: false });
        session.active_window = window_id.clone();
        let si = self.sessions.iter().position(|s| s.id == sid).unwrap_or(0);
        self.session_changed(si);
        self.sessions_changed();
        Ok(Created { session_id: sid, window_id, block_id })
    }

    /// Approximate full-window grid from the window's blocks.
    fn window_size(&self, window: &Window) -> TermSize {
        fn size(layout: &Layout, reg: &Registry) -> Option<TermSize> {
            match layout {
                Layout::Block { id } => reg.block_info(id).map(|i| i.size),
                Layout::Split { axis, first, second, .. } => {
                    let (a, b) = (size(first, reg)?, size(second, reg)?);
                    Some(match axis {
                        Axis::Horizontal => TermSize { columns: a.columns + b.columns + 1, rows: a.rows.max(b.rows), ..a },
                        Axis::Vertical => TermSize { columns: a.columns.max(b.columns), rows: a.rows + b.rows + 1, ..a },
                    })
                }
            }
        }
        size(&window.layout, self).unwrap_or(TermSize::new(80, 24))
    }

    pub fn split(&mut self, block_id: &str, axis: Axis, spawn: &SpawnSpec) -> Result<Created, RpcError> {
        let (si, wi) = self.locate(block_id)?;
        let sid = self.sessions[si].id.clone();
        let base = self.block_info(block_id).map_or(TermSize::new(80, 24), |i| i.size);
        let fallback = match axis {
            Axis::Horizontal => TermSize { columns: (base.columns / 2).max(2), ..base },
            Axis::Vertical => TermSize { rows: (base.rows / 2).max(2), ..base },
        };
        let mut spawn = spawn.clone();
        if spawn.cwd.is_none() {
            spawn.cwd = self.block_info(block_id).and_then(|i| i.cwd);
        }
        let new = self.spawn_block(&sid, &spawn, fallback)?;
        let window = &mut self.sessions[si].windows[wi];
        window.layout.split(block_id, new.clone(), axis);
        window.focused = new.clone();
        window.zoomed = false;
        let window_id = window.id.clone();
        self.session_changed(si);
        Ok(Created { session_id: sid, window_id, block_id: new })
    }

    pub fn focus(&mut self, block_id: &str) -> Result<(), RpcError> {
        let (si, wi) = self.locate(block_id)?;
        let session = &mut self.sessions[si];
        let changed = session.windows[wi].focused != block_id || session.active_window != session.windows[wi].id;
        session.windows[wi].focused = block_id.to_owned();
        session.active_window = session.windows[wi].id.clone();
        if changed {
            self.session_changed(si);
        }
        Ok(())
    }

    pub fn zoom(&mut self, block_id: &str, zoomed: bool) -> Result<(), RpcError> {
        let (si, wi) = self.locate(block_id)?;
        let window = &mut self.sessions[si].windows[wi];
        window.focused = block_id.to_owned();
        window.zoomed = zoomed;
        self.session_changed(si);
        Ok(())
    }

    /// Exchange the positions of two blocks of one window. Ratios stay, so
    /// each block's grid changes only if the two slots differ in size.
    pub fn swap(&mut self, block_id: &str, other_block_id: &str) -> Result<(), RpcError> {
        let (si, wi) = self.locate(block_id)?;
        let other = self.locate(other_block_id)?;
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
        for block in self.sessions[si].windows[wi].layout.blocks() {
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
        self.emit(Event::SessionClosed { session_id: sid });
        self.sessions_changed();
        Ok(())
    }

    pub fn close_block(&mut self, block_id: &str) -> Result<(), RpcError> {
        self.locate(block_id)?;
        self.kill_block(block_id);
        self.remove_block(block_id);
        Ok(())
    }

    fn kill_block(&mut self, block_id: &str) {
        // Closing a block hangs up its program, parked or not.
        self.parked.remove(block_id);
        if let Some(block) = self.blocks.remove(block_id) {
            block.send(BlockMsg::Shutdown);
        }
    }

    /// Remove a closed block from the layout, collapsing
    /// empty windows and sessions.
    pub fn remove_block(&mut self, block_id: &str) {
        self.blocks.remove(block_id);
        let Ok((si, wi)) = self.locate(block_id) else { return };
        let window = &mut self.sessions[si].windows[wi];
        let layout = std::mem::replace(&mut window.layout, Layout::block(""));
        match layout.remove(block_id) {
            Some(layout) => {
                let blocks = layout.blocks();
                if window.focused == block_id {
                    window.focused = blocks.first().cloned().unwrap_or_default();
                }
                window.zoomed &= blocks.len() > 1;
                window.layout = layout;
                self.session_changed(si);
            }
            None => self.drop_window(si, wi),
        }
    }

    fn drop_window(&mut self, si: usize, wi: usize) {
        let session = &mut self.sessions[si];
        let removed = session.windows.remove(wi);
        if session.windows.is_empty() {
            let sid = session.id.clone();
            self.sessions.remove(si);
            self.emit(Event::SessionClosed { session_id: sid });
        } else {
            if session.active_window == removed.id {
                session.active_window = session.windows[wi.min(session.windows.len() - 1)].id.clone();
            }
            self.session_changed(si);
        }
        self.sessions_changed();
    }

    pub fn issue_ticket(&mut self, client_id: ClientId, block_id: &str) -> Result<DataTicket, RpcError> {
        self.block(block_id)?;
        let now = Instant::now();
        self.tickets.retain(|_, t| t.expires > now);
        let ticket = ticket_hex(&rand::random::<[u8; 16]>());
        self.tickets.insert(ticket.clone(), Ticket { block_id: block_id.to_owned(), client_id, expires: now + TICKET_TTL });
        Ok(DataTicket { ticket, expires_in_ms: TICKET_TTL.as_millis() as u64 })
    }

    /// Consume a ticket: `(block, client)`.
    pub fn redeem(&mut self, ticket: &str) -> Option<(BlockId, ClientId)> {
        let t = self.tickets.remove(ticket)?;
        (t.expires > Instant::now() && self.clients.contains_key(&t.client_id)).then_some((t.block_id, t.client_id))
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
            Note::Dirty(id) => self.mark_block_dirty(&id),
            // The block stays in its layout, read-only, until closed.
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
            block.send(BlockMsg::Shutdown);
        }
        self.parked.clear();
        self.sessions.clear();
    }
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
        let window = |id: &str, layout: Layout, focused: &str| Window { id: id.into(), name: None, layout, focused: focused.into(), zoomed: false };
        reg.sessions.push(Session {
            id: "s".into(),
            name: "s".into(),
            windows: vec![window("w1", layout, "a"), window("w2", Layout::block("d"), "d"), window("w3", Layout::block("e"), "e")],
            active_window: "w1".into(),
            revision: 1,
            created_at: 0,
        });
        let client = reg.add_client("test".into(), false);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        reg.subscribe(client, Sink::new(tx));
        (reg, rx)
    }

    fn changed(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Frame>) -> SessionView {
        let frame = rx.try_recv().expect("event");
        match frame.parse::<ServerMessage>().unwrap() {
            ServerMessage::Event { event: Event::SessionChanged { view }, .. } => view,
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
}
