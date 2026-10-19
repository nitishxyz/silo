//! Terminal block actor: one thread exclusively owns the PTY, the
//! authoritative terminal, the raw output ring, subscribers and size
//! ownership, so attach (snapshot + live output) never races output.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use silo_proto::{
    Frame,
    control::{Capture, DesiredSize, HostOffer, OfferSource, OwnMode, SizeInfo},
    data::{Reject, Resume, ServerData, StartMode, Welcome},
    kind,
    model::{BlockId, BlockInfo, ClientId, CursorShape, Lifecycle, TermSize, TerminalConfig},
    offer::{Offer, OfferScanner},
};
use silo_pty::{Pty, PtyEvent, PtyInput, PtySize, SpawnConfig};
use silo_vt::{ColorScheme, GridSize, RestoreOptions, TerminalColors, TerminalCore, TerminalDefaults};
use tokio::sync::mpsc::UnboundedSender;

/// Raw output kept for resume after short disconnects.
pub const RING_CAP: usize = 4 * 1024 * 1024;
/// A subscriber whose live backlog (unsent bytes plus unacknowledged output)
/// exceeds this, or whose acked offset leaves the ring, is cut off; it
/// reconnects and is re-synchronised from a fresh snapshot (skip-ahead).
pub const SUBSCRIBER_BUDGET: usize = 16 * 1024 * 1024;
/// Snapshot history and ring replay are fed to a subscriber's writer only
/// while its unsent bytes stay below this, so large attaches never buffer
/// more than one copy of the snapshot.
pub const PUMP_WINDOW: usize = 1024 * 1024;
pub const HISTORY_CHUNK: usize = 256 * 1024;
const OUTPUT_CHUNK: usize = 256 * 1024;
const PROBE_INTERVAL: Duration = Duration::from_millis(400);
/// How often pending bulk sends are retried while a writer drains.
const PUMP_INTERVAL: Duration = Duration::from_millis(5);
/// How often the line discipline flags are polled while streams are
/// attached (they are also read after every output chunk).
const MODE_INTERVAL: Duration = Duration::from_millis(50);
/// Out-of-band output waits this long for a parser-ground boundary.
const INJECT_WAIT: Duration = Duration::from_millis(250);
/// Configuration and size transitions remembered inside the ring; older
/// resume points fall back to a snapshot.
const TRANSITION_LOG_CAP: usize = 1024;
/// PTY output read but not yet handled by the actor is capped at this: the
/// reader thread (and so the program) waits for the actor. Without it a
/// fast PTY (Linux) queues megabytes ahead of subscribers' ACKs in the
/// mailbox, and subscribers that keep up are cut as lagging.
const READ_AHEAD: usize = 256 * 1024;
/// Host offers (OSC 7717) a block may raise per [`OFFER_WINDOW`]; a ticket
/// is offered once per block.
const OFFER_LIMIT: usize = 3;
const OFFER_WINDOW: Duration = Duration::from_secs(60);
/// Distinct offered tickets remembered per block.
const OFFERED_CAP: usize = 16;

/// Back-pressure from the actor to the PTY reader thread.
#[derive(Default)]
struct ReadGate {
    /// Bytes queued for the actor, and whether waiting is disabled.
    state: Mutex<(usize, bool)>,
    room: Condvar,
}

impl ReadGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, (usize, bool)> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reader side: queue `len` bytes via `send`, then wait for room.
    fn read(&self, len: usize, send: impl FnOnce()) {
        self.lock().0 += len;
        send();
        let state = self.lock();
        drop(self.room.wait_while(state, |(queued, open)| *queued > READ_AHEAD && !*open));
    }

    /// Actor side: `len` queued bytes were taken.
    fn done(&self, len: usize) {
        let mut state = self.lock();
        state.0 = state.0.saturating_sub(len);
        if state.0 <= READ_AHEAD {
            self.room.notify_all();
        }
    }

    /// Stop throttling, so the reader thread can be stopped and joined.
    fn open(&self) {
        self.lock().1 = true;
        self.room.notify_all();
    }

    /// Throttle again from empty (a re-adopted PTY).
    fn reset(&self) {
        *self.lock() = (0, false);
    }
}

