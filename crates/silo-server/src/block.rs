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
    control::{AttentionReason, Capture, CommandOutput, DesiredSize, HostOffer, MAX_COMMAND_OUTPUT, OfferSource, OwnMode, RpcError, SizeInfo},
    data::{
        Clip, ClipKind, ClipboardLocation, CommandAnchor, CommandEnd, MAX_CLIP_BYTES, MAX_CLIP_TEXT, Reject, Resume, ServerData, StartMode, Welcome,
    },
    kind,
    model::{
        BlockId, BlockInfo, BlockStatus, Busy, ClientId, CommandRecord, CursorShape, Lifecycle, Progress, StatusKind, StatusState, TermSize,
        TerminalConfig,
    },
    offer::{Offer, OfferScanner},
    text::{MAX_BODY, MAX_TITLE, untrusted},
};
use silo_pty::{Pty, PtyEvent, PtyInput, PtySize, SpawnConfig};
use silo_vt::{
    ColorScheme, GridSize, PromptKind, PromptStep, ReportKind, ReportState, RestoreOptions, TerminalColors, TerminalCore, TerminalDefaults,
    TrackedRow, VtEvent,
};

use crate::{
    prompts::{CommandLog, keep_end},
    status::{self, BusyInputs, PROGRESS_INTERVAL, PROGRESS_TTL, Pacer, Records, Report},
};
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
/// The driving client's stream (see `Actor::driver`) may be this many
/// output bytes behind (sent, not yet applied by it); beyond that the PTY is
/// not read, so a program printing faster than the driver's link or
/// renderer slows to its pace, as on a local terminal or over SSH, instead
/// of the stream falling behind until it is cut and re-synchronised.
const DRIVER_WINDOW: u64 = 1024 * 1024;
/// A driver that stays more than `DRIVER_WINDOW` behind for this long stops
/// holding the PTY (until it catches up): a stalled client never stalls the
/// program for long.
const HOLD_LIMIT: Duration = Duration::from_secs(2);
/// Least time between clipboard writes a block sends; writes in between
/// coalesce, the last one wins.
const CLIP_WRITE_INTERVAL: Duration = Duration::from_millis(250);

/// Back-pressure from the actor to the PTY reader thread.
#[derive(Default)]
struct ReadGate {
    state: Mutex<GateState>,
    room: Condvar,
}

#[derive(Default)]
struct GateState {
    /// Bytes queued for the actor.
    queued: usize,
    /// The driving client is too far behind: read nothing more.
    held: bool,
    /// Waiting is disabled.
    open: bool,
}

impl ReadGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reader side: queue `len` bytes via `send`, then wait for room.
    fn read(&self, len: usize, send: impl FnOnce()) {
        self.lock().queued += len;
        send();
        let state = self.lock();
        drop(self.room.wait_while(state, |s| (s.queued > READ_AHEAD || s.held) && !s.open));
    }

    /// Actor side: `len` queued bytes were taken.
    fn done(&self, len: usize) {
        let mut state = self.lock();
        state.queued = state.queued.saturating_sub(len);
        if state.queued <= READ_AHEAD {
            self.room.notify_all();
        }
    }

    /// Actor side: hold or release reading for the driving client.
    fn hold(&self, held: bool) {
        self.lock().held = held;
        if !held {
            self.room.notify_all();
        }
    }

    /// Stop throttling, so the reader thread can be stopped and joined.
    fn open(&self) {
        self.lock().open = true;
        self.room.notify_all();
    }

    /// Throttle again from empty (a re-adopted PTY).
    fn reset(&self) {
        *self.lock() = GateState::default();
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
    /// Set attached clients' clipboard; replies how many streams got it.
    ClipboardWrite {
        text: String,
        reply: tokio::sync::oneshot::Sender<usize>,
    },
    /// The host's system clipboard changed to `text`: set remote clients'
    /// clipboard (local ones share the host's).
    HostClipboard {
        text: String,
    },
    /// Ask attached clients for their clipboard: the reply is the first
    /// answer with content, else `None` once every stream has answered
    /// (or detached), or right away when none is attached.
    ClipboardRead {
        kind: ClipKind,
        reply: tokio::sync::oneshot::Sender<Option<Clip>>,
    },
    /// Stream `sub_id`'s answer to a clipboard read.
    ClipboardContent {
        sub_id: u64,
        request: u64,
        content: Option<Clip>,
    },
    /// Splice `bytes` into the output stream (all clients and the server
    /// terminal see them in the same order) at the next parser-ground
    /// boundary, then write `input` to the program.
    Inject {
        bytes: Vec<u8>,
        input: Vec<u8>,
    },
    /// Report status through the same path as OSC 7501.
    Report {
        report: Report,
        reply: tokio::sync::oneshot::Sender<()>,
    },
    Notify {
        title: String,
        body: String,
        reply: tokio::sync::oneshot::Sender<()>,
    },
    /// Mark `done`/`error` records seen (`ids` omitted: all).
    StatusSeen {
        ids: Option<Vec<String>>,
        reply: tokio::sync::oneshot::Sender<()>,
    },
    /// The last `limit` command records.
    Commands {
        limit: Option<u32>,
        reply: tokio::sync::oneshot::Sender<Vec<CommandRecord>>,
    },
    /// A finished command's output (`index` omitted: the last).
    CommandOutput {
        index: Option<u64>,
        reply: tokio::sync::oneshot::Sender<Result<CommandOutput, RpcError>>,
    },
    /// Re-probe the foreground and answer whether the block is busy.
    Busy {
        reply: tokio::sync::oneshot::Sender<Option<Busy>>,
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
    /// Started with an explicit command rather than the login shell.
    #[serde(default)]
    pub command: bool,
    /// Block status and command records (Phase 7 §5.5).
    #[serde(default)]
    pub status: Option<SavedStatus>,
}

/// A command's row in a checkpoint's snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedAnchor {
    pub record: u64,
    pub rows_from_bottom: u32,
    #[serde(default)]
    pub column: u16,
}

