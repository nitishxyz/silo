//! Terminal block actor: one thread exclusively owns the PTY, the
//! authoritative terminal, the raw output ring, subscribers and size
//! ownership, so attach (snapshot + live output) never races output.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use silo_proto::{
    Frame,
    control::{Capture, DesiredSize, OwnMode, SizeInfo},
    data::{Resume, ServerData, StartMode, Welcome},
    kind,
    model::{BlockId, BlockInfo, ClientId, CursorShape, TermSize, TerminalConfig},
};
use silo_pty::{Pty, PtySize, SpawnConfig};
use silo_vt::{ColorScheme, GridSize, TerminalColors, TerminalCore, TerminalDefaults};
use tokio::sync::mpsc::UnboundedSender;

/// Raw output kept for resume after short disconnects.
const RING_CAP: usize = 4 * 1024 * 1024;
/// A subscriber whose unsent backlog exceeds this is dropped; it reconnects
/// and is re-synchronised from a fresh snapshot (skip-ahead).
const SUBSCRIBER_BACKLOG: usize = 16 * 1024 * 1024;
const HISTORY_CHUNK: usize = 256 * 1024;
const PROBE_INTERVAL: Duration = Duration::from_millis(400);
/// Out-of-band output waits this long for a parser-ground boundary.
const INJECT_WAIT: Duration = Duration::from_millis(250);
/// Configuration transitions remembered inside the ring; older resume points
/// fall back to a snapshot.
const CONFIG_LOG_CAP: usize = 64;

/// Outgoing frames for one data stream; `queued` counts unsent bytes.
#[derive(Clone)]
pub struct Sink {
    pub tx: UnboundedSender<Frame>,
    pub queued: Arc<AtomicUsize>,
}

impl Sink {
    pub fn send(&self, frame: Frame) -> bool {
        self.queued.fetch_add(frame.payload.len() + 5, Ordering::AcqRel);
        self.tx.send(frame).is_ok()
    }

    fn backlog(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
}

pub struct Attach {
    pub sub_id: u64,
    pub client_id: ClientId,
    pub resume: Option<Resume>,
    /// Peer runs the same Ghostty revision and snapshot version.
    pub native: bool,
    pub sink: Sink,
}

pub enum BlockMsg {
    Output(Vec<u8>),
    ReaderClosed,
    ChildExited(i32),
    Input(Vec<u8>),
    Attach(Box<Attach>),
    Detach(u64),
    Desired { client_id: ClientId, client_name: String, size: TermSize },
    /// A client's desired terminal configuration; effective while it owns
    /// the block (or, unowned, when it is the most recent).
    Config { client_id: ClientId, config: TerminalConfig },
    Own { client_id: ClientId, mode: OwnMode },
    Disown { client_id: ClientId },
    ClientGone { client_id: ClientId },
    Capture { history: bool, reply: tokio::sync::oneshot::Sender<Capture> },
    SizeInfo { reply: tokio::sync::oneshot::Sender<SizeInfo> },
    /// Splice `bytes` into the output stream (all clients and the server
    /// terminal see them in the same order) at the next parser-ground
    /// boundary, then write `input` to the program.
    Inject { bytes: Vec<u8>, input: Vec<u8> },
    Shutdown,
}

/// Notifications from block actors to the registry.
#[derive(Debug)]
pub enum Note {
    Changed(BlockId),
    Exited(BlockId),
    Bell(BlockId),
}

pub struct BlockHandle {
    pub tx: mpsc::Sender<BlockMsg>,
    pub info: Arc<Mutex<BlockInfo>>,
}

impl BlockHandle {
    pub fn send(&self, msg: BlockMsg) {
        let _ = self.tx.send(msg);
    }
}

struct Subscriber {
    sink: Sink,
}

struct Desired {
    name: String,
    size: TermSize,
    updated_at: u64,
}

struct Actor {
    id: BlockId,
    pty: Pty,
    input: mpsc::Sender<Vec<u8>>,
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
    /// Where configurations took effect in the output stream.
    config_log: ConfigLog,
}

/// Configuration transitions at their output offsets, so resume and replay
/// switch configuration between the same two bytes the live stream did.
struct ConfigLog {
    /// Effective from `floor` until the first change.
    base: TerminalConfig,
    /// Earliest offset whose configuration is known.
    floor: u64,
    /// `(offset, config)` changes, ascending by offset.
    changes: VecDeque<(u64, TerminalConfig)>,
}

impl ConfigLog {
    fn new(config: TerminalConfig) -> Self {
        Self { base: config, floor: 0, changes: VecDeque::new() }
    }