/// Outgoing frames for one data stream; `queued` counts unsent bytes.
#[derive(Clone)]
pub struct Sink {
    pub tx: UnboundedSender<Frame>,
    pub queued: Arc<AtomicUsize>,
    /// Set once the stream must end now (subscriber cut off).
    cancel: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Sink {
    #[must_use]
    pub fn new(tx: UnboundedSender<Frame>) -> Self {
        Self { tx, queued: Arc::new(AtomicUsize::new(0)), cancel: Arc::new(tokio::sync::watch::channel(false).0) }
    }

    pub fn send(&self, frame: Frame) -> bool {
        self.queued.fetch_add(frame.payload.len() + 5, Ordering::AcqRel);
        self.tx.send(frame).is_ok()
    }

    #[must_use]
    pub fn backlog(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    /// End the stream promptly, abandoning unsent frames.
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Resolves once [`Sink::cancel`] was called; never if every sink is
    /// dropped uncancelled. Does not keep the channel open.
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.cancel.subscribe();
        async move {
            if rx.wait_for(|cancelled| *cancelled).await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

pub struct Attach {
    pub sub_id: u64,
    pub client_id: ClientId,
    pub resume: Option<Resume>,
    pub sink: Sink,
    /// A remote stream: gets `TTY_MODE` and `INPUT_ACK` (for prediction).
    pub remote: bool,
}

pub enum BlockMsg {
    Output(Vec<u8>),
    ReaderClosed,
    /// The program's exit code; `None` when killed by a signal.
    ChildExited(Option<i32>),
    Input(Vec<u8>),
    /// Input from data stream `sub_id`: counted and acknowledged with
    /// `INPUT_ACK` on that stream.
    StreamInput {
        sub_id: u64,
        bytes: Vec<u8>,
    },
    Attach(Box<Attach>),
    Detach(u64),
    /// Subscriber `sub_id` applied every output byte below `offset`.
    Ack {
        sub_id: u64,
        offset: u64,
    },
    /// Flow-control state of one subscriber (diagnostics and tests).
    Subscriber {
        sub_id: u64,
        reply: tokio::sync::oneshot::Sender<Option<SubscriberStats>>,
    },
    /// `label` is the client's short name, shown as `owner_name`.
    Desired {
        client_id: ClientId,
        client_name: String,
        label: String,
        size: TermSize,
    },
    /// A client's desired terminal configuration; effective while it owns
    /// the block (or, unowned, when it is the most recent).
    Config {
        client_id: ClientId,
        config: TerminalConfig,
    },
    Own {
        client_id: ClientId,
        mode: OwnMode,
    },
    Disown {
        client_id: ClientId,
    },
    ClientGone {
        client_id: ClientId,
    },
    Capture {
        history: bool,
        reply: tokio::sync::oneshot::Sender<Capture>,
    },
    SizeInfo {
        reply: tokio::sync::oneshot::Sender<SizeInfo>,
    },
    /// Splice `bytes` into the output stream (all clients and the server
    /// terminal see them in the same order) at the next parser-ground
    /// boundary, then write `input` to the program.
    Inject {
        bytes: Vec<u8>,
        input: Vec<u8>,
    },
    /// Capture a [`Checkpoint`] between mutations.
    Checkpoint {
        reply: tokio::sync::oneshot::Sender<Result<Checkpoint, String>>,
    },
    /// Release the PTY and end the actor; see [`BlockHandle::handoff`].
    Handoff {
        reply: tokio::sync::oneshot::Sender<Result<Handoff, String>>,
    },
    Shutdown,
}

/// A block released for adoption by another actor (or server).
#[derive(Debug)]
pub struct Handoff {
    pub checkpoint: Checkpoint,
    /// The running program's PTY; `None` for exited or restored blocks.
    pub pty: Option<silo_pty::Detached>,
}

/// Everything needed to rebuild a block's terminal without its process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Ghostty snapshot of the terminal, history included.
    pub snapshot: Vec<u8>,
    pub size: TermSize,
    /// Effective terminal configuration.
    pub config: TerminalConfig,
    pub epoch: u64,
    /// Output offset (within `epoch`) the snapshot reflects.
    pub offset: u64,
    pub lifecycle: Lifecycle,
    pub exit_code: Option<i32>,
    /// Title set by the program (OSC 0/2); empty if none.
    pub osc_title: String,
    /// Working directory reported by the program (OSC 7).
    pub osc_pwd: Option<String>,
}

/// Notifications from block actors to the registry.
#[derive(Debug)]
pub enum Note {
    Changed(BlockId),
    Exited(BlockId),
    Bell(BlockId),
    /// A program printed a valid host offer.
    Offer(HostOffer),
    /// Terminal content or configuration changed for the first time since
    /// the last [`Checkpoint`]; sent once per checkpoint.
    Dirty(BlockId),
}

pub struct BlockHandle {
    pub tx: mpsc::Sender<BlockMsg>,
    pub info: Arc<Mutex<BlockInfo>>,
}

impl BlockHandle {
    pub fn send(&self, msg: BlockMsg) {
        let _ = self.tx.send(msg);
    }

    /// Snapshot the block's terminal and stream position for persistence.
    ///
    /// # Errors
    /// Fails if the actor is gone or the terminal cannot be snapshotted.
    pub async fn checkpoint(&self) -> anyhow::Result<Checkpoint> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(BlockMsg::Checkpoint { reply }).map_err(|_| anyhow::anyhow!("block actor gone"))?;
        rx.await.map_err(|_| anyhow::anyhow!("block actor gone"))?.map_err(anyhow::Error::msg)
    }

    /// Stop the block without hanging up its program: detach the PTY, apply
    /// all output read so far, checkpoint at exactly that offset, close
    /// every subscriber's stream and end the actor.
    ///
    /// # Errors
    /// Fails if the actor is gone or the terminal cannot be snapshotted (the
    /// block then keeps running).
    pub async fn handoff(&self) -> anyhow::Result<Handoff> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(BlockMsg::Handoff { reply }).map_err(|_| anyhow::anyhow!("block actor gone"))?;
        rx.await.map_err(|_| anyhow::anyhow!("block actor gone"))?.map_err(anyhow::Error::msg)
    }
}

/// Flow-control state of one subscriber.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriberStats {
    /// End offset of the output handed to the stream so far.
    pub sent: u64,
    /// Highest ACK; starts at the stream's first offset (`Welcome.offset`).
    pub acked: u64,
    /// Bytes handed to the writer but not yet written.
    pub queued: usize,
    /// Live bytes waiting behind a bulk send inside the actor.
    pub pending: usize,
    /// Snapshot/replay bytes not yet handed to the writer.
    pub bulk: usize,
}

/// Frames waiting for writer capacity.
enum Out {
    Frame(Frame),
    /// Snapshot history, sent as `SNAPSHOT_HISTORY` chunks from `pos`.
    History {
        bytes: Vec<u8>,
        pos: usize,
    },
    /// Ring bytes starting at output offset `offset`, sent as `OUTPUT`
    /// chunks from `pos`.
    Output {
        offset: u64,
        bytes: Vec<u8>,
        pos: usize,
    },
}

struct Subscriber {
    sink: Sink,
    /// Frames behind a bulk send, in stream order.
    pending: VecDeque<Out>,
    /// Bytes of `Out::Frame`s in `pending` (live traffic).
    pending_live: usize,
    /// Bytes of bulk entries in `pending`.
    pending_bulk: usize,
    sent: u64,
    /// Every output byte below this was applied by the client.
    acked: u64,
    /// `Input` frames written so far, the ring end at the last write, and
    /// the count already announced in `INPUT_ACK`.
    inputs: u64,
    input_offset: u64,
    inputs_acked: u64,
    /// Remote streams get `TTY_MODE` and `INPUT_ACK`.
    remote: bool,
}

impl Subscriber {
    fn new(sink: Sink, start: u64, remote: bool) -> Self {
        Self {
            remote,
            sink,
            pending: VecDeque::new(),
            pending_live: 0,
            pending_bulk: 0,
            sent: start,
            acked: start,
            inputs: 0,
            input_offset: start,
            inputs_acked: 0,
        }
    }

    /// Queue a frame behind any bulk send; returns false if the stream is
    /// gone.
    fn send(&mut self, frame: Frame) -> bool {
        if let Some(end) = output_end(&frame) {
            self.sent = self.sent.max(end);
        }
        if self.pending.is_empty() {
            return self.sink.send(frame);
        }
        self.pending_live = self.pending_live.saturating_add(frame.payload.len() + 5);
        self.pending.push_back(Out::Frame(frame));
        true
    }

    fn send_bulk(&mut self, out: Out) {
        let len = match &out {
            Out::Frame(_) => 0,
            Out::History { bytes, .. } => bytes.len(),
            Out::Output { offset, bytes, .. } => {
                self.sent = self.sent.max(offset.saturating_add(bytes.len() as u64));
                bytes.len()
            }
        };
        if len == 0 {
            return;
        }
        self.pending_bulk = self.pending_bulk.saturating_add(len);
        self.pending.push_back(out);
    }

    /// Feed pending frames to the writer while it has room; returns false
    /// if the stream is gone.
    fn pump(&mut self) -> bool {
        while self.sink.backlog() < PUMP_WINDOW {
            let Some(front) = self.pending.front_mut() else {
                break;
            };
            let frame = match front {
                Out::Frame(_) => {
                    let Some(Out::Frame(frame)) = self.pending.pop_front() else { unreachable!() };
                    self.pending_live = self.pending_live.saturating_sub(frame.payload.len() + 5);
                    frame
                }
                Out::History { bytes, pos } => {
                    let end = bytes.len().min(pos.saturating_add(HISTORY_CHUNK));
                    let frame = ServerData::SnapshotHistory(bytes[*pos..end].to_vec()).to_frame();
                    self.pending_bulk = self.pending_bulk.saturating_sub(end - *pos);
                    *pos = end;
                    if end == bytes.len() {
                        self.pending.pop_front();
                    }
                    frame
                }
                Out::Output { offset, bytes, pos } => {
                    let end = bytes.len().min(pos.saturating_add(OUTPUT_CHUNK));
                    let at = offset.saturating_add(*pos as u64);
                    let frame = ServerData::Output { offset: at, bytes: bytes[*pos..end].to_vec() }.to_frame();
                    self.pending_bulk = self.pending_bulk.saturating_sub(end - *pos);
                    *pos = end;
                    if end == bytes.len() {
                        self.pending.pop_front();
                    }
                    frame
                }
            };
            if !self.sink.send(frame) {
                return false;
            }
        }
        true
    }