/// What a [`Checkpoint`] carries of a block's status: everything on a hot
/// upgrade (the program lives on); a restore without the program runs the
/// exit rule on it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedStatus {
    #[serde(default)]
    pub records: Records,
    #[serde(default)]
    pub progress: Option<Progress>,
    #[serde(default)]
    pub busy: Option<Busy>,
    /// OSC 133: a command started (C) and has not ended.
    #[serde(default)]
    pub marks: Option<String>,
    #[serde(default)]
    pub commands: CommandLog,
    /// Where commands ended (D) and where their output started (C).
    #[serde(default)]
    pub ends: Vec<SavedAnchor>,
    #[serde(default)]
    pub outputs: Vec<SavedAnchor>,
}

/// Notifications from block actors to the registry.
#[derive(Debug)]
pub enum Note {
    Changed(BlockId),
    Exited(BlockId),
    Bell(BlockId),
    /// The block's status (`BlockHandle::status`) changed.
    Status(BlockId),
    /// Something in the block wants attention; text is sanitized.
    Attention {
        block_id: BlockId,
        reason: AttentionReason,
        title: String,
        body: String,
        at: u64,
    },
    /// A command finished (OSC 133 D).
    CommandFinished {
        block_id: BlockId,
        record: CommandRecord,
    },
    /// A program printed a valid host offer.
    Offer(HostOffer),
    /// Terminal content or configuration changed for the first time since
    /// the last [`Checkpoint`]; sent once per checkpoint.
    Dirty(BlockId),
}

pub struct BlockHandle {
    pub tx: mpsc::Sender<BlockMsg>,
    pub info: Arc<Mutex<BlockInfo>>,
    /// The status last published (never bumps the session revision).
    pub status: Arc<Mutex<BlockStatus>>,
    /// Runs an explicit command rather than the login shell.
    pub command: bool,
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
    /// The client the stream belongs to.
    client: ClientId,
}