    fn record(&mut self, offset: u64, config: TerminalConfig) {
        match self.changes.back_mut() {
            Some(last) if last.0 == offset => last.1 = config,
            _ => self.changes.push_back((offset, config)),
        }
        if self.changes.len() > CONFIG_LOG_CAP
            && let Some((offset, config)) = self.changes.pop_front()
        {
            self.base = config;
            self.floor = offset;
        }
    }

    /// Forget changes at or before `start` (the ring's first offset).
    fn trim(&mut self, start: u64) {
        while let Some(&(offset, config)) = self.changes.front() {
            if offset > start {
                break;
            }
            self.base = config;
            self.changes.pop_front();
        }
        self.floor = self.floor.max(start);
    }

    /// The configuration in effect at `offset` and the changes after it.
    fn from(&self, offset: u64) -> (TerminalConfig, Vec<(u64, TerminalConfig)>) {
        let mut config = self.base;
        let mut later = Vec::new();
        for &(at, c) in &self.changes {
            if at <= offset {
                config = c;
            } else {
                later.push((at, c));
            }
        }
        (config, later)
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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
pub fn spawn(
    info: BlockInfo,
    config: &SpawnConfig,
    notes: UnboundedSender<Note>,
) -> anyhow::Result<BlockHandle> {
    let size = TermSize { columns: info.size.columns.clamp(1, 512), rows: info.size.rows.clamp(1, 256), ..info.size };
    let (tx, rx) = mpsc::channel();
    let exit_tx = tx.clone();
    let pty = Pty::spawn(config, pty_size(size), move |status| {
        let code = status.code().unwrap_or(-1);
        let _ = exit_tx.send(BlockMsg::ChildExited(code));
    })?;
    let mut reader = pty.reader()?;
    let read_tx = tx.clone();
    std::thread::Builder::new().name(format!("silo-pty-read-{}", info.id)).spawn(move || {
        use std::io::Read;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if read_tx.send(BlockMsg::Output(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = read_tx.send(BlockMsg::ReaderClosed);
    })?;
    let mut writer = pty.writer()?;
    let (input, input_rx) = mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new().name(format!("silo-pty-write-{}", info.id)).spawn(move || {
        use std::io::Write;
        while let Ok(bytes) = input_rx.recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
        }
    })?;

    let mut info = info;
    info.size = size;
    info.pid = Some(pty.pid());
    let mut vt = TerminalCore::new(grid(size)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let config = TerminalConfig::default();
    install_config(&mut vt, &config);
    let shared = Arc::new(Mutex::new(info.clone()));
    let mut actor = Actor {
        id: info.id.clone(),
        pty,
        input,
        vt,
        epoch: rand::random(),
        ring: VecDeque::new(),
        ring_start: 0,
        subscribers: HashMap::new(),
        size,
        desired: HashMap::new(),
        owner: None,
        candidates: Vec::new(),
        info: shared.clone(),
        notes,
        osc_title: String::new(),
        osc_pwd: None,
        last_probe: Instant::now() - PROBE_INTERVAL,
        exit_code: None,
        reader_closed: false,
        injects: VecDeque::new(),
        inject_since: None,
        configs: HashMap::new(),
        config,
        pending_config: None,
        config_log: ConfigLog::new(config),
    };
    std::thread::Builder::new().name(format!("silo-block-{}", info.id)).spawn(move || actor.run(&rx))?;
    Ok(BlockHandle { tx, info: shared })
}

impl Actor {
    fn run(&mut self, rx: &mpsc::Receiver<BlockMsg>) {
        let mut exit_deadline: Option<Instant> = None;
        loop {
            let mut timeout = exit_deadline.map_or(PROBE_INTERVAL, |d| d.saturating_duration_since(Instant::now()));
            if let Some(since) = self.inject_since.or(self.pending_config.map(|(_, since)| since)) {
                timeout = timeout.min((since + INJECT_WAIT).saturating_duration_since(Instant::now()));
            }
            match rx.recv_timeout(timeout) {
                Ok(BlockMsg::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
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
            if let Some(code) = self.exit_code
                && (self.reader_closed || exit_deadline.is_some_and(|d| Instant::now() >= d)) {
                    self.finish(code);
                    break;
                }
            if self.last_probe.elapsed() >= PROBE_INTERVAL {
                self.probe();
            }
        }
    }

    /// Returns true when the child exited.
    fn handle(&mut self, msg: BlockMsg) -> bool {
        match msg {
            BlockMsg::Output(bytes) => self.output(&bytes),
            BlockMsg::ReaderClosed => self.reader_closed = true,
            BlockMsg::ChildExited(code) => {
                self.exit_code = Some(code);
                return true;
            }
            BlockMsg::Input(bytes) => {
                let _ = self.input.send(bytes);
            }
            BlockMsg::Attach(attach) => self.attach(*attach),
            BlockMsg::Detach(id) => {
                self.subscribers.remove(&id);
            }
            BlockMsg::Desired { client_id, client_name, size } => {
                self.desired.insert(client_id, Desired { name: client_name, size, updated_at: now_ms() });
                if self.owner.is_none() || self.owner == Some(client_id) {
                    self.apply_size(size);
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
            BlockMsg::Inject { bytes, input } => {
                self.injects.push_back((bytes, input));
                self.inject_since.get_or_insert_with(Instant::now);
            }
            BlockMsg::Shutdown => {}
        }
        false
    }

    fn output(&mut self, bytes: &[u8]) {
        let offset = self.ring_start + self.ring.len() as u64;
        match self.vt.write_output(bytes) {
            Ok(replies) if !replies.is_empty() => {
                let _ = self.input.send(replies);
            }
            _ => {}
        }
        self.ring.extend(bytes);
        if self.ring.len() > RING_CAP {
            let excess = self.ring.len() - RING_CAP;
            self.ring.drain(..excess);
            self.ring_start += excess as u64;
            self.config_log.trim(self.ring_start);
        }
        self.broadcast(&ServerData::Output { offset, bytes: bytes.to_vec() });
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
        let Some(since) = self.inject_since else { return };
        if !self.vt.at_ground().unwrap_or(true) && since.elapsed() < INJECT_WAIT {
            return;
        }
        self.inject_since = None;
        while let Some((bytes, input)) = self.injects.pop_front() {
            self.output(&bytes);
            if !input.is_empty() {
                let _ = self.input.send(input);
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
        let Some((config, since)) = self.pending_config else { return };
        if !self.vt.at_ground().unwrap_or(true) && since.elapsed() < INJECT_WAIT {
            return;
        }
        self.pending_config = None;
        if config == self.config {
            return;
        }
        let report = install_config(&mut self.vt, &config);
        if !report.is_empty() {
            // Mode 2031: the program asked to hear about scheme changes.
            let _ = self.input.send(report);
        }
        self.config = config;
        self.config_log.record(self.ring_end(), config);
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

    fn broadcast(&mut self, data: &ServerData) {
        if self.subscribers.is_empty() {
            return;
        }
        let frame = data.to_frame();
        self.subscribers.retain(|_, sub| sub.sink.backlog() <= SUBSCRIBER_BACKLOG && sub.sink.send(frame.clone()));
    }

    fn ring_end(&self) -> u64 {
        self.ring_start + self.ring.len() as u64
    }

    /// Send ring output from `from` to the end, preceded by the configuration
    /// effective at `from` and interleaved with later changes at their offsets.
    fn send_ring_from(&self, sink: &Sink, from: u64) {
        let (config, changes) = self.config_log.from(from);
        sink.send(ServerData::Configured(config).to_frame());
        let mut pos = from;
        for (at, config) in changes {
            self.send_ring(sink, pos, at);
            sink.send(ServerData::Configured(config).to_frame());
            pos = at;
        }
        self.send_ring(sink, pos, self.ring_end());
    }

    fn send_ring(&self, sink: &Sink, from: u64, to: u64) {
        let range = (from - self.ring_start) as usize..(to - self.ring_start) as usize;
        let bytes: Vec<u8> = self.ring.range(range).copied().collect();
        for (i, chunk) in bytes.chunks(256 * 1024).enumerate() {
            let offset = from + (i * 256 * 1024) as u64;
            sink.send(ServerData::Output { offset, bytes: chunk.to_vec() }.to_frame());
        }
    }

    fn attach(&mut self, attach: Attach) {
        let sink = attach.sink;
        let end = self.ring_end();
        let welcome = |mode, offset| {
            Frame::json(kind::WELCOME, &Welcome { epoch: self.epoch, mode, offset, size: self.size }).expect("welcome")
        };
        // Resume and replay need the configuration of every replayed byte;
        // offsets before the known configuration history take a snapshot.
        let start = self.ring_start.max(self.config_log.floor);
        let resumable = attach
            .resume
            .filter(|r| r.epoch == self.epoch && r.offset >= start && r.offset <= end);
        if let Some(resume) = resumable {
            sink.send(welcome(StartMode::Resume, resume.offset));
            self.send_ring_from(&sink, resume.offset);
        } else {
            let snapshot = if attach.native { self.vt.snapshot().ok() } else { None };
            match snapshot.and_then(|s| silo_vt::snapshot::head_len(&s).ok().map(|h| (s, h))) {
                Some((bytes, head)) => {
                    sink.send(welcome(StartMode::Snapshot, end));
                    // The snapshot already reflects the current configuration.
                    sink.send(ServerData::Configured(self.config).to_frame());
                    sink.send(ServerData::SnapshotHead(bytes[..head].to_vec()).to_frame());
                    for chunk in bytes[head..].chunks(HISTORY_CHUNK) {
                        sink.send(ServerData::SnapshotHistory(chunk.to_vec()).to_frame());
                    }
                    sink.send(ServerData::SnapshotDone.to_frame());
                }
                None => {
                    sink.send(welcome(StartMode::Replay, start));
                    self.send_ring_from(&sink, start);
                }
            }
        }
        if let Some(code) = self.exit_code {
            sink.send(ServerData::Exited { code }.to_frame());
        }
        self.subscribers.insert(attach.sub_id, Subscriber { sink });
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
        let _ = self.pty.resize(pty_size(size));
        if self.vt.resize(g).is_err() {
            return;
        }
        self.size = size;
        let offset = self.ring_end();
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
        let foreground = self.pty.foreground_pid();
        let title = if self.osc_title.is_empty() {
            foreground
                .and_then(silo_pty::process_name)
                .unwrap_or_else(|| "shell".to_owned())
        } else {
            self.osc_title.clone()
        };
        let cwd = self
            .osc_pwd
            .as_deref()
            .and_then(pwd_path)
            .or_else(|| silo_pty::process_cwd(foreground.unwrap_or(self.pty.pid())).map(|p| p.to_string_lossy().into_owned()));
        let changed = {
            let Ok(mut info) = self.info.lock() else { return };
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
        if let Ok(mut info) = self.info.lock() {
            info.size = self.size;
            info.owner = self.owner;
        }
        let _ = self.notes.send(Note::Changed(self.id.clone()));
    }

    fn finish(&mut self, code: i32) {
        self.broadcast(&ServerData::Exited { code });
        if let Ok(mut info) = self.info.lock() {
            info.exited = Some(code);
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
        if bytes[i] == b'%' && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&path[i + 1..i + 3], 16) {
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
    fn config_log_places_changes_at_offsets() {
        use silo_proto::model::TerminalConfig;
        let c = |background| TerminalConfig { background, ..TerminalConfig::default() };
        let mut log = super::ConfigLog::new(c(1));
        log.record(10, c(2));
        log.record(20, c(3));
        assert_eq!(log.from(5), (c(1), vec![(10, c(2)), (20, c(3))]));
        assert_eq!(log.from(10), (c(2), vec![(20, c(3))]));
        assert_eq!(log.from(25), (c(3), vec![]));
        log.record(20, c(4));
        assert_eq!(log.from(15), (c(2), vec![(20, c(4))]));
        log.trim(12);
        assert_eq!((log.floor, log.from(12)), (12, (c(2), vec![(20, c(4))])));
        for i in 0..super::CONFIG_LOG_CAP as u32 {
            log.record(100 + u64::from(i), c(10 + i));
        }
        assert_eq!(log.changes.len(), super::CONFIG_LOG_CAP);
        assert_eq!(log.floor, 20, "older resume points need a snapshot");
        assert_eq!(log.from(20).0, c(4));
    }
}