    /// Too far behind to keep: its oldest unapplied byte already left the
    /// ring, or unsent plus unacknowledged bytes exceed the budget.
    fn lagging(&self, ring_start: u64) -> bool {
        if self.acked < ring_start {
            return true;
        }
        let unacked = usize::try_from(self.sent.saturating_sub(self.acked)).unwrap_or(usize::MAX);
        self.sink.backlog().saturating_add(self.pending_live).saturating_add(unacked) > SUBSCRIBER_BUDGET
    }

    /// Record an ACK; returns false if it is invalid (backwards or past the
    /// output sent), which ends the stream.
    fn ack(&mut self, offset: u64) -> bool {
        if !(self.acked..=self.sent).contains(&offset) {
            return false;
        }
        self.acked = offset;
        true
    }

    fn stats(&self) -> SubscriberStats {
        SubscriberStats { sent: self.sent, acked: self.acked, queued: self.sink.backlog(), pending: self.pending_live, bulk: self.pending_bulk }
    }
}

/// End offset of an `OUTPUT` frame.
fn output_end(frame: &Frame) -> Option<u64> {
    if frame.kind != kind::OUTPUT {
        return None;
    }
    let offset = u64::from_le_bytes(frame.payload.get(..8)?.try_into().ok()?);
    offset.checked_add((frame.payload.len() - 8) as u64)
}

struct Desired {
    name: String,
    label: String,
    size: TermSize,
    updated_at: u64,
}

struct Actor {
    id: BlockId,
    /// The PTY while the program runs; `None` once exited or restored.
    pty: Option<Pty>,
    input: Option<PtyInput>,
    /// The actor's own channel, held only alongside `pty` (whose threads
    /// hold it anyway) to re-adopt a PTY after a failed handoff.
    tx: Option<mpsc::Sender<BlockMsg>>,
    /// A PTY released by a failed handoff that could not be re-adopted: the
    /// program keeps running (frozen, no I/O) and the next handoff returns it.
    parked: Option<silo_pty::Detached>,
    /// Throttles the PTY reader thread; opened before the PTY is dropped
    /// or detached, which joins that thread.
    gate: Arc<ReadGate>,
    lifecycle: Lifecycle,
    vt: TerminalCore,
    epoch: u64,
    ring: VecDeque<u8>,
    /// Offset of `ring[0]`.
    ring_start: u64,
    subscribers: HashMap<u64, Subscriber>,
    size: TermSize,
    desired: HashMap<ClientId, Desired>,
    owner: Option<ClientId>,
    /// Ownership candidates, least recently active first.
    candidates: Vec<ClientId>,
    info: Arc<Mutex<BlockInfo>>,
    notes: UnboundedSender<Note>,
    osc_title: String,
    osc_pwd: Option<String>,
    last_probe: Instant,
    /// The child is gone; the block becomes read-only once output drains.
    child_exited: bool,
    /// Its exit code; `None` when killed by a signal (or unknown).
    exit_code: Option<i32>,
    reader_closed: bool,
    /// Pending injections and when the first was queued.
    injects: VecDeque<(Vec<u8>, Vec<u8>)>,
    inject_since: Option<Instant>,
    /// Every client's desired configuration and when it was reported.
    configs: HashMap<ClientId, (TerminalConfig, u64)>,
    /// Configuration installed in `vt` and announced to subscribers.
    config: TerminalConfig,
    /// A configuration waiting for a parser-ground boundary, and since when.
    pending_config: Option<(TerminalConfig, Instant)>,
    /// Where configurations and sizes took effect in the output stream.
    log: TransitionLog,
    /// Send [`Note::Dirty`] on the next change; re-armed by `checkpoint()`.
    dirty_armed: bool,
    /// Last `(echo, icanon)` read from the PTY and when it was polled.
    tty_mode: Option<(bool, bool)>,
    last_mode_poll: Instant,
    /// Strips host offers from the output.
    scanner: OfferScanner,
    /// Tickets already offered and when recent offers were raised.
    offered: VecDeque<String>,
    offer_times: VecDeque<Instant>,
}

/// Configuration or size change at an output offset.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Transition {
    Config(TerminalConfig),
    Resize(TermSize),
}

impl Transition {
    fn to_data(self, offset: u64) -> ServerData {
        match self {
            Self::Config(config) => ServerData::Configured(config),
            Self::Resize(size) => ServerData::Resized { offset, size },
        }
    }
}

/// Configuration and size in effect at some offset.
#[derive(Clone, Copy, Debug, PartialEq)]
struct StreamState {
    config: TerminalConfig,
    size: TermSize,
}

impl StreamState {
    fn apply(&mut self, transition: Transition) {
        match transition {
            Transition::Config(config) => self.config = config,
            Transition::Resize(size) => self.size = size,
        }
    }
}

/// Configuration and size transitions at their output offsets, in the order
/// they happened, so resume and replay switch state between the same two
/// bytes the live stream did.
struct TransitionLog {
    /// Effective from `floor` until the first change.
    base: StreamState,
    /// Earliest offset whose state is known.
    floor: u64,
    /// `(offset, transition)`, ascending by offset, in live order.
    changes: VecDeque<(u64, Transition)>,
}

impl TransitionLog {
    fn new(config: TerminalConfig, size: TermSize) -> Self {
        Self { base: StreamState { config, size }, floor: 0, changes: VecDeque::new() }
    }

    fn record(&mut self, offset: u64, transition: Transition) {
        match self.changes.back_mut() {
            Some(last) if last.0 == offset && std::mem::discriminant(&last.1) == std::mem::discriminant(&transition) => {
                last.1 = transition;
            }
            _ => self.changes.push_back((offset, transition)),
        }
        if self.changes.len() > TRANSITION_LOG_CAP
            && let Some((offset, transition)) = self.changes.pop_front()
        {
            self.base.apply(transition);
            self.floor = self.floor.max(offset);
        }
    }

    /// Forget changes at or before `start` (the ring's first offset).
    fn trim(&mut self, start: u64) {
        while let Some(&(offset, transition)) = self.changes.front() {
            if offset > start {
                break;
            }
            self.base.apply(transition);
            self.changes.pop_front();
        }
        self.floor = self.floor.max(start);
    }

    /// The state in effect at `offset` and the changes after it.
    fn from(&self, offset: u64) -> (StreamState, Vec<(u64, Transition)>) {
        let mut state = self.base;
        let mut later = Vec::new();
        for &(at, transition) in &self.changes {
            if at <= offset {
                state.apply(transition);
            } else {
                later.push((at, transition));
            }
        }
        (state, later)
    }
}
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn grid(size: TermSize) -> GridSize {
    GridSize {
        columns: size.columns.clamp(1, 512),
        rows: size.rows.clamp(1, 256),
        cell_width: if size.cell_width == 0 { 8 } else { size.cell_width },
        cell_height: if size.cell_height == 0 { 16 } else { size.cell_height },
    }
}

fn pty_size(size: TermSize) -> PtySize {
    let g = grid(size);
    PtySize { columns: g.columns, rows: g.rows, cell_width: g.cell_width, cell_height: g.cell_height }
}