impl Subscriber {
    fn new(sink: Sink, start: u64, remote: bool, client: ClientId) -> Self {
        Self {
            remote,
            client,
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
        self.pump_within(PUMP_WINDOW)
    }

    /// Hand every pending frame to the writer (the stream ends after it).
    fn flush(&mut self) {
        self.pump_within(usize::MAX);
    }

    fn pump_within(&mut self, window: usize) -> bool {
        while self.sink.backlog() < window {
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
    /// The client that last typed into the block or took it (focus): its
    /// configuration drives the server terminal (see `wanted_config`).
    driver: Option<ClientId>,
    /// Since when the PTY is held for the driver's stream (`DRIVER_WINDOW`).
    held_since: Option<Instant>,
    /// A driver stream that held past `HOLD_LIMIT`: not waited for again
    /// until it catches up.
    unheld: Option<u64>,
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
    /// Runs an explicit command rather than the login shell.
    command: bool,
    reader_closed: bool,
    /// Pending injections and when the first was queued.
    injects: VecDeque<(Vec<u8>, Vec<u8>)>,
    inject_since: Option<Instant>,
    /// Every client's desired configuration.
    configs: HashMap<ClientId, TerminalConfig>,
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
    /// Clipboard reads awaiting an answer with content, and the streams
    /// that have not answered yet.
    clip_reads: HashMap<u64, ClipRead>,
    next_clip_read: u64,
    /// When the last clipboard write went out, and a later one held back
    /// (with whether it is only for remote streams).
    clip_sent: Option<Instant>,
    clip_held: Option<(String, bool)>,
    /// Block status and command records.
    status: Status,
    /// What `status` last published.
    shared_status: Arc<Mutex<BlockStatus>>,
}

/// Block status and command records (Phase 7 §5, §7.3).
struct Status {
    records: Records,
    progress: Option<(Progress, Instant)>,
    busy: Option<Busy>,
    /// OSC 133: C without a D, A or RIS since, with its command line.
    marks: Option<String>,
    commands: CommandLog,
    /// Per command index: where its output started (C) and where it ended (D).
    anchors: HashMap<u64, (Option<TrackedRow>, Option<TrackedRow>)>,
    /// Attention: notifications over the pace are dropped; the latest
    /// record transition over it is `held` until the pace allows.
    pacer: Pacer,
    held: Option<HeldAttention>,
    /// State changes published at once, up to a pace; later ones wait.
    changes: Pacer,
    /// When status last went out, and whether a change waits.
    sent_at: Option<Instant>,
    pending: bool,
}

/// A record's attention held back by the pace: sent later only if the
/// record is still in `state` then.
struct HeldAttention {
    id: String,
    state: StatusState,
    reason: AttentionReason,
    title: String,
    body: String,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            records: Records::default(),
            progress: None,
            busy: None,
            marks: None,
            commands: CommandLog::default(),
            anchors: HashMap::new(),
            pacer: Pacer::default(),
            held: None,
            changes: Pacer::changes(),
            sent_at: None,
            pending: false,
        }
    }
}

impl Status {
    /// The status a checkpoint carried, re-pinned on `vt`. Without the
    /// program (`live` false) the exit rule runs: only done and error
    /// records stay, and nothing is busy or in progress.
    fn restore(vt: &mut TerminalCore, saved: Option<SavedStatus>, live: bool) -> Self {
        let Some(saved) = saved else { return Self::default() };
        let mut status = Self { records: saved.records, commands: saved.commands, ..Self::default() };
        if live {
            status.progress = saved.progress.map(|p| (p, Instant::now()));
            status.busy = saved.busy;
            status.marks = saved.marks;
        } else {
            status.records.settle();
            status.commands.abandon();
        }
        let running = status.commands.running();
        let keep = |record: u64| status.commands.indexes().any(|i| i == record) || running == Some(record);
        let kept: Vec<(bool, SavedAnchor)> =
            saved.ends.iter().map(|a| (true, *a)).chain(saved.outputs.iter().map(|a| (false, *a))).filter(|(_, a)| keep(a.record)).collect();
        for (end, anchor) in kept {
            if let Some(row) = vt.track_row(anchor.rows_from_bottom, anchor.column) {
                let slot = status.anchors.entry(anchor.record).or_default();
                if end {
                    slot.1 = Some(row);
                } else {
                    slot.0 = Some(row);
                }
            }
        }
        status
    }
}

struct ClipRead {
    reply: tokio::sync::oneshot::Sender<Option<Clip>>,
    waiting: std::collections::HashSet<u64>,
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
    let mut vt = TerminalCore::server(grid(size)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let terminal = TerminalConfig::default();
    install_config(&mut vt, &terminal);
    launch(BlockInfo { size, ..info }, vt, terminal, Carried::default(), config, notes)
}

/// Rebuild a block from a [`Checkpoint`] around a fresh program: the
/// restored screen and history stay above a separator line. The stream
/// starts a new epoch.
///
/// # Errors
/// Fails if the snapshot cannot be restored or the program not spawned.
pub fn spawn_resumed(info: BlockInfo, checkpoint: Checkpoint, config: &SpawnConfig, notes: UnboundedSender<Note>) -> anyhow::Result<BlockHandle> {
    let (mut vt, terminal) = restore_vt(&checkpoint)?;
    // The old program is gone: the exit rule runs. Rows are re-pinned
    // before the banner moves them.
    let status = Status::restore(&mut vt, checkpoint.status, false);
    let banner = resume_banner(&mut vt)?;
    vt.write_output(banner.as_bytes()).map_err(|e| anyhow::anyhow!("{e}"))?;
    drop(vt.take_events());
    let mut info = info;
    info.size = checkpoint.size;
    info.owner = None;
    info.owner_name = None;
    info.exited = None;
    launch(info, vt, terminal, Carried { offset: checkpoint.offset, osc_title: checkpoint.osc_title, status }, config, notes)
}

/// Leave whatever modes the old program set, then a dim separator line
/// below the cursor.
fn resume_banner(vt: &mut TerminalCore) -> anyhow::Result<String> {
    let frame = vt.frame().map_err(|e| anyhow::anyhow!("{e}"))?;
    let leave_alt = if frame.alt_screen { "\x1b[?1049l" } else { "" };
    let reset = "\x1b[!p\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[<99u\x1b[=0u";
    let newline = if frame.alt_screen || frame.cursor.column > 0 { "\r\n" } else { "\r" };
    Ok(format!("{leave_alt}{reset}{newline}\x1b[2m\u{2014} restored {} \u{2014}\x1b[0m\r\n", local_time()))
}

/// `YYYY-MM-DD HH:MM` in local time.
#[allow(unsafe_code)]
fn local_time() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let now = libc::time_t::try_from(now).unwrap_or_default();
    // SAFETY: `localtime_r` only writes the `tm` it is given.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&raw const now, &raw mut tm);
        tm
    };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

/// What a launched block carries over from a checkpoint: where its stream
/// starts, the title until the program sets one, and its status.
#[derive(Default)]
struct Carried {
    offset: u64,
    osc_title: String,
    status: Status,
}

/// Run `config`'s program in a PTY behind `vt`, carrying `carried` over.
fn launch(
    mut info: BlockInfo,
    vt: TerminalCore,
    terminal: TerminalConfig,
    carried: Carried,
    config: &SpawnConfig,
    notes: UnboundedSender<Note>,
) -> anyhow::Result<BlockHandle> {
    let (tx, rx) = mpsc::channel();
    let exit_tx = tx.clone();
    let mut pty = Pty::spawn(config, pty_size(info.size), move |status| {
        let _ = exit_tx.send(BlockMsg::ChildExited(status.and_then(|s| s.code())));
    })?;
    let gate = Arc::new(ReadGate::default());
    let input = start_io(&mut pty, &info.id, &tx, &gate)?;
    info.pid = Some(pty.pid());
    info.lifecycle = Lifecycle::Running;
    let mut actor = Actor::new(&info, vt, terminal, notes);
    actor.gate = gate;
    actor.ring_start = carried.offset;
    actor.log.floor = carried.offset;
    actor.osc_title = carried.osc_title;
    actor.status = carried.status;
    actor.command = !config.command.is_empty() && !crate::shell::is_hooked_login(&config.command);
    actor.pty = Some(pty);
    actor.input = Some(input);
    actor.tx = Some(tx.clone());
    start(actor, &info.id, tx, rx)
}

/// Rebuild a block from a [`Checkpoint`] without a process: its contents are
/// readable, input is ignored. An exited program stays [`Lifecycle::Exited`]
/// with its code, anything else becomes [`Lifecycle::Restored`]. The stream
/// starts a new epoch.
///
/// # Errors
/// Fails if the snapshot cannot be restored.
pub fn spawn_restored(info: BlockInfo, checkpoint: Checkpoint, notes: UnboundedSender<Note>) -> anyhow::Result<BlockHandle> {
    let (mut vt, config) = restore_vt(&checkpoint)?;
    let status = Status::restore(&mut vt, checkpoint.status.clone(), false);
    let exited = checkpoint.lifecycle == Lifecycle::Exited;
    let lifecycle = if exited { Lifecycle::Exited } else { Lifecycle::Restored };
    let exit_code = checkpoint.exit_code.filter(|_| exited);
    let size = checkpoint.size;
    let mut info = info;
    info.size = size;
    info.pid = None;
    info.owner = None;
    info.owner_name = None;
    info.lifecycle = lifecycle;
    info.exited = exit_code;
    restore_osc(&mut info, &checkpoint);
    let mut actor = Actor::new(&info, vt, config, notes);
    actor.status = status;
    actor.lifecycle = lifecycle;
    actor.exit_code = exit_code;
    actor.command = checkpoint.command;
    actor.ring_start = checkpoint.offset;
    actor.log.floor = checkpoint.offset;
    (actor.osc_title, actor.osc_pwd) = (checkpoint.osc_title, checkpoint.osc_pwd);
    let (tx, rx) = mpsc::channel();
    start(actor, &info.id, tx, rx)
}

/// Run a block around a program's PTY released by [`BlockHandle::handoff`],
/// continuing its stream: epoch and offsets carry on from `checkpoint`, so
/// clients resume at the handoff offset without a gap. The exit code of an
/// adopted program comes from its PTY host; without one it is unknown
/// (`None`).
///
/// # Errors
/// Fails if the snapshot cannot be restored or the PTY not adopted.
pub fn spawn_adopted(info: BlockInfo, checkpoint: Checkpoint, pty: silo_pty::Detached, notes: UnboundedSender<Note>) -> anyhow::Result<BlockHandle> {
    let (mut vt, config) = match restore_vt(&checkpoint) {
        Ok(restored) => restored,
        Err(error) => {
            return Err(AdoptFailed { error, detached: pty }.into());
        }
    };
    let (tx, rx) = mpsc::channel();
    let gate = Arc::new(ReadGate::default());
    let (pty, input) = adopt_pty(&info.id, pty, &tx, &gate).map_err(|e| AdoptFailed { error: e.error.into(), detached: e.detached })?;
    // The program lives on: its status carries over as is.
    let status = Status::restore(&mut vt, checkpoint.status.clone(), true);
    let mut info = info;
    info.size = checkpoint.size;
    info.pid = Some(pty.pid());
    info.owner = None;
    info.owner_name = None;
    info.lifecycle = Lifecycle::Running;
    info.exited = None;
    restore_osc(&mut info, &checkpoint);
    let mut actor = Actor::new(&info, vt, config, notes);
    actor.status = status;
    actor.epoch = checkpoint.epoch;
    actor.command = checkpoint.command;
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
        move |status| {
            let _ = exit_tx.send(BlockMsg::ChildExited(status.and_then(|s| s.code())));
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
    vt.hook().map_err(|e| anyhow::anyhow!("{e}"))?;
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
        PtyEvent::Output(bytes) => {
            tracing::trace!(target: "silo::latency", stage = "pty_output");
            gate.read(bytes.len(), || drop(tx.send(BlockMsg::Output(bytes))));
        }
        PtyEvent::Closed => drop(tx.send(BlockMsg::ReaderClosed)),
    }
}

fn start(mut actor: Actor, id: &str, tx: mpsc::Sender<BlockMsg>, rx: mpsc::Receiver<BlockMsg>) -> anyhow::Result<BlockHandle> {
    let shared = actor.info.clone();
    let command = actor.command;
    if let Ok(mut status) = actor.shared_status.lock() {
        *status = actor.status_now();
    }
    let status = actor.shared_status.clone();
    std::thread::Builder::new().name(format!("silo-block-{id}")).spawn(move || actor.run(&rx))?;
    Ok(BlockHandle { tx, info: shared, status, command })
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
            driver: None,
            held_since: None,
            unheld: None,
            candidates: Vec::new(),
            info: Arc::new(Mutex::new(info.clone())),
            notes,
            osc_title: String::new(),
            osc_pwd: None,
            last_probe: Instant::now() - PROBE_INTERVAL,
            child_exited: false,
            exit_code: None,
            command: false,
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
            clip_reads: HashMap::new(),
            next_clip_read: 1,
            clip_sent: None,
            clip_held: None,
            status: Status::default(),
            shared_status: Arc::new(Mutex::new(BlockStatus { block_id: info.id.clone(), records: Vec::new(), progress: None, busy: None })),
        }
    }

    fn run(&mut self, rx: &mpsc::Receiver<BlockMsg>) {
        let mut exit_deadline: Option<Instant> = None;
        // A message taken while coalescing output, handled next.
        let mut next: Option<BlockMsg> = None;
        loop {
            let received = match next.take() {
                Some(msg) => Ok(msg),
                None => rx.recv_timeout(self.wait_timeout(exit_deadline)),
            };
            match received {
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
                Ok(BlockMsg::Output(mut bytes)) => {
                    self.gate.done(bytes.len());
                    // PTY reads already queued (at most 1 KiB each on
                    // macOS) become one chunk: one parse and one frame per
                    // subscriber instead of hundreds. Nothing waits for more.
                    while bytes.len() < OUTPUT_CHUNK {
                        match rx.try_recv() {
                            Ok(BlockMsg::Output(more)) => {
                                self.gate.done(more.len());
                                bytes.extend_from_slice(&more);
                            }
                            Ok(other) => {
                                next = Some(other);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    self.handle(BlockMsg::Output(bytes));
                }
                Ok(msg) => {
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
            self.flush_clipboard();
            self.tick_status();
            self.clip_reads.retain(|_, read| !read.reply.is_closed());
            if self.remote_attached() && self.last_mode_poll.elapsed() >= MODE_INTERVAL {
                self.poll_mode();
            }
            self.pump();
            self.update_hold();
            if self.child_exited && self.lifecycle.is_running() && (self.reader_closed || exit_deadline.is_some_and(|d| Instant::now() >= d)) {
                self.finish();
                exit_deadline = None;
            }
            if self.lifecycle.is_running() && self.last_probe.elapsed() >= PROBE_INTERVAL {
                self.probe();
            }
        }
    }

    /// How long the actor may wait for its next message.
    fn wait_timeout(&self, exit_deadline: Option<Instant>) -> Duration {
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
        if let (Some(_), Some(sent)) = (&self.clip_held, self.clip_sent) {
            timeout = timeout.min(CLIP_WRITE_INTERVAL.saturating_sub(sent.elapsed()));
        }
        if let Some(since) = self.held_since {
            timeout = timeout.min(HOLD_LIMIT.saturating_sub(since.elapsed()));
        }
        if let Some(wait) = self.status_wait() {
            timeout = timeout.min(wait);
        }
        if let Some((_, at)) = self.status.progress {
            timeout = timeout.min(PROGRESS_TTL.saturating_sub(at.elapsed()));
        }
        timeout
    }

    /// Hold the PTY while the driver's stream is more than `DRIVER_WINDOW`
    /// behind, for at most `HOLD_LIMIT` without it catching up.
    fn update_hold(&mut self) {
        let driver = self.driver;
        let behind = self
            .subscribers
            .iter()
            .filter(|(_, sub)| Some(sub.client) == driver)
            .map(|(id, sub)| (*id, sub.sent.saturating_sub(sub.acked)))
            .max_by_key(|(_, behind)| *behind);
        if let Some(id) = self.unheld
            && behind.is_none_or(|(driving, behind)| driving != id || behind <= DRIVER_WINDOW)
        {
            self.unheld = None;
        }
        // After the child exited the actor waits only briefly for the
        // output still in the PTY: never park the reader then.
        let hold = !self.child_exited && behind.is_some_and(|(id, behind)| behind > DRIVER_WINDOW && self.unheld != Some(id));
        match (hold, self.held_since) {
            (true, Some(since)) if since.elapsed() >= HOLD_LIMIT => {
                tracing::debug!(block = %self.id, "driver stalled; not holding output for it");
                self.unheld = behind.map(|(id, _)| id);
                self.held_since = None;
                self.gate.hold(false);
            }
            (true, None) => {
                self.held_since = Some(Instant::now());
                self.gate.hold(true);
            }
            (false, Some(_)) => {
                self.held_since = None;
                self.gate.hold(false);
            }
            _ => {}
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
                let client = self.subscribers.get(&sub_id).map(|sub| sub.client);
                if let Some(sub) = self.subscribers.get_mut(&sub_id).filter(|sub| sub.remote) {
                    sub.inputs += 1;
                    sub.input_offset = end;
                }
                tracing::trace!(target: "silo::latency", stage = "actor_input");
                // A focus report (mode 1004) is sent on focus changes, not
                // typed: losing focus must not make that client the driver.
                let typed = !is_focus_report(&bytes);
                self.send_input(bytes);
                if let Some(client) = client.filter(|_| typed) {
                    self.drive(client);
                }
            }
            BlockMsg::Attach(attach) => self.attach(*attach),
            BlockMsg::Detach(id) => {
                self.subscribers.remove(&id);
                self.clip_forget(id);
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
                self.configs.insert(client_id, config.sanitized());
                self.reconfigure();
            }
            BlockMsg::Own { client_id, mode } => self.own(client_id, mode),
            BlockMsg::Disown { client_id } => self.disown(client_id),
            BlockMsg::ClientGone { client_id } => {
                self.desired.remove(&client_id);
                self.configs.remove(&client_id);
                if self.driver == Some(client_id) {
                    self.driver = None;
                }
                self.disown(client_id);
                self.reconfigure();
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
            BlockMsg::ClipboardWrite { text, reply } => {
                let _ = reply.send(self.clipboard_write(text));
            }
            BlockMsg::HostClipboard { text } => {
                self.hold_clipboard(text, true);
            }
            BlockMsg::ClipboardRead { kind, reply } => {
                let request = self.next_clip_read;
                self.next_clip_read += 1;
                self.broadcast(&ServerData::ClipboardRead { request, kind });
                let waiting: std::collections::HashSet<u64> = self.subscribers.keys().copied().collect();
                if waiting.is_empty() {
                    let _ = reply.send(None);
                } else {
                    self.clip_reads.insert(request, ClipRead { reply, waiting });
                }
            }
            BlockMsg::ClipboardContent { sub_id, request, content } => {
                let content = content.filter(|c| c.size() <= MAX_CLIP_BYTES);
                let Some(read) = self.clip_reads.get_mut(&request) else { return false };
                if (content.is_some() || (read.waiting.remove(&sub_id) && read.waiting.is_empty()))
                    && let Some(read) = self.clip_reads.remove(&request)
                {
                    let _ = read.reply.send(content);
                }
            }
            BlockMsg::Inject { bytes, input } if self.lifecycle.is_running() => {
                self.injects.push_back((bytes, input));
                self.inject_since.get_or_insert_with(Instant::now);
            }
            BlockMsg::Inject { .. } => {}
            BlockMsg::Checkpoint { reply } => {
                let _ = reply.send(self.checkpoint());
            }
            BlockMsg::StatusSeen { ids, reply } => {
                if self.status.records.seen(ids.as_deref()) {
                    self.publish_status();
                }
                let _ = reply.send(());
            }
            BlockMsg::Report { report, reply } => {
                self.report(report);
                self.publish_status();
                let _ = reply.send(());
            }
            BlockMsg::Notify { title, body, reply } => {
                self.attention(AttentionReason::Notification, untrusted(&title, MAX_TITLE), untrusted(&body, MAX_BODY), None);
                let _ = reply.send(());
            }
            BlockMsg::Commands { limit, reply } => {
                let _ = reply.send(self.status.commands.records(limit));
            }
            BlockMsg::CommandOutput { index, reply } => {
                let _ = reply.send(self.command_output(index));
            }
            BlockMsg::Busy { reply } => {
                self.refresh_busy();
                let _ = reply.send(self.status.busy.clone());
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
                // Every stream gets all output up to the checkpoint, then
                // ends as its writer drains (the sink dropped here is the
                // last), so its client resumes on the next owner.
                for (_, mut sub) in self.subscribers.drain() {
                    sub.flush();
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
        self.held_since = None;
        self.unheld = None;
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
            command: self.command,
            osc_title: self.osc_title.clone(),
            osc_pwd: self.osc_pwd.clone(),
            status: Some(self.saved_status()),
        })
    }

    /// The status a checkpoint carries, anchors in its snapshot's rows.
    fn saved_status(&mut self) -> SavedStatus {
        let (mut ends, mut outputs) = (Vec::new(), Vec::new());
        for (&record, (output, end)) in &self.status.anchors {
            for (row, list) in [(output, &mut outputs), (end, &mut ends)] {
                if let Some(row) = row
                    && let Some(rows_from_bottom) = self.vt.rows_from_bottom(row)
                {
                    list.push(SavedAnchor { record, rows_from_bottom, column: row.column().unwrap_or(0) });
                }
            }
        }
        ends.sort_by_key(|a| a.record);
        outputs.sort_by_key(|a| a.record);
        SavedStatus {
            records: self.status.records.clone(),
            progress: self.status.progress.map(|(p, _)| p),
            busy: self.status.busy.clone(),
            marks: self.status.marks.clone(),
            commands: self.status.commands.clone(),
            ends,
            outputs,
        }
    }

    /// The status as published now.
    fn status_now(&self) -> BlockStatus {
        BlockStatus {
            block_id: self.id.clone(),
            records: self.status.records.published(),
            progress: self.status.progress.map(|(p, _)| p),
            busy: self.status.busy.clone(),
        }
    }

    /// Publish the status if it changed: state changes at once,
    /// progress-only changes at most every [`PROGRESS_INTERVAL`], all of
    /// them within [`Pacer::changes`]; anything over that waits for `tick_status`,
    /// so the last state always goes out. Never touches the session
    /// revision.
    fn publish_status(&mut self) {
        let status = self.status_now();
        let Ok(mut shared) = self.shared_status.lock() else { return };
        if *shared == status {
            self.status.pending = false;
            return;
        }
        let states = |s: &BlockStatus| s.records.iter().map(|r| (r.id.clone(), r.state, r.kind)).collect::<Vec<_>>();
        let urgent = states(&shared) != states(&status) || shared.busy != status.busy;
        // Every publish counts against the pace: steady progress (4/s)
        // keeps the bucket full, so state changes still go out at once.
        let due = urgent || self.status.sent_at.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL);
        let allowed = due && self.status.changes.allow(Instant::now());
        if !allowed {
            self.status.pending = true;
            return;
        }
        *shared = status;
        drop(shared);
        self.status.sent_at = Some(Instant::now());
        self.status.pending = false;
        self.mark_dirty();
        let _ = self.notes.send(Note::Status(self.id.clone()));
    }

    /// How long until a waiting status change or held attention is due.
    fn status_wait(&self) -> Option<Duration> {
        let now = Instant::now();
        let status = self.status.pending.then(|| {
            let interval = self.status.sent_at.map_or(Duration::ZERO, |at| PROGRESS_INTERVAL.saturating_sub(at.elapsed()));
            interval.max(self.status.changes.wait(now))
        });
        let held = self.status.held.as_ref().map(|_| self.status.pacer.wait(now));
        status.into_iter().chain(held).min()
    }

    /// Send a coalesced status change or held attention once due; drop
    /// stale progress.
    fn tick_status(&mut self) {
        if self.status.progress.is_some_and(|(_, at)| at.elapsed() >= PROGRESS_TTL) {
            self.status.progress = None;
            self.publish_status();
        }
        if self.status.pending
            && self.status.sent_at.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL)
            && self.status.changes.wait(Instant::now()).is_zero()
        {
            self.publish_status();
        }
        if self.status.held.is_some() && self.status.pacer.wait(Instant::now()).is_zero() {
            let held = self.status.held.take().expect("checked");
            if self.status.records.state(&held.id) == Some(held.state) {
                self.attention(held.reason, held.title, held.body, None);
            }
        }
    }

    /// Re-read the foreground and publish a change in `busy` (§5.7).
    fn refresh_busy(&mut self) {
        let foreground = self.pty.as_ref().and_then(Pty::foreground_pid);
        self.update_busy(foreground, foreground.and_then(silo_pty::process_name));
    }

    /// [`Self::refresh_busy`] with the foreground already read (by `probe`).
    fn update_busy(&mut self, foreground: Option<i32>, foreground_name: Option<String>) {
        let found = match self.pty.as_ref().filter(|_| self.lifecycle.is_running() && !self.child_exited) {
            Some(pty) => {
                let pid = pty.pid();
                // Only a command block whose program holds the foreground
                // (or none does) needs the program's own name.
                let pid_name = match foreground {
                    _ if !self.command => None,
                    Some(fg) if fg == pid => foreground_name.clone(),
                    Some(_) => None,
                    None => silo_pty::process_name(pid),
                };
                // The program leads its own group (it is the session leader).
                let group = if self.command && foreground.is_none_or(|fg| fg == pid) && pid_name.as_deref().is_some_and(status::harmless) {
                    silo_pty::process_group(pid).into_iter().filter(|&p| p != pid).filter_map(silo_pty::process_name).collect()
                } else {
                    Vec::new()
                };
                status::busy(&BusyInputs {
                    command_block: self.command,
                    pid,
                    pid_name,
                    foreground,
                    foreground_name,
                    group,
                    marks: self.status.marks.clone(),
                })
            }
            None => None,
        };
        let busy = status::busy_status(found, self.status.busy.as_ref(), now_ms());
        if busy != self.status.busy {
            self.status.busy = busy;
            self.publish_status();
        }
    }

    /// Raise attention within the pace. Over it, a notification is
    /// dropped and a record transition (`record`: id and state) is held,
    /// replacing an older held one, until the pace allows.
    fn attention(&mut self, reason: AttentionReason, title: String, body: String, record: Option<(String, StatusState)>) {
        if self.status.pacer.allow(Instant::now()) {
            if record.as_ref().is_some_and(|(id, _)| self.status.held.as_ref().is_some_and(|held| held.id == *id)) {
                self.status.held = None;
            }
            let _ = self.notes.send(Note::Attention { block_id: self.id.clone(), reason, title, body, at: now_ms() });
        } else if let Some((id, state)) = record {
            self.status.held = Some(HeldAttention { id, state, reason, title, body });
        }
    }

    /// Apply the server terminal's hook events in order. Returns the
    /// commands that ended (D) in this chunk.
    fn status_events(&mut self, events: Vec<VtEvent>, lost: bool) -> Vec<u64> {
        let mut ended = Vec::new();
        let mut changed = false;
        let mut marks = false;
        if lost {
            // The queue overflowed and its oldest events are gone (a CLEAR,
            // a prompt start, a C or D): records, progress and the open
            // command may be stale. Start them over from the events that
            // are left; busy is re-derived from the foreground below.
            // Finished commands and their anchors were complete and stay.
            tracing::warn!(block = %self.id, "status events were dropped; status starts over");
            changed |= self.status.records.clear("");
            changed |= self.status.progress.take().is_some();
            self.status.held = None;
            if let Some(index) = self.status.commands.abandon() {
                self.status.anchors.remove(&index);
            }
            self.status.marks = None;
            marks = true;
        }
        for event in events {
            match event {
                VtEvent::Status { state: ReportState::Clear, id, .. } => changed |= self.status.records.clear(&id),
                VtEvent::Status { state, kind, progress, id, app, title, message } => {
                    let state = match state {
                        ReportState::Idle => StatusState::Idle,
                        ReportState::Working => StatusState::Working,
                        ReportState::Done => StatusState::Done,
                        ReportState::Blocked => StatusState::Blocked,
                        ReportState::Error => StatusState::Error,
                        ReportState::Clear | ReportState::Unknown => StatusState::Unknown,
                    };
                    let kind = match kind {
                        ReportKind::None => None,
                        ReportKind::Permission => Some(StatusKind::Permission),
                        ReportKind::Question => Some(StatusKind::Question),
                        ReportKind::Auth => Some(StatusKind::Auth),
                        ReportKind::Unknown => Some(StatusKind::Unknown),
                    };
                    self.report(Report { id, state: Some(state), kind, progress, app, title, message });
                    changed = true;
                }
                VtEvent::Progress { state, percent } => {
                    self.status.progress = status::progress(state, percent).map(|p| (p, Instant::now()));
                    changed = true;
                }
                VtEvent::Notify { title, body } => {
                    self.attention(AttentionReason::Notification, untrusted(&title, MAX_TITLE), untrusted(&body, MAX_BODY), None);
                }
                VtEvent::Prompt { step: PromptStep::PromptStart, prompt_kind: PromptKind::Primary, .. } => {
                    changed |= self.status.records.settle();
                    changed |= self.status.progress.take().is_some();
                    if let Some(index) = self.status.commands.abandon() {
                        self.status.anchors.remove(&index);
                    }
                    marks |= self.status.marks.take().is_some();
                }
                VtEvent::Prompt { step: PromptStep::OutputStart, command, row, .. } => {
                    if let Some(index) = self.status.commands.abandon() {
                        self.status.anchors.remove(&index);
                    }
                    let index = self.status.commands.start(&command, now_ms());
                    self.status.anchors.insert(index, (row, None));
                    self.status.marks = Some(command);
                    marks = true;
                }
                VtEvent::Prompt { step: PromptStep::CommandEnd, exit, row, .. } => {
                    // No command behind it (see `CommandLog::end`): no record, and busy stays.
                    let Some((record, evicted)) = self.status.commands.end(exit, now_ms()) else { continue };
                    for index in evicted {
                        self.status.anchors.remove(&index);
                    }
                    self.status.anchors.entry(record.index).or_default().1 = row;
                    ended.push(record.index);
                    let _ = self.notes.send(Note::CommandFinished { block_id: self.id.clone(), record });
                    marks |= self.status.marks.take().is_some();
                }
                VtEvent::Prompt { .. } => {}
                VtEvent::Reset => {
                    // RIS also clears the title and cwd, which upstream does
                    // not report through the change callbacks.
                    self.status.records.clear("");
                    self.status.progress = None;
                    self.status.marks = None;
                    self.status.commands.abandon();
                    self.status.anchors.clear();
                    ended.clear();
                    self.osc_title.clear();
                    self.osc_pwd = None;
                    self.probe();
                    changed = true;
                    marks = true;
                }
            }
        }
        if marks {
            self.refresh_busy();
        }
        if changed {
            self.publish_status();
        }
        ended
    }

    fn report(&mut self, report: Report) {
        let heading = untrusted(if report.title.trim().is_empty() { &report.app } else { &report.title }, MAX_TITLE);
        let body = untrusted(&report.message, silo_proto::text::MAX_MESSAGE);
        let record = report.state.map(|state| (report.id.clone(), state));
        if let Some(reason) = self.status.records.report(report, now_ms()) {
            self.attention(reason, heading, body, record);
        }
    }

    fn command_output(&mut self, index: Option<u64>) -> Result<CommandOutput, RpcError> {
        let record = self.status.commands.finished(index).cloned().ok_or_else(|| RpcError::not_found("no such finished command"))?;
        let output = match self.status.anchors.get(&record.index) {
            Some((Some(row), end)) => self.vt.output_text(row, end.as_ref()).map_err(|e| RpcError::internal(e.to_string()))?,
            _ => silo_vt::OutputText::default(),
        };
        let (text, cut) = keep_end(output.text, MAX_COMMAND_OUTPUT);
        Ok(CommandOutput { text, record, truncated: output.cut || cut })
    }

    /// Row anchors of finished commands still in the terminal.
    fn command_anchors(&mut self) -> Vec<CommandAnchor> {
        let mut anchors: Vec<CommandAnchor> = self
            .status
            .anchors
            .iter()
            .filter_map(|(&record, (_, end))| {
                let rows_from_bottom = self.vt.rows_from_bottom(end.as_ref()?)?;
                Some(CommandAnchor { record, rows_from_bottom })
            })
            .collect();
        anchors.sort_by_key(|a| a.record);
        anchors
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

    /// Feed output to the ring and subscribers, then the terminal: clients
    /// parse for themselves, so they never wait for the server's parse (the
    /// actor's single thread keeps snapshots and offsets consistent).
    fn apply(&mut self, bytes: &[u8]) {
        let offset = self.ring_end();
        self.mark_dirty();
        self.ring.extend(bytes);
        if self.ring.len() > RING_CAP {
            let excess = self.ring.len() - RING_CAP;
            self.ring.drain(..excess);
            self.ring_start = self.ring_start.saturating_add(excess as u64);
            self.log.trim(self.ring_start);
        }
        self.broadcast(&ServerData::Output { offset, bytes: bytes.to_vec() });
        tracing::trace!(target: "silo::latency", stage = "server_output");
        match self.vt.write_output(bytes) {
            Ok(replies) if !replies.is_empty() => {
                self.send_input(replies);
            }
            _ => {}
        }
        if self.remote_attached() {
            self.poll_mode();
        }
        if let Ok(events) = self.vt.take_events() {
            if events.bells > 0 {
                let _ = self.notes.send(Note::Bell(self.id.clone()));
            }
            let ended = self.status_events(events.vt, events.lost);
            // After the whole chunk: the same line in every mirror.
            let end = self.ring_end();
            for record in ended {
                let row = self.status.anchors.get(&record).and_then(|(_, end)| end.as_ref());
                if let Some(rows_from_bottom) = row.and_then(|row| self.vt.rows_from_bottom(row)) {
                    self.broadcast(&ServerData::CommandEnd(CommandEnd { offset: end, record, rows_from_bottom }));
                }
            }
            for (_, text) in events.clipboard {
                self.clipboard_write(text);
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

    /// `client` typed into the block or took it: its configuration now
    /// drives the server terminal.
    fn drive(&mut self, client: ClientId) {
        if self.driver != Some(client) {
            self.driver = Some(client);
            self.reconfigure();
        }
    }

    /// Make the wanted configuration effective (at the next ground).
    fn reconfigure(&mut self) {
        if let Some(config) = self.wanted_config() {
            self.request_config(config);
        }
    }

    /// The configuration the server terminal answers colour queries,
    /// `CSI ? 996 n` and snapshots with (clients render with their own):
    /// the driver's, else the owner's (a client may hold either without
    /// reporting one, e.g. `silo attach`), else that of the client that
    /// connected first, so an unowned block (a server-created session)
    /// does not flip with every client that reports one.
    fn wanted_config(&self) -> Option<TerminalConfig> {
        wanted_config(&self.configs, self.driver, self.owner)
    }

    /// Hand clipboard text to every attached client (dropped above
    /// [`MAX_CLIP_TEXT`]; held back while [`CLIP_WRITE_INTERVAL`] has not
    /// passed since the last write); returns how many streams it goes to.
    /// Silo has a single clipboard: primary selection writes land there too.
    fn clipboard_write(&mut self, text: String) -> usize {
        self.hold_clipboard(text, false)
    }

    /// Hold `text` for the next clipboard write, to remote streams only if
    /// `remote_only`; returns how many streams it goes to.
    fn hold_clipboard(&mut self, text: String, remote_only: bool) -> usize {
        if text.len() > MAX_CLIP_TEXT {
            tracing::debug!(block = %self.id, len = text.len(), "clipboard write too large; dropped");
            return 0;
        }
        let count = self.subscribers.values().filter(|sub| sub.remote || !remote_only).count();
        if count == 0 {
            return 0;
        }
        self.clip_held = Some((text, remote_only));
        self.flush_clipboard();
        count
    }

    fn flush_clipboard(&mut self) {
        if self.clip_held.is_none() || self.clip_sent.is_some_and(|sent| sent.elapsed() < CLIP_WRITE_INTERVAL) {
            return;
        }
        if let Some((text, remote_only)) = self.clip_held.take() {
            self.clip_sent = Some(Instant::now());
            self.broadcast_where(&ServerData::ClipboardWrite { location: ClipboardLocation::Standard, text }, |sub| sub.remote || !remote_only);
        }
    }

    /// Send to every subscriber, cutting off those that fell too far behind.
    /// Never blocks: slow subscribers only grow their own queue until cut.
    fn broadcast(&mut self, data: &ServerData) {
        self.broadcast_where(data, |_| true);
    }

    /// [`Self::broadcast`] to the subscribers `to` selects.
    fn broadcast_where(&mut self, data: &ServerData, to: impl Fn(&Subscriber) -> bool) {
        if self.subscribers.is_empty() {
            return;
        }
        let frame = data.to_frame();
        let ring_start = self.ring_start;
        let mut cut = Vec::new();
        for (id, sub) in self.subscribers.iter_mut().filter(|(_, sub)| to(sub)) {
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
        self.clip_forget(id);
    }

    /// Stream `sub_id` is gone: reads only it still owed answer `None`.
    fn clip_forget(&mut self, sub_id: u64) {
        let done: Vec<u64> = self
            .clip_reads
            .iter_mut()
            .filter_map(|(request, read)| (read.waiting.remove(&sub_id) && read.waiting.is_empty()).then_some(*request))
            .collect();
        for request in done {
            if let Some(read) = self.clip_reads.remove(&request) {
                let _ = read.reply.send(None);
            }
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
            sub = Subscriber::new(attach.sink, from, attach.remote, attach.client_id);
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
            sub = Subscriber::new(attach.sink, end, attach.remote, attach.client_id);
            let history = head.split_off(head_len.min(head.len()));
            sub.send(welcome(StartMode::Snapshot, end, self.size));
            // The snapshot already reflects the current configuration.
            sub.send(ServerData::Configured(self.config).to_frame());
            sub.send(ServerData::SnapshotHead(head).to_frame());
            let anchors = self.command_anchors();
            if !anchors.is_empty() {
                sub.send(ServerData::Commands(anchors).to_frame());
            }
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
        if mode == OwnMode::Take {
            self.driver = Some(client);
        }
        if mode == OwnMode::Take || self.owner.is_none() {
            if self.owner != Some(client) {
                self.owner = Some(client);
                self.publish();
            }
            if let Some(size) = self.desired.get(&client).map(|d| d.size) {
                self.apply_size(size);
            }
        }
        self.reconfigure();
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
            self.reconfigure();
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
            self.update_busy(None, None);
            return;
        };
        let foreground = pty.foreground_pid();
        let pid = pty.pid();
        let foreground_name = foreground.and_then(silo_pty::process_name);
        self.update_busy(foreground, foreground_name.clone());
        let title = if self.osc_title.is_empty() { foreground_name.unwrap_or_else(|| "shell".to_owned()) } else { self.osc_title.clone() };
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
        // The exit rule: running work is over, results stay until seen.
        self.status.records.settle();
        self.status.progress = None;
        self.status.marks = None;
        self.status.busy = None;
        if let Some(index) = self.status.commands.abandon() {
            self.status.anchors.remove(&index);
        }
        self.publish_status();
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

/// `ESC [ I` / `ESC [ O`: a focus report (mode 1004), not typing.
fn is_focus_report(bytes: &[u8]) -> bool {
    matches!(bytes, b"\x1b[I" | b"\x1b[O")
}

/// See `Actor::wanted_config`.
fn wanted_config(configs: &HashMap<ClientId, TerminalConfig>, driver: Option<ClientId>, owner: Option<ClientId>) -> Option<TerminalConfig> {
    [driver, owner]
        .into_iter()
        .flatten()
        .find_map(|c| configs.get(&c))
        .or_else(|| configs.iter().min_by_key(|(id, _)| **id).map(|(_, config)| config))
        .copied()
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
            CursorShape::Block | CursorShape::Unknown => silo_vt::CursorShape::Block,
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
    fn the_driver_s_then_the_owner_s_then_the_first_client_s_config_wins() {
        use std::collections::HashMap;

        use silo_proto::model::TerminalConfig;
        let c = |background| TerminalConfig { background, ..TerminalConfig::default() };
        let configs = HashMap::from([(3, c(3)), (5, c(5)), (8, c(8))]);
        assert_eq!(super::wanted_config(&configs, Some(8), Some(5)), Some(c(8)));
        // A driver or owner without a config (`silo attach`) is skipped.
        assert_eq!(super::wanted_config(&configs, Some(9), Some(5)), Some(c(5)));
        assert_eq!(super::wanted_config(&configs, Some(9), Some(10)), Some(c(3)));
        assert_eq!(super::wanted_config(&configs, None, None), Some(c(3)));
        assert_eq!(super::wanted_config(&HashMap::new(), Some(1), Some(1)), None);
    }

    #[test]
    fn only_exact_focus_reports_are_focus_reports() {
        assert!(super::is_focus_report(b"\x1b[I") && super::is_focus_report(b"\x1b[O"));
        assert!(!super::is_focus_report(b"\x1b[A") && !super::is_focus_report(b"x\x1b[O") && !super::is_focus_report(b"O"));
    }

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
