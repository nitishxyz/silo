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
    model::{BlockId, BlockInfo, ClientId, TermSize},
};
use silo_pty::{Pty, PtySize, SpawnConfig};
use silo_vt::{GridSize, TerminalCore};
use tokio::sync::mpsc::UnboundedSender;

/// Raw output kept for resume after short disconnects.
const RING_CAP: usize = 4 * 1024 * 1024;
/// A subscriber whose unsent backlog exceeds this is dropped; it reconnects
/// and is re-synchronised from a fresh snapshot (skip-ahead).
const SUBSCRIBER_BACKLOG: usize = 16 * 1024 * 1024;
const HISTORY_CHUNK: usize = 256 * 1024;
const PROBE_INTERVAL: Duration = Duration::from_millis(400);

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
    Own { client_id: ClientId, mode: OwnMode },
    Disown { client_id: ClientId },
    ClientGone { client_id: ClientId },
    Capture { history: bool, reply: tokio::sync::oneshot::Sender<Capture> },
    SizeInfo { reply: tokio::sync::oneshot::Sender<SizeInfo> },
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
    let vt = TerminalCore::new(grid(size)).map_err(|e| anyhow::anyhow!("{e}"))?;
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
    };
    std::thread::Builder::new().name(format!("silo-block-{}", info.id)).spawn(move || actor.run(&rx))?;
    Ok(BlockHandle { tx, info: shared })
}

impl Actor {
    fn run(&mut self, rx: &mpsc::Receiver<BlockMsg>) {
        let mut exit_deadline: Option<Instant> = None;
        loop {
            let timeout = exit_deadline.map_or(PROBE_INTERVAL, |d| d.saturating_duration_since(Instant::now()));
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
            BlockMsg::Own { client_id, mode } => self.own(client_id, mode),
            BlockMsg::Disown { client_id } => self.disown(client_id),
            BlockMsg::ClientGone { client_id } => {
                self.desired.remove(&client_id);
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

    fn attach(&mut self, attach: Attach) {
        let sink = attach.sink;
        let end = self.ring_end();
        let welcome = |mode, offset| {
            Frame::json(kind::WELCOME, &Welcome { epoch: self.epoch, mode, offset, size: self.size }).expect("welcome")
        };
        let resumable = attach
            .resume
            .filter(|r| r.epoch == self.epoch && r.offset >= self.ring_start && r.offset <= end);
        if let Some(resume) = resumable {
            sink.send(welcome(StartMode::Resume, resume.offset));
            let skip = (resume.offset - self.ring_start) as usize;
            let missed: Vec<u8> = self.ring.iter().skip(skip).copied().collect();
            for (i, chunk) in missed.chunks(256 * 1024).enumerate() {
                let offset = resume.offset + (i * 256 * 1024) as u64;
                sink.send(ServerData::Output { offset, bytes: chunk.to_vec() }.to_frame());
            }
        } else {
            let snapshot = if attach.native { self.vt.snapshot().ok() } else { None };
            match snapshot.and_then(|s| silo_vt::snapshot::head_len(&s).ok().map(|h| (s, h))) {
                Some((bytes, head)) => {
                    sink.send(welcome(StartMode::Snapshot, end));
                    sink.send(ServerData::SnapshotHead(bytes[..head].to_vec()).to_frame());
                    for chunk in bytes[head..].chunks(HISTORY_CHUNK) {
                        sink.send(ServerData::SnapshotHistory(chunk.to_vec()).to_frame());
                    }
                    sink.send(ServerData::SnapshotDone.to_frame());
                }
                None => {
                    sink.send(welcome(StartMode::Replay, self.ring_start));
                    let all: Vec<u8> = self.ring.iter().copied().collect();
                    for (i, chunk) in all.chunks(256 * 1024).enumerate() {
                        let offset = self.ring_start + (i * 256 * 1024) as u64;
                        sink.send(ServerData::Output { offset, bytes: chunk.to_vec() }.to_frame());
                    }
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
}