/// Spawn a terminal block and its actor thread.
pub fn spawn(info: BlockInfo, config: &SpawnConfig, notes: UnboundedSender<Note>) -> anyhow::Result<BlockHandle> {
    let size = TermSize { columns: info.size.columns.clamp(1, 512), rows: info.size.rows.clamp(1, 256), ..info.size };
    let (tx, rx) = mpsc::channel();
    let exit_tx = tx.clone();
    let mut pty = Pty::spawn(config, pty_size(size), move |status| {
        let _ = exit_tx.send(BlockMsg::ChildExited(status.code()));
    })?;
    let gate = Arc::new(ReadGate::default());
    let input = start_io(&mut pty, &info.id, &tx, &gate)?;

    let mut info = info;
    info.size = size;
    info.pid = Some(pty.pid());
    info.lifecycle = Lifecycle::Running;
    let mut vt = TerminalCore::new(grid(size)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let config = TerminalConfig::default();
    install_config(&mut vt, &config);
    let mut actor = Actor::new(&info, vt, config, notes);
    actor.gate = gate;
    actor.pty = Some(pty);
    actor.input = Some(input);
    actor.tx = Some(tx.clone());
    start(actor, &info.id, tx, rx)
}

/// Rebuild a block from a [`Checkpoint`] without a process: its contents are
/// readable, input is ignored. The stream starts a new epoch.
///
/// # Errors
/// Fails if the snapshot cannot be restored.
pub fn spawn_restored(info: BlockInfo, checkpoint: Checkpoint, notes: UnboundedSender<Note>) -> anyhow::Result<BlockHandle> {
    let (vt, config) = restore_vt(&checkpoint)?;
    let size = checkpoint.size;
    let mut info = info;
    info.size = size;
    info.pid = None;
    info.owner = None;
    info.owner_name = None;
    info.lifecycle = Lifecycle::Restored;
    info.exited = None;
    restore_osc(&mut info, &checkpoint);
    let mut actor = Actor::new(&info, vt, config, notes);
    actor.lifecycle = Lifecycle::Restored;
    actor.ring_start = checkpoint.offset;
    actor.log.floor = checkpoint.offset;
    (actor.osc_title, actor.osc_pwd) = (checkpoint.osc_title, checkpoint.osc_pwd);
    let (tx, rx) = mpsc::channel();
    start(actor, &info.id, tx, rx)
}

/// Run a block around a program's PTY released by [`BlockHandle::handoff`],
/// continuing its stream: epoch and offsets carry on from `checkpoint`, so
/// clients resume at the handoff offset without a gap. The exit code of an
/// adopted program is unknown (`None`).
///
/// # Errors
/// Fails if the snapshot cannot be restored or the PTY not adopted.
pub fn spawn_adopted(info: BlockInfo, checkpoint: Checkpoint, pty: silo_pty::Detached, notes: UnboundedSender<Note>) -> anyhow::Result<BlockHandle> {
    let (vt, config) = match restore_vt(&checkpoint) {
        Ok(restored) => restored,
        Err(error) => {
            return Err(AdoptFailed { error, detached: pty }.into());
        }
    };
    let (tx, rx) = mpsc::channel();
    let gate = Arc::new(ReadGate::default());
    let (pty, input) = adopt_pty(&info.id, pty, &tx, &gate).map_err(|e| AdoptFailed { error: e.error.into(), detached: e.detached })?;
    let mut info = info;
    info.size = checkpoint.size;
    info.pid = Some(pty.pid());
    info.owner = None;
    info.owner_name = None;
    info.lifecycle = Lifecycle::Running;
    info.exited = None;
    restore_osc(&mut info, &checkpoint);
    let mut actor = Actor::new(&info, vt, config, notes);
    actor.epoch = checkpoint.epoch;
    actor.gate = gate;
    actor.ring_start = checkpoint.offset;
    actor.log.floor = checkpoint.offset;
    (actor.osc_title, actor.osc_pwd) = (checkpoint.osc_title, checkpoint.osc_pwd);
    actor.pty = Some(pty);
    actor.input = Some(input);
    actor.tx = Some(tx.clone());
    start(actor, &info.id, tx, rx)
}

/// Show a checkpoint's program-set title and cwd right away.
fn restore_osc(info: &mut BlockInfo, checkpoint: &Checkpoint) {
    if !checkpoint.osc_title.is_empty() {
        info.title.clone_from(&checkpoint.osc_title);
    }
    if let Some(cwd) = checkpoint.osc_pwd.as_deref().and_then(pwd_path) {
        info.cwd = Some(cwd);
    }
}

/// Why [`spawn_adopted`] failed, with the still-open PTY (no hangup);
/// [`spawn_adopted`] errors downcast to this.
#[derive(Debug)]
pub struct AdoptFailed {
    pub error: anyhow::Error,
    pub detached: silo_pty::Detached,
}

impl std::fmt::Display for AdoptFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "adopt pid {}: {:#}", self.detached.pid, self.error)
    }
}

impl std::error::Error for AdoptFailed {}

/// Adopt a detached PTY whose exit and I/O feed the actor's channel. On
/// failure the master comes back inside the error, still open.
fn adopt_pty(id: &str, pty: silo_pty::Detached, tx: &mpsc::Sender<BlockMsg>, gate: &Arc<ReadGate>) -> Result<(Pty, PtyInput), silo_pty::AdoptError> {
    let (exit_tx, io_tx, gate) = (tx.clone(), tx.clone(), gate.clone());
    Pty::adopt_with_io(
        pty,
        &format!("silo-pty-{id}"),
        move |code| {
            let _ = exit_tx.send(BlockMsg::ChildExited(code));
        },
        move |event| feed(&io_tx, &gate, event),
    )
}

/// The terminal and effective configuration of a checkpoint.
fn restore_vt(checkpoint: &Checkpoint) -> anyhow::Result<(TerminalCore, TerminalConfig)> {
    let options = RestoreOptions { retain_continuation: true, compress_history: false };
    let (mut vt, _) = TerminalCore::restore(&checkpoint.snapshot, options).map_err(|e| anyhow::anyhow!("{e}"))?;
    if vt.restoring() {
        anyhow::bail!("truncated snapshot");
    }
    let config = checkpoint.config.sanitized();
    install_config(&mut vt, &config);
    if vt.resize(grid(checkpoint.size)).is_err() {
        anyhow::bail!("cannot size restored terminal");
    }
    Ok((vt, config))
}

/// Run the PTY's owned reader/writer threads, feeding the actor's channel.
fn start_io(pty: &mut Pty, id: &str, tx: &mpsc::Sender<BlockMsg>, gate: &Arc<ReadGate>) -> std::io::Result<PtyInput> {
    let (tx, gate) = (tx.clone(), gate.clone());
    pty.start_io(&format!("silo-pty-{id}"), move |event| feed(&tx, &gate, event))
}

/// Reader thread: hand an event to the actor, waiting while it is behind.
fn feed(tx: &mpsc::Sender<BlockMsg>, gate: &ReadGate, event: PtyEvent) {
    match event {
        PtyEvent::Output(bytes) => gate.read(bytes.len(), || drop(tx.send(BlockMsg::Output(bytes)))),
        PtyEvent::Closed => drop(tx.send(BlockMsg::ReaderClosed)),
    }
}

fn start(mut actor: Actor, id: &str, tx: mpsc::Sender<BlockMsg>, rx: mpsc::Receiver<BlockMsg>) -> anyhow::Result<BlockHandle> {
    let shared = actor.info.clone();
    std::thread::Builder::new().name(format!("silo-block-{id}")).spawn(move || actor.run(&rx))?;
    Ok(BlockHandle { tx, info: shared })
}

impl Drop for Actor {
    /// Dropping the PTY joins its reader thread: let it finish first.
    fn drop(&mut self) {
        self.gate.open();
    }
}

impl Actor {
    fn new(info: &BlockInfo, vt: TerminalCore, config: TerminalConfig, notes: UnboundedSender<Note>) -> Self {
        Self {
            id: info.id.clone(),
            pty: None,
            input: None,
            tx: None,
            parked: None,
            gate: Arc::default(),
            lifecycle: Lifecycle::Running,
            vt,
            epoch: rand::random(),
            ring: VecDeque::new(),
            ring_start: 0,
            subscribers: HashMap::new(),
            size: info.size,
            desired: HashMap::new(),
            owner: None,
            candidates: Vec::new(),
            info: Arc::new(Mutex::new(info.clone())),
            notes,
            osc_title: String::new(),
            osc_pwd: None,
            last_probe: Instant::now() - PROBE_INTERVAL,
            child_exited: false,
            exit_code: None,
            reader_closed: false,
            injects: VecDeque::new(),
            inject_since: None,
            configs: HashMap::new(),
            config,
            pending_config: None,
            log: TransitionLog::new(config, info.size),
            dirty_armed: true,
            tty_mode: None,
            last_mode_poll: Instant::now(),
            scanner: OfferScanner::default(),
            offered: VecDeque::new(),
            offer_times: VecDeque::new(),
        }
    }

    fn run(&mut self, rx: &mpsc::Receiver<BlockMsg>) {
        let mut exit_deadline: Option<Instant> = None;
        loop {
            let mut timeout = exit_deadline.map_or(PROBE_INTERVAL, |d| d.saturating_duration_since(Instant::now()));
            if let Some(since) = self.inject_since.or(self.pending_config.map(|(_, since)| since)) {
                timeout = timeout.min((since + INJECT_WAIT).saturating_duration_since(Instant::now()));
            }
            if self.subscribers.values().any(|sub| !sub.pending.is_empty()) {
                timeout = timeout.min(PUMP_INTERVAL);
            }
            if self.pty.is_some() && self.remote_attached() {
                timeout = timeout.min(MODE_INTERVAL.saturating_sub(self.last_mode_poll.elapsed()));
            }
            match rx.recv_timeout(timeout) {
                Ok(BlockMsg::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(BlockMsg::Handoff { reply }) => match self.handoff(rx) {
                    Ok(handoff) => {
                        let _ = reply.send(Ok(handoff));
                        break;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                },
                Ok(msg) => {
                    if let BlockMsg::Output(bytes) = &msg {
                        self.gate.done(bytes.len());
                    }
                    if self.handle(msg) && exit_deadline.is_none() {
                        // Drain output still buffered in the PTY before
                        // reporting the exit, but do not wait forever for
                        // background jobs holding the terminal open.
                        exit_deadline = Some(Instant::now() + Duration::from_millis(300));
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            self.flush_config();
            self.flush_injects();
            self.flush_input_acks();
            if self.remote_attached() && self.last_mode_poll.elapsed() >= MODE_INTERVAL {
                self.poll_mode();
            }
            self.pump();
            if self.child_exited && self.lifecycle.is_running() && (self.reader_closed || exit_deadline.is_some_and(|d| Instant::now() >= d)) {
                self.finish();
                exit_deadline = None;
            }
            if self.lifecycle.is_running() && self.last_probe.elapsed() >= PROBE_INTERVAL {
                self.probe();
            }
        }
    }

    /// Returns true when the child exited.
    fn handle(&mut self, msg: BlockMsg) -> bool {
        match msg {
            BlockMsg::Output(bytes) if self.lifecycle.is_running() => self.output(&bytes),
            BlockMsg::Output(_) => {}
            BlockMsg::ReaderClosed => self.reader_closed = true,
            BlockMsg::ChildExited(code) if self.lifecycle.is_running() => {
                self.child_exited = true;
                self.exit_code = code;
                return true;
            }
            BlockMsg::ChildExited(_) => {}
            BlockMsg::Input(bytes) => {
                self.send_input(bytes);
            }
            BlockMsg::StreamInput { sub_id, bytes } => {
                let end = self.ring_end();
                if let Some(sub) = self.subscribers.get_mut(&sub_id).filter(|sub| sub.remote) {
                    sub.inputs += 1;
                    sub.input_offset = end;
                }
                self.send_input(bytes);
            }
            BlockMsg::Attach(attach) => self.attach(*attach),
            BlockMsg::Detach(id) => {
                self.subscribers.remove(&id);
            }
            BlockMsg::Ack { sub_id, offset } => {
                let ring_start = self.ring_start;
                if let Some(sub) = self.subscribers.get_mut(&sub_id)
                    && (!sub.ack(offset) || sub.lagging(ring_start))
                {
                    self.cut(sub_id);
                }
            }
            BlockMsg::Subscriber { sub_id, reply } => {
                let _ = reply.send(self.subscribers.get(&sub_id).map(Subscriber::stats));
            }
            BlockMsg::Desired { client_id, client_name, label, size } => {
                let fresh = self.desired.insert(client_id, Desired { name: client_name, label, size, updated_at: now_ms() }).is_none();
                if self.owner.is_none() || self.owner == Some(client_id) {
                    self.apply_size(size);
                }
                if fresh && self.owner == Some(client_id) {
                    self.publish();
                }
            }
            BlockMsg::Config { client_id, config } => {
                self.configs.insert(client_id, (config.sanitized(), now_ms()));
                if self.owner.is_none() || self.owner == Some(client_id) {
                    self.request_config(config.sanitized());
                }
            }
            BlockMsg::Own { client_id, mode } => self.own(client_id, mode),
            BlockMsg::Disown { client_id } => self.disown(client_id),
            BlockMsg::ClientGone { client_id } => {
                self.desired.remove(&client_id);
                self.configs.remove(&client_id);
                self.disown(client_id);
            }
            BlockMsg::Capture { history, reply } => {
                let _ = reply.send(self.capture(history));
            }
            BlockMsg::SizeInfo { reply } => {
                let mut desired: Vec<DesiredSize> = self
                    .desired
                    .iter()
                    .map(|(id, d)| DesiredSize { client_id: *id, client_name: d.name.clone(), size: d.size, updated_at: d.updated_at })
                    .collect();
                desired.sort_by_key(|d| d.client_id);
                let _ = reply.send(SizeInfo { size: self.size, owner: self.owner, desired });
            }
            BlockMsg::Inject { bytes, input } if self.lifecycle.is_running() => {
                self.injects.push_back((bytes, input));
                self.inject_since.get_or_insert_with(Instant::now);
            }
            BlockMsg::Inject { .. } => {}
            BlockMsg::Checkpoint { reply } => {
                let _ = reply.send(self.checkpoint());
            }
            BlockMsg::Handoff { .. } | BlockMsg::Shutdown => {}
        }
        false
    }

    /// Detach the PTY (its I/O threads are joined, so every byte read is
    /// already queued), apply that output, checkpoint and close all
    /// streams. On failure the PTY is re-adopted and the block keeps
    /// running, or, if that fails, parked open for the next handoff.
    fn handoff(&mut self, rx: &mpsc::Receiver<BlockMsg>) -> Result<Handoff, String> {
        self.input = None;
        self.gate.open();
        let detached = self.pty.take().map(Pty::detach).or_else(|| self.parked.take());
        while let Ok(msg) = rx.try_recv() {
            match msg {
                BlockMsg::Output(bytes) if self.lifecycle.is_running() => self.output(&bytes),
                BlockMsg::Attach(attach) => attach.sink.cancel(),
                _ => {}
            }
        }
        if self.lifecycle.is_running() {
            self.release_held();
        }
        match self.checkpoint() {
            Ok(checkpoint) => {
                for (_, sub) in self.subscribers.drain() {
                    sub.sink.cancel();
                }
                Ok(Handoff { checkpoint, pty: detached })
            }
            Err(e) => {
                if let Some(detached) = detached {
                    self.readopt(detached);
                }
                Err(e)
            }
        }
    }

    /// Resume running a PTY this actor released; if that fails, park it
    /// (still open, no hangup) for the next handoff.
    fn readopt(&mut self, detached: silo_pty::Detached) {
        let Some(tx) = self.tx.clone() else {
            self.parked = Some(detached);
            return;
        };
        self.gate.reset();
        match adopt_pty(&self.id, detached, &tx, &self.gate) {
            Ok((pty, input)) => {
                self.pty = Some(pty);
                self.input = Some(input);
            }
            Err(e) => {
                tracing::warn!("block {}: re-adopt failed, PTY parked: {}", self.id, e.error);
                self.parked = Some(e.detached);
            }
        }
    }

    /// Write to the program; dropped once it has exited (or never ran).
    fn send_input(&self, bytes: Vec<u8>) {
        if let Some(input) = self.input.as_ref().filter(|_| self.lifecycle.is_running()) {
            input.send(bytes);
        }
    }

    /// Report the first change since the last checkpoint.
    fn mark_dirty(&mut self) {
        if std::mem::take(&mut self.dirty_armed) {
            let _ = self.notes.send(Note::Dirty(self.id.clone()));
        }
    }

    /// Raise a printed host offer: valid tickets only, each once, at most
    /// [`OFFER_LIMIT`] per [`OFFER_WINDOW`].
    fn offer(&mut self, offer: Offer) {
        let now = Instant::now();
        while self.offer_times.front().is_some_and(|t| now.duration_since(*t) >= OFFER_WINDOW) {
            self.offer_times.pop_front();
        }
        if self.offer_times.len() >= OFFER_LIMIT || self.offered.contains(&offer.ticket) {
            return;
        }
        let source = OfferSource::Terminal { block_id: self.id.clone() };
        let Some(host) = crate::auth::host_offer(&offer.ticket, &offer.name, source) else {
            return;
        };
        self.offer_times.push_back(now);
        self.offered.push_back(offer.ticket);
        if self.offered.len() > OFFERED_CAP {
            self.offered.pop_front();
        }
        let _ = self.notes.send(Note::Offer(host));
    }

    fn checkpoint(&mut self) -> Result<Checkpoint, String> {
        let snapshot = self.vt.snapshot().map_err(|e| e.to_string())?;
        self.dirty_armed = true;
        Ok(Checkpoint {
            snapshot,
            size: self.size,
            config: self.config,
            epoch: self.epoch,
            offset: self.ring_end(),
            lifecycle: self.lifecycle,
            exit_code: self.exit_code,
            osc_title: self.osc_title.clone(),
            osc_pwd: self.osc_pwd.clone(),
        })
    }

    /// Program output: host offers are taken out before it is applied.
    fn output(&mut self, raw: &[u8]) {
        if self.scanner.is_idle() && !raw.contains(&0x1b) {
            return self.apply(raw);
        }
        let mut bytes = Vec::with_capacity(raw.len());
        for offer in self.scanner.feed(raw, &mut bytes) {
            self.offer(offer);
        }
        if !bytes.is_empty() {
            self.apply(&bytes);
        }
    }

    /// Both the terminal parser and the offer scanner are between sequences.
    fn at_ground(&mut self) -> bool {
        self.scanner.is_idle() && self.vt.at_ground().unwrap_or(true)
    }

    /// Pass on output the offer scanner holds, unparsed (before an
    /// out-of-band write that cannot wait longer, or a handoff).
    fn release_held(&mut self) {
        if !self.scanner.is_idle() {
            let mut held = Vec::new();
            self.scanner.release(&mut held);
            self.apply(&held);
        }
    }

    /// Feed output to the terminal, the ring and subscribers.
    fn apply(&mut self, bytes: &[u8]) {
        let offset = self.ring_end();
        self.mark_dirty();
        match self.vt.write_output(bytes) {
            Ok(replies) if !replies.is_empty() => {
                self.send_input(replies);
            }
            _ => {}
        }
        self.ring.extend(bytes);
        if self.ring.len() > RING_CAP {
            let excess = self.ring.len() - RING_CAP;
            self.ring.drain(..excess);
            self.ring_start = self.ring_start.saturating_add(excess as u64);
            self.log.trim(self.ring_start);
        }
        self.broadcast(&ServerData::Output { offset, bytes: bytes.to_vec() });
        if self.remote_attached() {
            self.poll_mode();
        }
        if let Ok(events) = self.vt.take_events() {
            if events.bells > 0 {
                let _ = self.notes.send(Note::Bell(self.id.clone()));
            }
            let mut changed = false;
            if let Some(title) = events.title {
                self.osc_title = title;
                changed = true;
            }
            if let Some(pwd) = events.pwd {
                self.osc_pwd = Some(pwd);
                changed = true;
            }
            if changed {
                self.probe();
            }
        }
    }

    /// Apply pending injections at a ground boundary (or after waiting
    /// `INJECT_WAIT` for one).
    fn flush_injects(&mut self) {
        let Some(since) = self.inject_since else {
            return;
        };
        if !self.at_ground() && since.elapsed() < INJECT_WAIT {
            return;
        }
        self.inject_since = None;
        self.release_held();
        while let Some((bytes, input)) = self.injects.pop_front() {
            self.apply(&bytes);
            if !input.is_empty() {
                self.send_input(input);
            }
        }
    }

    /// Queue `config` to become effective at the next ground boundary.
    fn request_config(&mut self, config: TerminalConfig) {
        if config == self.config && self.pending_config.is_none() {
            return;
        }
        let since = self.pending_config.map_or_else(Instant::now, |(_, since)| since);
        self.pending_config = Some((config, since));
    }

    /// Install a pending configuration at a parser-ground boundary (or after
    /// waiting `INJECT_WAIT` for one) and announce it at exactly that point
    /// of the output stream, so every mirror switches between the same two
    /// bytes the server terminal did. Program OSC overrides survive.
    fn flush_config(&mut self) {
        let Some((config, since)) = self.pending_config else {
            return;
        };
        if !self.at_ground() && since.elapsed() < INJECT_WAIT {
            return;
        }
        self.pending_config = None;
        if config == self.config {
            return;
        }
        self.release_held();
        let report = install_config(&mut self.vt, &config);
        if !report.is_empty() {
            // Mode 2031: the program asked to hear about scheme changes.
            self.send_input(report);
        }
        self.config = config;
        self.mark_dirty();
        self.log.record(self.ring_end(), Transition::Config(config));
        self.broadcast(&ServerData::Configured(config));
    }

    /// The configuration that should be effective: the owner's, else the
    /// most recently reported one, else the current one.
    fn wanted_config(&self) -> Option<TerminalConfig> {
        match self.owner.and_then(|o| self.configs.get(&o)) {
            Some((config, _)) => Some(*config),
            None => self.configs.values().max_by_key(|(_, at)| *at).map(|(config, _)| *config),
        }
    }

    /// Send to every subscriber, cutting off those that fell too far behind.
    /// Never blocks: slow subscribers only grow their own queue until cut.
    fn broadcast(&mut self, data: &ServerData) {
        if self.subscribers.is_empty() {
            return;
        }
        let frame = data.to_frame();
        let ring_start = self.ring_start;
        let mut cut = Vec::new();
        for (id, sub) in &mut self.subscribers {
            if !sub.send(frame.clone()) || sub.lagging(ring_start) {
                cut.push(*id);
            }
        }
        for id in cut {
            self.cut(id);
        }
    }

    /// Announce each stream's newly written inputs (one `INPUT_ACK` per
    /// stream per actor turn).
    fn flush_input_acks(&mut self) {
        for sub in self.subscribers.values_mut() {
            if sub.inputs > sub.inputs_acked {
                sub.inputs_acked = sub.inputs;
                sub.send(ServerData::InputAck { count: sub.inputs, offset: sub.input_offset }.to_frame());
            }
        }
    }

    fn remote_attached(&self) -> bool {
        self.subscribers.values().any(|sub| sub.remote)
    }

    /// Read the line discipline flags and announce a change at the current
    /// end of the output stream to remote streams.
    fn poll_mode(&mut self) {
        self.last_mode_poll = Instant::now();
        let Some(mode) = self.pty.as_ref().and_then(|pty| pty.line_mode().ok()) else {
            return;
        };
        if self.tty_mode.replace(mode) != Some(mode) {
            let (echo, icanon) = mode;
            let frame = ServerData::TtyMode { offset: self.ring_end(), echo, icanon }.to_frame();
            for sub in self.subscribers.values_mut().filter(|sub| sub.remote) {
                sub.send(frame.clone());
            }
        }
    }

    /// Feed bulk sends to writers with room.
    fn pump(&mut self) {
        let mut cut = Vec::new();
        for (id, sub) in &mut self.subscribers {
            if !sub.pending.is_empty() && !sub.pump() {
                cut.push(*id);
            }
        }
        for id in cut {
            self.cut(id);
        }
    }

    /// Drop a subscriber and end its stream now; the client reconnects and,
    /// its resume point most likely gone, gets a fresh snapshot.
    fn cut(&mut self, id: u64) {
        if let Some(sub) = self.subscribers.remove(&id) {
            tracing::debug!("block {}: cutting off slow subscriber {id}", self.id);
            sub.sink.cancel();
        }
    }

    fn ring_end(&self) -> u64 {
        self.ring_start.saturating_add(self.ring.len() as u64)
    }

    /// Queue ring output from `from` to the end, interleaved with later
    /// transitions at their offsets. `from` must lie within the ring.
    fn send_ring_from(&self, sub: &mut Subscriber, from: u64, changes: Vec<(u64, Transition)>) {
        let mut pos = from;
        for (at, transition) in changes {
            self.send_ring(sub, pos, at);
            sub.send(transition.to_data(at).to_frame());
            pos = pos.max(at);
        }
        self.send_ring(sub, pos, self.ring_end());
    }

    fn send_ring(&self, sub: &mut Subscriber, from: u64, to: u64) {
        let index = |at: u64| at.checked_sub(self.ring_start).and_then(|i| usize::try_from(i).ok());
        let (Some(start), Some(end)) = (index(from), index(to)) else {
            return;
        };
        let end = end.min(self.ring.len());
        if start >= end {
            return;
        }
        let bytes: Vec<u8> = self.ring.range(start..end).copied().collect();
        sub.send_bulk(Out::Output { offset: from, bytes, pos: 0 });
    }

    fn attach(&mut self, attach: Attach) {
        if attach.remote {
            self.poll_mode();
        }
        let end = self.ring_end();
        let welcome = |mode, offset, size| Frame::json(kind::WELCOME, &Welcome { epoch: self.epoch, mode, offset, size }).expect("welcome");
        // Resume needs the configuration and size of every replayed byte;
        // offsets before the known history take a snapshot.
        let start = self.ring_start.max(self.log.floor);
        let resumable = attach.resume.filter(|r| r.epoch == self.epoch && (start..=end).contains(&r.offset));
        let mut sub;
        if let Some(Resume { offset: from, .. }) = resumable {
            sub = Subscriber::new(attach.sink, from, attach.remote);
            let (state, changes) = self.log.from(from);
            sub.send(welcome(StartMode::Resume, from, state.size));
            sub.send(ServerData::Configured(state.config).to_frame());
            self.send_ring_from(&mut sub, from, changes);
        } else {
            let (mut head, head_len) = match self.vt.snapshot().map_err(|e| e.to_string()).and_then(|s| {
                let head = silo_vt::snapshot::head_len(&s).map_err(|e| e.to_string())?;
                Ok((s, head))
            }) {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    tracing::error!("block {}: attach failed, cannot snapshot: {e}", self.id);
                    let reject = Reject { code: "snapshot_failed".into(), reason: format!("cannot snapshot block: {e}") };
                    let _ = attach.sink.send(Frame::json(kind::REJECT, &reject).expect("reject"));
                    return;
                }
            };
            sub = Subscriber::new(attach.sink, end, attach.remote);
            let history = head.split_off(head_len.min(head.len()));
            sub.send(welcome(StartMode::Snapshot, end, self.size));
            // The snapshot already reflects the current configuration.
            sub.send(ServerData::Configured(self.config).to_frame());
            sub.send(ServerData::SnapshotHead(head).to_frame());
            sub.send_bulk(Out::History { bytes: history, pos: 0 });
            sub.send(ServerData::SnapshotDone.to_frame());
        }
        if let Some((echo, icanon)) = self.tty_mode.filter(|_| attach.remote && self.pty.is_some()) {
            sub.send(ServerData::TtyMode { offset: end, echo, icanon }.to_frame());
        }
        if !self.lifecycle.is_running() {
            sub.send(ServerData::Exited { code: self.exit_code }.to_frame());
        }
        if sub.pump() {
            self.subscribers.insert(attach.sub_id, sub);
        } else {
            sub.sink.cancel();
        }
    }

    fn own(&mut self, client: ClientId, mode: OwnMode) {
        self.candidates.retain(|c| *c != client);
        self.candidates.push(client);
        if mode == OwnMode::Take || self.owner.is_none() {
            if self.owner != Some(client) {
                self.owner = Some(client);
                self.publish();
            }
            if let Some(size) = self.desired.get(&client).map(|d| d.size) {
                self.apply_size(size);
            }
            if let Some(config) = self.wanted_config() {
                self.request_config(config);
            }
        }
    }

    fn disown(&mut self, client: ClientId) {
        self.candidates.retain(|c| *c != client);
        if self.owner == Some(client) {
            self.owner = self.candidates.last().copied();
            let size = match self.owner {
                Some(owner) => self.desired.get(&owner).map(|d| d.size),
                None => self.desired.values().max_by_key(|d| d.updated_at).map(|d| d.size),
            };
            if let Some(size) = size {
                self.apply_size(size);
            }
            if let Some(config) = self.wanted_config() {
                self.request_config(config);
            }
            self.publish();
        }
    }

    fn apply_size(&mut self, size: TermSize) {
        let g = grid(size);
        let size = TermSize { columns: g.columns, rows: g.rows, cell_width: size.cell_width, cell_height: size.cell_height };
        if size == self.size {
            return;
        }
        if let Some(pty) = &self.pty {
            let _ = pty.resize(pty_size(size));
        }
        if self.vt.resize(g).is_err() {
            return;
        }
        self.size = size;
        self.mark_dirty();
        let offset = self.ring_end();
        self.log.record(offset, Transition::Resize(size));
        self.broadcast(&ServerData::Resized { offset, size });
        self.publish();
    }

    fn capture(&mut self, history: bool) -> Capture {
        let mut lines = Vec::new();
        let state = self.vt.scroll_state().unwrap_or_default();
        let start = if history { 0 } else { state.total.saturating_sub(state.len) };
        let mut row = start;
        while row < state.total.max(1) {
            self.vt.scroll_to_row(row);
            let Ok(frame) = self.vt.frame() else { break };
            let columns = usize::from(frame.size.columns).max(1);
            for (i, cells) in frame.cells.chunks(columns).enumerate() {
                if frame.scroll.offset + (i as u64) < row {
                    continue;
                }
                let line: String = cells.iter().map(|c| if c.text.is_empty() { " " } else { c.text.as_str() }).collect();
                lines.push(line.trim_end().to_owned());
            }
            let next = frame.scroll.offset + frame.scroll.len;
            if next <= row {
                break;
            }
            row = next;
        }
        self.vt.scroll_to_row(u64::MAX);
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        Capture { text: lines.join("\n"), size: self.size }
    }

    /// Refresh title and cwd from OSC state and the foreground process.
    fn probe(&mut self) {
        self.last_probe = Instant::now();
        // A dead child has no foreground process to inspect.
        let Some(pty) = self.pty.as_ref().filter(|_| !self.child_exited) else {
            return;
        };
        let foreground = pty.foreground_pid();
        let pid = pty.pid();
        let title = if self.osc_title.is_empty() {
            foreground.and_then(silo_pty::process_name).unwrap_or_else(|| "shell".to_owned())
        } else {
            self.osc_title.clone()
        };
        let cwd = self
            .osc_pwd
            .as_deref()
            .and_then(pwd_path)
            .or_else(|| silo_pty::process_cwd(foreground.unwrap_or(pid)).map(|p| p.to_string_lossy().into_owned()));
        let changed = {
            let Ok(mut info) = self.info.lock() else {
                return;
            };
            let changed = info.title != title || info.cwd != cwd;
            info.title = title;
            info.cwd = cwd;
            changed
        };
        if changed {
            let _ = self.notes.send(Note::Changed(self.id.clone()));
        }
    }

    fn publish(&mut self) {
        let owner_name = self.owner.and_then(|o| self.desired.get(&o)).map(|d| d.label.clone());
        if let Ok(mut info) = self.info.lock() {
            info.size = self.size;
            info.owner = self.owner;
            info.owner_name = owner_name;
        }
        let _ = self.notes.send(Note::Changed(self.id.clone()));
    }

    fn finish(&mut self) {
        let code = self.exit_code;
        // Stay alive as a read-only block: drop the PTY and the input path.
        self.lifecycle = Lifecycle::Exited;
        self.input = None;
        self.gate.open();
        self.pty = None;
        self.tx = None;
        self.injects.clear();
        self.inject_since = None;
        self.broadcast(&ServerData::Exited { code });
        if let Ok(mut info) = self.info.lock() {
            info.exited = code;
            info.lifecycle = Lifecycle::Exited;
        }
        let _ = self.notes.send(Note::Exited(self.id.clone()));
    }
}

/// Install `config` as the engine's defaults; returns a mode-2031 scheme
/// report to write to the program (usually empty).
fn install_config(vt: &mut TerminalCore, config: &TerminalConfig) -> Vec<u8> {
    let (colors, defaults) = engine_config(config);
    let report = vt.set_colors(&colors).unwrap_or_default();
    if let Err(error) = vt.set_defaults(&defaults) {
        tracing::warn!("terminal defaults rejected: {error}");
    }
    report
}

pub fn engine_config(config: &TerminalConfig) -> (TerminalColors, TerminalDefaults) {
    let colors = TerminalColors {
        scheme: if config.is_light() { ColorScheme::Light } else { ColorScheme::Dark },
        foreground: config.foreground,
        background: config.background,
        cursor: config.cursor,
        ansi: config.palette,
    };
    let defaults = TerminalDefaults {
        cursor_shape: Some(match config.cursor_shape {
            CursorShape::Block => silo_vt::CursorShape::Block,
            CursorShape::Bar => silo_vt::CursorShape::Bar,
            CursorShape::Underline => silo_vt::CursorShape::Underline,
            CursorShape::Hollow => silo_vt::CursorShape::Hollow,
        }),
        cursor_blink: Some(config.cursor_blink),
        scrollback_bytes: config.scrollback_bytes(),
        scrollback_lines: config.scrollback_lines,
    };
    (colors, defaults)
}

/// Path from an OSC 7 `file://host/path` URI (or a bare path).
fn pwd_path(pwd: &str) -> Option<String> {
    let rest = pwd.strip_prefix("file://").unwrap_or(pwd);
    let path = if pwd.starts_with("file://") { &rest[rest.find('/')?..] } else { rest };
    let mut out = Vec::new();
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&path[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok().filter(|p| p.starts_with('/'))
}

#[cfg(test)]
mod tests {
    #[test]
    fn osc7_paths() {
        assert_eq!(super::pwd_path("file://host/Users/me/a%20b").as_deref(), Some("/Users/me/a b"));
        assert_eq!(super::pwd_path("/tmp").as_deref(), Some("/tmp"));
    }

    #[test]
    fn transition_log_places_changes_at_offsets() {
        use super::Transition::{Config, Resize};
        use silo_proto::model::{TermSize, TerminalConfig};
        let c = |background| Config(TerminalConfig { background, ..TerminalConfig::default() });
        let base = |background| TerminalConfig { background, ..TerminalConfig::default() };
        let s = |columns| TermSize::new(columns, 10);
        let mut log = super::TransitionLog::new(base(1), s(40));
        log.record(10, c(2));
        log.record(10, Resize(s(50)));
        log.record(20, c(3));
        let (state, later) = log.from(5);
        assert_eq!((state.config, state.size), (base(1), s(40)));
        assert_eq!(later, vec![(10, c(2)), (10, Resize(s(50))), (20, c(3))]);
        let (state, later) = log.from(10);
        assert_eq!((state.config, state.size, later), (base(2), s(50), vec![(20, c(3))]));
        log.record(20, c(4));
        assert_eq!(log.from(15).1, vec![(20, c(4))]);
        log.trim(12);
        assert_eq!(log.floor, 12);
        assert_eq!(log.from(12).0.size, s(50));
        for i in 0..super::TRANSITION_LOG_CAP as u32 {
            log.record(100 + u64::from(i), c(10 + i));
        }
        assert_eq!(log.changes.len(), super::TRANSITION_LOG_CAP);
        assert_eq!(log.floor, 20, "older resume points need a snapshot");
        assert_eq!(log.from(20).0.config, base(4));
    }
}
