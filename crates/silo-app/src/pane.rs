//! One terminal block: a local mirror restored from the server snapshot and
//! fed the block's output, rendered with the single-canvas painter. Keys,
//! mouse, paste, wheel and selection are encoded locally and sent as input.

use std::{
    cell::RefCell,
    ops::Range,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, ImeTextBatch, IntoElement, Keystroke, Pixels, Point,
    Render, ShapedLine, Task, TextInputPurpose, TextRun, UTF16Selection, Window, canvas, div, fill, point, prelude::*, px, rgb, size,
};
use silo_client::{Applied, Control, DataSender, DataStream, Mirror, Predictor, cache::MirrorCheckpoint, transport::Conn};
use silo_proto::{
    control::{Method, OwnMode},
    data::{ClientData, ClipKind, ClipboardLocation, ServerData},
    model::{BlockInfo, Lifecycle, TermSize},
};
use silo_render::{CellMetrics, CursorPresentation, FrameOptions, FramePreparer, Overlay, Predicted, PreparedFrame, terminal_canvas};
use silo_vt::{Frame, GridSize, KeyEvent, Modifiers, MouseAction, MouseButton, MouseEvent};

use crate::theme::Continuous as _;
use crate::{
    clipboard::{self, Contents, Paste, Source},
    composition::{Composition, InputGate, RawDisposition, byte_range},
    theme,
};

pub const PADDING: f32 = 6.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Live,
    Lost,
}

pub enum PaneEvent {
    /// The user focused or typed in this pane.
    Activated,
    /// "New shell here" on an exited or restored block.
    NewShell,
    /// A program in the block wrote this text to the clipboard.
    ClipboardWrite(String),
    /// A brief message for the status bar.
    Notice(String),
}

/// Files on their way to the block's (remote) host.
struct Uploading {
    files: usize,
    /// Bytes sent and total.
    progress: Arc<(AtomicU64, AtomicU64)>,
    abort: tokio::task::AbortHandle,
    _task: Task<()>,
}

/// Whether a block takes input and size ownership: only while its process
/// runs (unknown blocks are assumed running).
#[must_use]
pub fn takes_input(info: Option<&BlockInfo>) -> bool {
    info.is_none_or(|i| i.lifecycle.is_running())
}

/// The quiet status pill of a block without a process.
#[must_use]
pub fn lifecycle_label(lifecycle: Lifecycle, exited: Option<i32>) -> Option<String> {
    match (lifecycle, exited) {
        (Lifecycle::Running, _) => None,
        (Lifecycle::Exited, Some(code)) => Some(format!("Process exited (code {code})")),
        (Lifecycle::Exited, None) => Some("Process exited".into()),
        (Lifecycle::Restored, _) => Some("Restored · process exited".into()),
    }
}

/// Quiet time after the last grid change before the PTY is resized.
const SIZE_SETTLE: Duration = Duration::from_millis(120);

pub struct Pane {
    pub block_id: String,
    /// The block's host by name; `None` for this Mac.
    pub host: Option<String>,
    rt: tokio::runtime::Handle,
    control: Option<Control>,
    mirror: Mirror,
    /// Predictive local echo over the mirror (docs/PHASE5.md).
    predictor: Predictor,
    /// `SILO_LATENCY_TRACE`: key-to-paint timestamps.
    trace: Option<Rc<RefCell<crate::latency::Trace>>>,
    sender: Option<DataSender>,
    pub status: Status,
    pub info: Option<BlockInfo>,
    /// Bumped whenever the mirror changes; preparation follows it.
    generation: u64,
    /// Stream point and configuration of the last cache checkpoint (or of
    /// the cached mirror it started from).
    saved: Option<(silo_proto::data::Resume, Option<silo_proto::model::TerminalConfig>)>,
    preparer: Arc<Mutex<FramePreparer>>,
    prepared: Option<Arc<PreparedFrame>>,
    /// `Mirror::next_offset` when `prepared`'s frame was captured.
    prepared_offset: u64,
    prepared_key: Option<(u64, u32, u32, u32, u32)>,
    preparing: bool,
    last_frame: Option<Arc<Frame>>,
    content_bounds: Option<Bounds<Pixels>>,
    /// Grid this client wants for the block (from its pixel layout).
    grid: Option<GridSize>,
    /// Sends `grid` once the layout settles, so held resize keys or a live
    /// window resize signal the PTY once.
    size_task: Option<Task<()>>,
    pub focus: FocusHandle,
    composition: Composition,
    input_gate: InputGate,
    ime: Option<ImeLayout>,
    dragging: bool,
    /// Lines per autoscroll tick while a selection drag is outside the grid.
    autoscroll: i32,
    drag_event: Option<MouseEvent>,
    autoscroll_task: Option<Task<()>>,
    blink: crate::blink::Blink,
    /// Whether the last render wanted the blink animation; the timer task
    /// stops itself as soon as this is false.
    blink_wanted: bool,
    /// At most one timer, dropped (cancelled) with the pane.
    blink_task: Option<Task<()>>,
    wheel: f32,
    own_requested: Option<Instant>,
    /// A multi-line paste awaiting confirmation (no bracketed paste).
    pending_paste: Option<String>,
    /// The last ⌘V, Ctrl+V or Ctrl+Shift+V: lets a program read the clipboard.
    paste_gesture: Option<Instant>,
    /// The Mac clipboard for a remote host, sent ahead of the paste gesture's input.
    push: Option<silo_proto::data::Clip>,
    upload: Option<Uploading>,
    /// Focused pane: takes ownership rather than requesting it.
    active: bool,
    attempt: u64,
    stream_task: Option<Task<()>>,
}

impl EventEmitter<PaneEvent> for Pane {}

struct ImeLayout {
    text: String,
    line: ShapedLine,
    origin: Point<Pixels>,
    bounds: Bounds<Pixels>,
    height: Pixels,
}

impl ImeLayout {
    fn range_bounds(&self, bytes: Range<usize>) -> Bounds<Pixels> {
        let x0 = self.origin.x + self.line.x_for_index(bytes.start);
        let x1 = (self.origin.x + self.line.x_for_index(bytes.end)).max(x0 + px(1.0));
        Bounds::new(point(x0, self.origin.y), size(x1 - x0, self.height))
    }
}

impl Pane {
    /// `restored`: a mirror from the cache, shown until the stream resumes.
    pub fn new(
        block_id: String,
        host: Option<String>,
        rt: tokio::runtime::Handle,
        control: Option<Control>,
        restored: Option<Mirror>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        cx.on_focus(&focus, window, |this, _, cx| {
            this.blink.reset();
            this.active = true;
            this.send_focus(true);
            cx.emit(PaneEvent::Activated);
            this.own(OwnMode::Take);
            cx.notify();
        })
        .detach();
        cx.on_blur(&focus, window, |this, window, cx| {
            this.active = false;
            this.send_focus(false);
            this.composition.cancel();
            this.input_gate.clear();
            window.invalidate_character_coordinates();
            cx.notify();
        })
        .detach();
        let mut mirror = restored.unwrap_or_default();
        let cell = CellMetrics::for_window(window).grid(1.0, 1.0);
        mirror.set_cell_size(cell.cell_width, cell.cell_height);
        let mut pane = Self {
            block_id,
            host,
            rt,
            predictor: predictor_for(control.as_ref()),
            trace: crate::latency::Trace::from_env().map(|t| Rc::new(RefCell::new(t))),
            control,
            mirror,
            sender: None,
            status: Status::Connecting,
            info: None,
            generation: 0,
            saved: None,
            preparer: Arc::new(Mutex::new(FramePreparer::new(cx.text_system().clone()))),
            prepared: None,
            prepared_offset: 0,
            prepared_key: None,
            preparing: false,
            last_frame: None,
            content_bounds: None,
            grid: None,
            size_task: None,
            focus,
            composition: Composition::default(),
            input_gate: InputGate::default(),
            ime: None,
            dragging: false,
            autoscroll: 0,
            drag_event: None,
            autoscroll_task: None,
            blink: crate::blink::Blink::default(),
            blink_wanted: false,
            blink_task: None,
            wheel: 0.0,
            own_requested: None,
            pending_paste: None,
            paste_gesture: None,
            push: None,
            upload: None,
            active: false,
            attempt: 0,
            stream_task: None,
        };
        pane.saved = pane.dirty();
        pane.connect(cx);
        pane
    }

    /// A new control connection (after reconnecting to the server).
    pub fn set_control(&mut self, control: Control, cx: &mut Context<Self>) {
        self.predictor = predictor_for(Some(&control));
        self.control = Some(control);
        self.own_requested = None;
        self.connect(cx);
    }

    pub fn set_info(&mut self, info: BlockInfo, cx: &mut Context<Self>) {
        if self.info.as_ref() != Some(&info) {
            self.own_requested = None;
            self.info = Some(info);
            cx.notify();
        }
    }

    pub fn title(&self) -> String {
        self.info.as_ref().map_or_else(|| "shell".into(), block_title)
    }

    /// An iroh connection: the only kind that predicts (docs/PHASE5.md §4.1).
    fn remote(&self) -> bool {
        self.control.as_ref().is_some_and(|c| matches!(c.conn(), Conn::Iroh(_)))
    }

    fn is_owner(&self) -> bool {
        let me = self.control.as_ref().map(Control::client_id);
        self.info.as_ref().is_some_and(|i| i.owner.is_some() && i.owner == me)
    }

    fn running(&self) -> bool {
        takes_input(self.info.as_ref())
    }

    /// The mirror's checkpointable state when it differs from the last
    /// checkpoint (new output or configuration; not scrolling/selection).
    fn dirty(&self) -> Option<(silo_proto::data::Resume, Option<silo_proto::model::TerminalConfig>)> {
        let state = (self.mirror.checkpointable()?, self.mirror.config());
        (self.saved != Some(state)).then_some(state)
    }

    /// Hand over the mirror (leaving its host): shown again from memory on
    /// return, and resumed from where it was.
    pub fn take_mirror(&mut self) -> silo_client::Mirror {
        std::mem::take(&mut self.mirror)
    }

    /// Whether [`checkpoint`](Self::checkpoint) has anything to save.
    pub fn needs_checkpoint(&self) -> bool {
        self.dirty().is_some()
    }

    /// A checkpoint of the mirror if it changed since the last one. Encodes
    /// the snapshot on the calling thread (~10 ms for 100k lines).
    pub fn checkpoint(&mut self) -> Option<MirrorCheckpoint> {
        let state = self.dirty()?;
        let checkpoint = MirrorCheckpoint::capture(&mut self.mirror).ok().flatten()?;
        self.saved = Some(state);
        Some(checkpoint)
    }

    /// Ask for size ownership: `Take` on focus and input, `Request` when
    /// merely visible.
    pub fn own(&mut self, mode: OwnMode) {
        if !self.running() || (mode == OwnMode::Take && self.is_owner()) {
            return;
        }
        if self.own_requested.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return;
        }
        let Some(control) = self.control.clone() else {
            return;
        };
        self.own_requested = Some(Instant::now());
        let block = self.block_id.clone();
        self.rt.spawn(async move {
            let _ = control.call_value(Method::OwnBlock { block_ids: vec![block], mode }).await;
        });
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let Some(control) = self.control.clone() else {
            return;
        };
        self.attempt += 1;
        let attempt = self.attempt;
        self.status = Status::Connecting;
        self.sender = None;
        let resume = self.mirror.resume();
        let block = self.block_id.clone();
        let rt = self.rt.clone();
        self.stream_task = Some(cx.spawn(async move |this, cx| {
            let opened = rt.spawn(async move { DataStream::open(&control, &block, resume).await }).await;
            let mut stream = match opened {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => {
                    tracing::debug!("attach failed: {error:#}");
                    let _ = this.update(cx, |pane, cx| pane.lost(attempt, cx));
                    return;
                }
                Err(_) => return,
            };
            let ok = this.update(cx, |pane, cx| pane.started(&stream, cx)).unwrap_or(false);
            if !ok {
                return;
            }
            tracing::debug!("pane attached");
            while let Some(first) = stream.rx.recv().await {
                let mut batch = vec![first];
                while batch.len() < 512 {
                    match stream.rx.try_recv() {
                        Ok(data) => batch.push(data),
                        Err(_) => break,
                    }
                }
                let keep = this.update(cx, |pane, cx| pane.apply(batch, cx)).unwrap_or(false);
                if !keep {
                    return;
                }
            }
            let _ = this.update(cx, |pane, cx| pane.lost(attempt, cx));
        }));
    }

    fn started(&mut self, stream: &DataStream, cx: &mut Context<Self>) -> bool {
        if let Err(error) = self.mirror.begin(&stream.welcome) {
            tracing::debug!("resync: {error}");
            self.reset_mirror();
            self.connect(cx);
            return false;
        }
        self.predictor.begin(&stream.welcome);
        self.sender = Some(stream.sender());
        self.status = Status::Live;
        self.send_config();
        self.send_size();
        self.own_requested = None;
        self.own(if self.active { OwnMode::Take } else { OwnMode::Request });
        cx.notify();
        true
    }

    fn reset_mirror(&mut self) {
        self.mirror = Mirror::new();
        if let Some(grid) = self.grid {
            self.mirror.set_cell_size(grid.cell_width, grid.cell_height);
        }
    }

    /// Apply server frames; returns false to stop the stream task.
    fn apply(&mut self, batch: Vec<ServerData>, cx: &mut Context<Self>) -> bool {
        let mut dirty = false;
        let shown = self.predictor.overlay();
        // A short batch drained everything received so far.
        let idle = batch.len() < 512;
        for data in batch {
            self.predictor.server(&data);
            match &data {
                ServerData::ClipboardWrite { location: ClipboardLocation::Standard, text } => cx.emit(PaneEvent::ClipboardWrite(text.clone())),
                ServerData::ClipboardRead { request, kind } => self.clipboard_read(*request, *kind, cx),
                _ => {}
            }
            match self.mirror.apply(data) {
                Ok(Applied::Redraw) => dirty = true,
                Ok(Applied::None) => {}
                Ok(Applied::Exited(_)) => dirty = true,
                Err(error) => {
                    tracing::warn!(block = %self.block_id, "re-sync after: {error:#}");
                    self.reset_mirror();
                    self.connect(cx);
                    return false;
                }
            }
        }
        if let Some(sender) = &self.sender {
            sender.ack(&mut self.mirror, idle);
        }
        // A changed mirror is judged once its prepared frame lands.
        if !dirty {
            self.judge_painted();
        }
        if dirty {
            self.generation += 1;
            cx.notify();
        } else if self.predictor.overlay() != shown {
            cx.notify();
        }
        true
    }

    /// Judge predictions against the frame actually painted, so a confirmed
    /// cell leaves the overlay only once the grid shows its glyph.
    fn judge_painted(&mut self) {
        if let (true, Some(prepared)) = (self.remote(), &self.prepared) {
            self.predictor.judge(prepared.frame(), self.prepared_offset);
        }
    }

    fn lost(&mut self, attempt: u64, cx: &mut Context<Self>) {
        if attempt != self.attempt {
            return;
        }
        self.status = Status::Lost;
        self.sender = None;
        cx.notify();
        // Retry while the control connection lives; otherwise the workspace
        // hands over a new one after reconnecting.
        if self.control.as_ref().is_some_and(|c| !c.is_closed()) {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(300)).await;
                let _ = this.update(cx, |pane, cx| {
                    if pane.attempt == attempt && pane.status == Status::Lost {
                        pane.connect(cx);
                    }
                });
            })
            .detach();
        }
    }

    /// `key`: typed input (a key press or committed text), stamped by the latency trace.
    fn send_input(&mut self, bytes: Vec<u8>, key: bool) {
        if bytes.is_empty() || !matches!(self.status, Status::Live) || !self.running() {
            return;
        }
        let Some(sender) = &self.sender else { return };
        send_frames(&self.block_id, sender, clipboard::frames(self.push.take(), Some(bytes.clone())));
        if !self.remote() && self.trace.is_none() {
            return;
        }
        let Some(frame) = self.mirror.terminal().and_then(|vt| vt.frame().ok()) else {
            return;
        };
        let target = self.predictor.overlay().1.unwrap_or((frame.cursor.row, frame.cursor.column));
        self.predictor.input(&bytes, &frame);
        if let Some(trace) = self.trace.as_ref().filter(|_| key) {
            trace.borrow_mut().key(&bytes, target);
        }
    }

    /// The user asked to paste: programs may read the clipboard for a
    /// moment, and a remote host gets it on its system clipboard before the
    /// paste's input.
    fn on_paste_gesture(&mut self, item: Option<&gpui::ClipboardItem>) {
        self.paste_gesture = Some(Instant::now());
        self.push = item.filter(|_| self.remote()).and_then(|item| clipboard::push(Contents::of(item)));
    }

    /// Send a gesture's push that no input carried.
    fn flush_push(&mut self) {
        let Some(push) = self.push.take() else { return };
        if let Some(sender) = self.sender.as_ref().filter(|_| self.status == Status::Live && self.running()) {
            send_frames(&self.block_id, sender, clipboard::frames(Some(push), None));
        }
    }

    fn send_focus(&mut self, focused: bool) {
        if let Some(vt) = self.mirror.terminal()
            && let Ok(bytes) = vt.encode_focus(focused)
        {
            self.send_input(bytes, false);
        }
    }

    /// This client's desired terminal configuration (colors, cursor,
    /// scrollback); the font stays local.
    fn send_config(&self) {
        if let Some(sender) = &self.sender {
            sender.send(&ClientData::Config(theme::terminal_config()));
        }
    }

    fn send_size(&self) {
        if !self.running() {
            return;
        }
        if let (Some(sender), Some(g)) = (&self.sender, self.grid) {
            sender.send(&ClientData::Resize(TermSize { columns: g.columns, rows: g.rows, cell_width: g.cell_width, cell_height: g.cell_height }));
        }
    }

    fn typed(&mut self, cx: &mut Context<Self>) {
        self.blink.reset();
        self.own(OwnMode::Take);
        // The prediction overlay changed with the input just sent.
        cx.notify();
        let Some(vt) = self.mirror.terminal() else {
            return;
        };
        let mut changed = vt.clear_selection();
        if vt.scroll_state().is_ok_and(|s| !s.at_bottom()) {
            vt.scroll_to_row(u64::MAX);
            changed = true;
        }
        if changed {
            self.generation += 1;
        }
    }

    fn accepts_input(&self, window: &Window) -> bool {
        self.status == Status::Live && self.running() && self.focus.is_focused(window) && window.is_window_active()
    }

    // ---- keyboard ----

    fn route_key(&mut self, key: &Keystroke, release: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // Command chords belong to the app (bindings run before this),
        // except the few line-editing ones the terminal handles.
        if key.modifiers.platform
            && !(crate::keys::Chord::of(&key.modifiers) == Some(crate::keys::Chord::Cmd) && crate::keys::terminal_cmd_key(&key.key))
        {
            return false;
        }
        if !self.accepts_input(window) {
            return true;
        }
        if !release && clipboard::is_paste_gesture(&key.key, &key.modifiers) {
            let item = if self.remote() { cx.read_from_clipboard() } else { None };
            self.on_paste_gesture(item.as_ref());
        }
        // Return confirms a pending paste and Escape cancels it; any other key cancels it too.
        if self.pending_paste.is_some() && !release {
            self.confirm_paste(key.key == "enter", cx);
            if matches!(key.key.as_str(), "enter" | "escape") {
                return true;
            }
        }
        let printable =
            key.key_char.as_ref().is_some_and(|t| !t.is_empty()) && !key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform;
        let handled = match self.input_gate.route(&key.key, printable, release, self.composition.is_active()) {
            RawDisposition::Encode => {
                self.encode_key(key, release, cx);
                true
            }
            RawDisposition::PlatformText => false,
            RawDisposition::Ignore => true,
            RawDisposition::Cancel => {
                self.composition.cancel();
                self.ime = None;
                window.invalidate_character_coordinates();
                cx.notify();
                true
            }
        };
        self.flush_push();
        handled
    }

    fn encode_key(&mut self, key: &Keystroke, release: bool, cx: &mut Context<Self>) {
        let text = key.key_char.clone().unwrap_or_else(|| {
            if key.key == "space" {
                " ".into()
            } else if key.key.chars().count() == 1 {
                key.key.clone()
            } else {
                String::new()
            }
        });
        let event = KeyEvent {
            key: key.key.clone(),
            text,
            modifiers: Modifiers {
                shift: key.modifiers.shift,
                control: key.modifiers.control,
                alt: key.modifiers.alt,
                super_key: key.modifiers.platform,
            },
            release,
        };
        let Some(vt) = self.mirror.terminal() else {
            return;
        };
        let encoded = vt.encode_key(&event).map(|bytes| {
            // macOS editing chords whose legacy encoding shells ignore.
            match crate::keys::edit_override(&key.key, crate::keys::Chord::of(&key.modifiers), &bytes) {
                Some(edit) if !release => edit.to_vec(),
                _ => bytes,
            }
        });
        if let Ok(bytes) = encoded
            && !bytes.is_empty()
        {
            self.send_input(bytes, !release);
            if !release {
                self.typed(cx);
            }
        }
    }

    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        let event = KeyEvent { key: String::new(), text: text.to_owned(), modifiers: Modifiers::default(), release: false };
        let Some(vt) = self.mirror.terminal() else {
            return;
        };
        if let Ok(bytes) = vt.encode_key(&event) {
            self.send_input(bytes, true);
            self.typed(cx);
        }
    }

    // ---- clipboard ----

    pub fn copy(&mut self, cx: &mut Context<Self>) {
        let Some(vt) = self.mirror.terminal() else {
            return;
        };
        if let Ok(text) = vt.copy_selection()
            && !text.is_empty()
        {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        if let Some(vt) = self.mirror.terminal()
            && vt.select_all()
        {
            self.generation += 1;
            cx.notify();
        }
    }

    /// Content bounds from the last layout (window coordinates).
    pub fn bounds(&self) -> Option<Bounds<Pixels>> {
        self.content_bounds
    }

    /// A visual-only copy of the last painted frame and its slot (content
    /// bounds grown by the padding), for a close animation.
    pub fn ghost(&self) -> Option<(Arc<PreparedFrame>, Bounds<Pixels>)> {
        let bounds = self.content_bounds?;
        Some((self.prepared.clone()?, bounds.dilate(px(PADDING))))
    }

    /// Report the (changed) terminal preferences to the server. They become
    /// effective for every client while this app owns the block; the
    /// server announces the result, which the mirror then renders.
    pub fn apply_theme(&mut self, cx: &mut Context<Self>) {
        self.send_config();
        self.generation += 1;
        self.prepared_key = None;
        cx.notify();
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let item = cx.read_from_clipboard();
        self.on_paste_gesture(item.as_ref());
        let Some(item) = item else { return };
        match clipboard::choose(Contents::of(&item), self.remote()) {
            Some(Paste::Text(text)) => self.paste_text(text, cx),
            Some(Paste::Paths(paths)) => self.paste_paths(&paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(), cx),
            Some(Paste::SaveImage(png)) => match clipboard::save_png(&silo_proto::paths::home(), &png) {
                Ok(path) => self.paste_paths(&[path.to_string_lossy().into_owned()], cx),
                Err(error) => cx.emit(PaneEvent::Notice(format!("Couldn't save the image: {error}"))),
            },
            Some(Paste::Upload(sources)) => self.upload(sources, cx),
            None => {}
        }
        self.flush_push();
    }

    /// Files dropped from Finder: their paths, uploaded first on a remote host.
    fn drop_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.remote() {
            self.upload(paths.into_iter().map(Source::Path).collect(), cx);
        } else {
            self.paste_paths(&paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(), cx);
        }
    }

    fn paste_paths<S: AsRef<str>>(&mut self, paths: &[S], cx: &mut Context<Self>) {
        if !paths.is_empty() {
            self.paste_text(format!("{} ", clipboard::quoted(paths)), cx);
        }
    }

    fn paste_text(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(vt) = self.mirror.terminal() else {
            return;
        };
        match vt.paste(&text) {
            Ok(Some(bytes)) => {
                self.send_input(bytes, false);
                self.typed(cx);
            }
            Ok(None) => {
                self.pending_paste = Some(text);
                cx.notify();
            }
            Err(error) => tracing::warn!(block = %self.block_id, "{error}"),
        }
    }

    fn upload(&mut self, sources: Vec<Source>, cx: &mut Context<Self>) {
        if self.upload.is_some() {
            cx.emit(PaneEvent::Notice("Upload in progress".into()));
            return;
        }
        let Some(control) = self.control.clone().filter(|_| self.status == Status::Live && self.running()) else {
            return;
        };
        let files: Vec<silo_client::Upload> = sources
            .into_iter()
            .map(|source| match source {
                Source::Path(path) => silo_client::Upload {
                    name: path.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned()),
                    source: silo_client::UploadSource::Path(path),
                },
                Source::Png(bytes) => silo_client::Upload { name: "clipboard.png".into(), source: silo_client::UploadSource::Bytes(bytes) },
            })
            .collect();
        let count = files.len();
        let progress = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
        let report = progress.clone();
        let block = self.block_id.clone();
        let handle = self.rt.spawn(async move {
            silo_client::upload(&control, &block, files, move |sent, total| {
                report.0.store(sent, Ordering::Relaxed);
                report.1.store(total, Ordering::Relaxed);
            })
            .await
        });
        let abort = handle.abort_handle();
        let task = cx.spawn(async move |this, cx| {
            while !handle.is_finished() {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
            let result = handle.await;
            let _ = this.update(cx, |pane, cx| {
                pane.upload = None;
                cx.notify();
                match result {
                    Ok(Ok(paths)) => pane.paste_paths(&paths, cx),
                    Ok(Err(error)) => cx.emit(PaneEvent::Notice(format!("Upload failed: {error:#}"))),
                    Err(_) => {}
                }
            });
        });
        self.upload = Some(Uploading { files: count, progress, abort, _task: task });
        cx.notify();
    }

    fn cancel_upload(&mut self, cx: &mut Context<Self>) {
        if let Some(upload) = self.upload.take() {
            upload.abort.abort();
            cx.notify();
        }
    }

    /// A program in the block asks for the clipboard: answered only right
    /// after the user pasted into this focused block, never prompting. One
    /// paste answers one read.
    fn clipboard_read(&mut self, request: u64, kind: ClipKind, cx: &mut Context<Self>) {
        let Some(sender) = self.sender.clone() else { return };
        let gesture = self.paste_gesture.take_if(|at| clipboard::read_allowed(self.active, Some(*at), Instant::now()));
        let contents = gesture.and_then(|_| cx.read_from_clipboard()).map(|item| Contents::of(&item));
        cx.spawn(async move |this, cx| {
            let answered = cx
                .background_spawn(async move {
                    let content = contents.and_then(|contents| clipboard::answer(contents, kind));
                    let answered = content.is_some();
                    sender.send(&ClientData::ClipboardContent { request, content });
                    answered
                })
                .await;
            if !answered && gesture.is_some() {
                let _ = this.update(cx, |pane, _| {
                    pane.paste_gesture = pane.paste_gesture.or(gesture);
                });
            }
        })
        .detach();
    }

    fn upload_card(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let upload = self.upload.as_ref()?;
        let label = clipboard::upload_label(upload.files, upload.progress.0.load(Ordering::Relaxed), upload.progress.1.load(Ordering::Relaxed));
        Some(
            div()
                .id("upload-progress")
                .occlude()
                .absolute()
                .bottom(px(8.0))
                .right(px(8.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .px(px(12.0))
                .py(px(8.0))
                .continuous(10.0)
                .bg(rgb(theme::popover()))
                .border_1()
                .border_color(rgb(theme::border()))
                .shadow_xl()
                .text_size(px(12.0))
                .child(label)
                .child(
                    div()
                        .id("upload-cancel")
                        .text_color(rgb(theme::muted()))
                        .cursor_pointer()
                        .hover(|s| s.text_color(rgb(theme::foreground())))
                        .child("Cancel")
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_upload(cx))),
                ),
        )
    }

    fn confirm_paste(&mut self, paste: bool, cx: &mut Context<Self>) {
        let Some(text) = self.pending_paste.take() else { return };
        cx.notify();
        let Some(vt) = self.mirror.terminal().filter(|_| paste) else { return };
        match vt.paste_confirmed(&text) {
            Ok(bytes) => {
                self.send_input(bytes, false);
                self.typed(cx);
            }
            Err(error) => tracing::warn!(block = %self.block_id, "{error}"),
        }
    }

    fn paste_card(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let lines = self.pending_paste.as_ref()?.lines().count();
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .h(px(24.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .continuous(12.0)
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .cursor_pointer()
                .when(primary, |el| el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())).hover(|s| s.opacity(0.88)))
                .when(!primary, |el| el.bg(rgb(theme::tab_active())).hover(|s| s.bg(rgb(theme::row_active()))))
                .child(label)
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| this.confirm_paste(primary, cx)))
        };
        Some(
            div()
                .id("paste-confirm")
                .occlude()
                .absolute()
                .bottom(px(8.0))
                .right(px(8.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .p(px(12.0))
                .continuous(10.0)
                .bg(rgb(theme::popover()))
                .border_1()
                .border_color(rgb(theme::border()))
                .shadow_xl()
                .text_size(px(13.0))
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(format!("Paste {lines} line{}?", if lines == 1 { "" } else { "s" })))
                .child(div().text_size(px(12.0)).text_color(rgb(theme::muted())).child("It may run as commands."))
                .child(div().flex().justify_end().gap(px(8.0)).child(button("paste-cancel", "Cancel", false)).child(button(
                    "paste-confirm-button",
                    "Paste",
                    true,
                ))),
        )
    }

    // ---- pointer ----

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn pointer(
        &mut self,
        position: Point<Pixels>,
        action: MouseAction,
        button: MouseButton,
        modifiers: gpui::Modifiers,
        scale: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.content_bounds else {
            return;
        };
        let scale = scale.max(0.5);
        let exited = !self.running();
        let Some(vt) = self.mirror.terminal() else {
            return;
        };
        let grid = vt.size();
        let max_x = (f32::from(grid.columns) * f32::from(grid.cell_width) - 1.0).max(0.0);
        let max_y = (f32::from(grid.rows) * f32::from(grid.cell_height) - 1.0).max(0.0);
        let mut x = f32::from(position.x - bounds.left()) * scale;
        let mut y = f32::from(position.y - bounds.top()) * scale;
        // Selection autoscroll while dragging above or below the grid.
        if self.dragging && matches!(action, MouseAction::Motion) {
            let cell = f32::from(grid.cell_height).max(1.0);
            self.autoscroll = if y < 0.0 {
                -((-y / cell).ceil() as i32).clamp(1, 8)
            } else if y > max_y {
                (((y - max_y) / cell).ceil() as i32).clamp(1, 8)
            } else {
                0
            };
        }
        if self.dragging || matches!(action, MouseAction::Release) {
            x = x.clamp(0.0, max_x);
            y = y.clamp(0.0, max_y);
        }
        if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 || x > max_x || y > max_y {
            return;
        }
        let mut event = MouseEvent {
            action,
            button,
            x: x as u16,
            y: y as u16,
            modifiers: Modifiers { shift: modifiers.shift, control: modifiers.control, alt: modifiers.alt, super_key: modifiers.platform },
        };
        // A block without a process has no program to report to: always select.
        if exited {
            event.modifiers.shift = true;
        }
        tracing::trace!(?action, x = event.x, y = event.y, "pointer");
        if matches!(action, MouseAction::Motion) && self.dragging {
            self.drag_event = Some(event);
        }
        if let Ok(bytes) = vt.encode_mouse_at(event, Instant::now())
            && !exited
        {
            self.send_input(bytes, false);
        }
        if matches!(action, MouseAction::Release) {
            self.autoscroll = 0;
            self.drag_event = None;
        }
        if self.autoscroll != 0 {
            self.start_autoscroll(cx);
        }
        self.generation += 1;
        cx.notify();
    }

    /// While a selection drag is outside the grid, scroll the viewport and
    /// re-apply the last drag position every 50 ms.
    fn start_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.autoscroll_task.is_some() {
            return;
        }
        self.autoscroll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(50)).await;
                let keep = this
                    .update(cx, |pane, cx| {
                        if !pane.dragging || pane.autoscroll == 0 {
                            pane.autoscroll_task = None;
                            return false;
                        }
                        let lines = pane.autoscroll;
                        let event = pane.drag_event;
                        if let Some(vt) = pane.mirror.terminal()
                            && vt.drag_scroll(lines)
                            && let Some(event) = event
                        {
                            let _ = vt.encode_mouse_at(event, Instant::now());
                        }
                        pane.generation += 1;
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    return;
                }
            }
        }));
    }

    fn end_drag(&mut self) {
        self.dragging = false;
        self.autoscroll = 0;
        self.drag_event = None;
    }

    fn scroll_wheel(&mut self, event: &gpui::ScrollWheelEvent, scale: f32, cx: &mut Context<Self>) {
        let cell_height = self.grid.map_or(16.0, |g| f32::from(g.cell_height) / scale.max(0.5));
        let delta = match event.delta {
            gpui::ScrollDelta::Pixels(p) => f32::from(p.y) / cell_height.max(1.0),
            gpui::ScrollDelta::Lines(l) => l.y,
        };
        self.wheel += delta;
        #[allow(clippy::cast_possible_truncation)]
        let lines = self.wheel.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.wheel -= lines as f32;
        let tracking = self.last_frame.as_ref().is_some_and(|f| f.mouse_tracking);
        if tracking && self.status == Status::Live && self.running() {
            let button = if lines > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
            for _ in 0..lines.unsigned_abs().min(10) {
                self.pointer(event.position, MouseAction::Press, button, event.modifiers, scale, cx);
            }
        } else if let Some(vt) = self.mirror.terminal() {
            vt.scroll(-lines);
            self.generation += 1;
            cx.notify();
        }
    }

    // ---- layout & paint ----

    fn layout(&mut self, bounds: Bounds<Pixels>, metrics: CellMetrics, cx: &mut Context<Self>) {
        self.content_bounds = Some(bounds);
        let grid = metrics.grid(f32::from(bounds.size.width), f32::from(bounds.size.height));
        if self.grid != Some(grid) {
            tracing::debug!(block = %self.block_id, columns = grid.columns, rows = grid.rows, "pane grid");
            let first = self.grid.replace(grid).is_none();
            self.mirror.set_cell_size(grid.cell_width, grid.cell_height);
            if first {
                self.send_size();
            } else {
                self.size_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SIZE_SETTLE).await;
                    let _ = this.update(cx, |this, _| this.send_size());
                }));
            }
        }
    }

    fn request_prepare(&mut self, metrics: CellMetrics, cx: &mut Context<Self>) {
        let key = (self.generation, metrics.width.to_bits(), metrics.height.to_bits(), metrics.scale.to_bits(), metrics.font);
        if self.preparing || self.prepared_key == Some(key) {
            return;
        }
        let offset = self.mirror.next_offset();
        let Some(vt) = self.mirror.terminal() else {
            return;
        };
        let Ok(frame) = vt.frame() else { return };
        let frame = Arc::new(frame);
        self.last_frame = Some(frame.clone());
        self.preparing = true;
        let preparer = self.preparer.clone();
        cx.spawn(async move |this, cx| {
            let prepared = cx
                .background_spawn(async move {
                    let mut preparer = preparer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    preparer.prepare(&frame, metrics, FrameOptions::default())
                })
                .await;
            let _ = this.update(cx, |pane, cx| {
                pane.preparing = false;
                pane.prepared = Some(Arc::new(prepared));
                pane.prepared_offset = offset;
                pane.prepared_key = Some(key);
                pane.judge_painted();
                cx.notify();
            });
        })
        .detach();
    }

    /// Cursor phase for this render, starting or stopping the blink timer
    /// as needed. The painted frame's cursor decides: DECTCEM visibility,
    /// DECSCUSR steady/blinking (and the terminal settings' blink
    /// default), and whether the viewport shows the cursor at all.
    fn blink_phase(&mut self, focused: bool, window_active: bool, cx: &mut Context<Self>) -> bool {
        let frame = self.prepared.as_ref().map(|p| p.frame().clone());
        let Some(frame) = frame else {
            self.blink_wanted = false;
            return true;
        };
        let c = frame.cursor;
        self.blink.track(c.column, c.row);
        let live = self.status == Status::Live && self.running();
        self.blink_wanted = crate::blink::active(focused && live, window_active, c.visible, c.blinking, frame.scroll.at_bottom());
        if !self.blink_wanted {
            return true;
        }
        if self.blink_task.is_none() {
            tracing::debug!(block = %self.block_id, "blink timer start");
            self.blink_task = Some(cx.spawn(async move |this, cx| {
                // Each step decides and clears the slot in one update, so a
                // render never sees a stale task and skips starting a new one.
                let next = |pane: &mut Pane| {
                    if pane.blink_wanted {
                        Some(pane.blink.until_next(Instant::now()))
                    } else {
                        tracing::debug!(block = %pane.block_id, "blink timer stop");
                        pane.blink_task = None;
                        None
                    }
                };
                let mut wait = match this.update(cx, |pane, _| next(pane)) {
                    Ok(Some(wait)) => wait,
                    _ => return,
                };
                loop {
                    cx.background_executor().timer(wait).await;
                    wait = match this.update(cx, |pane, cx| {
                        let wait = next(pane);
                        if wait.is_some() {
                            cx.notify();
                        }
                        wait
                    }) {
                        Ok(Some(wait)) => wait,
                        _ => return,
                    };
                }
            }));
        }
        self.blink.visible_at(Instant::now())
    }

    fn status_text(&self) -> Option<String> {
        // Offline: the workspace shows one indicator for all panes.
        if self.control.as_ref().is_none_or(Control::is_closed) {
            return None;
        }
        match &self.status {
            Status::Live => None,
            Status::Connecting => Some("attaching…".into()),
            Status::Lost => Some("reconnecting…".into()),
        }
    }

    fn lifecycle_text(&self) -> Option<String> {
        self.info.as_ref().and_then(|i| lifecycle_label(i.lifecycle, i.exited))
    }

    /// The server applies another client's size to this block.
    fn sized_elsewhere(&self) -> bool {
        let applied = self.mirror.size();
        self.status == Status::Live
            && self.running()
            && self.grid.is_some_and(|g| g.columns != applied.columns || g.rows != applied.rows)
            && self.info.as_ref().is_some_and(|i| i.owner.is_some())
            && !self.is_owner()
    }
}

fn send_frames(block: &str, sender: &DataSender, frames: Vec<ClientData>) {
    for frame in frames {
        if let ClientData::ClipboardPush { content } = &frame {
            let (kind, bytes) = match content {
                silo_proto::data::Clip::Text(text) => ("text", text.len()),
                silo_proto::data::Clip::Image { data, .. } => ("image", data.len()),
                silo_proto::data::Clip::Types(_) => ("types", 0),
            };
            tracing::debug!(block, kind, bytes, "clipboard push");
        }
        sender.send(&frame);
    }
}

/// Remote (iroh) connections predict, seeded with the path RTT; local ones never do.
fn predictor_for(control: Option<&Control>) -> Predictor {
    match control.map(Control::conn) {
        Some(Conn::Iroh(conn)) => Predictor::remote(conn.rtt()),
        _ => Predictor::local(),
    }
}

/// Tab title: a program's title or foreground command, else the
/// cwd's last component (`~` for home, `/` for root) while a shell sits at
/// its prompt.
pub fn block_title(info: &BlockInfo) -> String {
    let shell = info.title.is_empty() || matches!(info.title.trim_start_matches('-'), "zsh" | "bash" | "fish" | "sh" | "nu" | "shell" | "login");
    match info.cwd.as_deref() {
        Some(cwd) if shell => {
            let home = std::env::var("HOME").unwrap_or_default();
            let cwd = cwd.trim_end_matches('/');
            if !home.is_empty() && cwd == home.trim_end_matches('/') {
                return "~".into();
            }
            cwd.rsplit('/').next().filter(|last| !last.is_empty()).unwrap_or("/").to_owned()
        }
        _ => info.title.clone(),
    }
}

fn mouse_button(button: Option<gpui::MouseButton>) -> MouseButton {
    match button {
        Some(gpui::MouseButton::Left) => MouseButton::Left,
        Some(gpui::MouseButton::Right) => MouseButton::Right,
        Some(gpui::MouseButton::Middle) => MouseButton::Middle,
        _ => MouseButton::None,
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metrics = CellMetrics::for_window(window);
        self.request_prepare(metrics, cx);
        let focused = self.focus.is_focused(window) && window.is_window_active();
        let phase_visible = self.blink_phase(focused, window.is_window_active(), cx);
        let cursor = CursorPresentation { focused, phase_visible };
        let selection = rgb(theme::selection()).opacity(0.55);
        let status = self.status_text();
        let lifecycle = self.lifecycle_text();
        let sized_elsewhere = self.sized_elsewhere();
        // Another client's smaller grid: rule off and fill what is not ours.
        let applied = self.mirror.size();
        let edge_x = self.grid.filter(|g| sized_elsewhere && applied.columns < g.columns).map(|_| px(f32::from(applied.columns) * metrics.width));
        let edge_y = self.grid.filter(|g| sized_elsewhere && applied.rows < g.rows).map(|_| px(f32::from(applied.rows) * metrics.height));
        let outside = || div().absolute().bg(rgb(theme::surface())).border_color(rgb(theme::border()));
        let (cells, predicted_cursor) = self.predictor.overlay();
        let overlay = Overlay {
            predicted: cells.into_iter().map(|c| Predicted { row: c.row, column: c.column, text: c.text }).collect(),
            predicted_cursor,
            predicted_underline: rgb(theme::muted()),
            predicted_foreground: rgb(self.mirror.config().map_or_else(theme::foreground, |c| c.foreground)),
            ..Overlay::default()
        };
        let trace = self.trace.clone().zip(self.prepared.clone()).map(|(t, p)| (t, p, overlay.predicted.clone()));
        let entity = cx.entity();
        let mut element = div()
            .id(gpui::ElementId::Name(format!("pane-{}", self.block_id).into()))
            .key_context("Terminal")
            .track_focus(&self.focus)
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            // No fill: the workspace's pane card paints the background, so
            // its rounded corners (Comfortable) stay visible.
            .p(px(PADDING))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if this.route_key(&event.keystroke, false, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .on_key_up(cx.listener(|this, event: &gpui::KeyUpEvent, window, cx| {
                if this.route_key(&event.keystroke, true, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, window, cx| {
                    if this.dragging {
                        this.pointer(event.position, MouseAction::Release, MouseButton::Left, event.modifiers, window.scale_factor(), cx);
                        this.end_drag();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                if this.dragging && event.pressed_button != Some(gpui::MouseButton::Left) {
                    this.end_drag();
                }
                let tracking = this.last_frame.as_ref().is_some_and(|f| f.mouse_tracking);
                if this.dragging || tracking {
                    this.pointer(event.position, MouseAction::Motion, mouse_button(event.pressed_button), event.modifiers, window.scale_factor(), cx);
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, window, cx| {
                this.scroll_wheel(event, window.scale_factor(), cx);
            }));
        for button in [gpui::MouseButton::Left, gpui::MouseButton::Right, gpui::MouseButton::Middle] {
            element = element
                .on_mouse_down(
                    button,
                    cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                        this.focus.focus(window, cx);
                        if event.button == gpui::MouseButton::Left {
                            this.dragging = true;
                        }
                        this.pointer(
                            event.position,
                            MouseAction::Press,
                            mouse_button(Some(event.button)),
                            event.modifiers,
                            window.scale_factor(),
                            cx,
                        );
                    }),
                )
                .on_mouse_up(
                    button,
                    cx.listener(|this, event: &gpui::MouseUpEvent, window, cx| {
                        this.pointer(
                            event.position,
                            MouseAction::Release,
                            mouse_button(Some(event.button)),
                            event.modifiers,
                            window.scale_factor(),
                            cx,
                        );
                        if event.button == gpui::MouseButton::Left {
                            this.end_drag();
                        }
                    }),
                );
        }
        element.child(
            div()
                .relative()
                .size_full()
                .when_some(self.prepared.clone(), |el, prepared| {
                    el.child(div().absolute().top_0().left_0().size_full().child(terminal_canvas(prepared, metrics, cursor, selection, overlay)))
                })
                .when_some(edge_x, |el, x| {
                    el.child(outside().top_0().right_0().left(x).border_l_1().map(|d| match edge_y {
                        Some(y) => d.h(y),
                        None => d.bottom_0(),
                    }))
                })
                .when_some(edge_y, |el, y| {
                    el.child(outside().left_0().bottom_0().top(y).border_t_1().map(|d| match edge_x {
                        Some(x) => d.w(x),
                        None => d.right_0(),
                    }))
                })
                .when_some(edge_x.zip(edge_y), |el, (x, y)| el.child(outside().left(x).top(y).right_0().bottom_0()))
                .when_some(trace, |el, (trace, prepared, predicted)| el.child(crate::latency::paint_probe(trace, prepared, predicted)))
                .child(input_canvas(entity, metrics))
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .continuous(6.0)
                        .drag_over::<gpui::ExternalPaths>(|s, _, _, _| {
                            s.bg(rgb(theme::accent()).opacity(0.08)).border_2().border_color(rgb(theme::accent()))
                        })
                        .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                            this.focus.focus(window, cx);
                            this.drop_paths(paths.paths().to_vec(), cx);
                        })),
                )
                .when_some(status.filter(|_| lifecycle.is_none()), |el, text| {
                    el.child(
                        div()
                            .absolute()
                            .bottom(px(4.0))
                            .right(px(4.0))
                            .px(px(8.0))
                            .py(px(3.0))
                            .continuous(4.0)
                            .bg(rgb(theme::tab_active()))
                            .text_color(rgb(theme::muted()))
                            .text_size(px(11.0))
                            .child(text),
                    )
                })
                .when_some(lifecycle, |el, text| {
                    el.child(
                        div()
                            .absolute()
                            .bottom(px(4.0))
                            .right(px(4.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .px(px(8.0))
                            .py(px(3.0))
                            .continuous(4.0)
                            .bg(rgb(theme::tab_active()))
                            .border_1()
                            .border_color(rgb(theme::border()))
                            .text_color(rgb(theme::foreground()).opacity(0.75))
                            .text_size(px(11.0))
                            .child(text)
                            .child(
                                div()
                                    .id("new-shell")
                                    .text_color(rgb(theme::foreground()))
                                    .cursor_pointer()
                                    .child("New shell here")
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::NewShell))),
                            ),
                    )
                })
                .when(sized_elsewhere, |el| {
                    let owner = self.info.as_ref().and_then(|i| i.owner_name.clone()).unwrap_or_else(|| "another client".into());
                    el.child(
                        div()
                            .absolute()
                            .top(px(2.0))
                            .right(px(2.0))
                            .flex()
                            .gap(px(6.0))
                            .px(px(6.0))
                            .py(px(2.0))
                            .continuous(4.0)
                            .bg(rgb(theme::tab_active()))
                            .text_color(rgb(theme::muted()))
                            .text_size(px(11.0))
                            .child(format!("sized by {owner}"))
                            .child("·")
                            .child(
                                div()
                                    .id("take-size")
                                    .text_color(rgb(theme::foreground()))
                                    .cursor_pointer()
                                    .child("Take size")
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                    .on_click(cx.listener(|this, _, _, _| {
                                        this.own_requested = None;
                                        this.own(OwnMode::Take);
                                    })),
                            ),
                    )
                })
                .children(self.paste_card(cx))
                .children(self.upload_card(cx)),
        )
    }
}

/// Reports the content bounds, hosts IME composition and registers the
/// platform input handler.
fn input_canvas(entity: Entity<Pane>, metrics: CellMetrics) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            let pane = entity.read(cx);
            let text = pane.composition.text().to_owned();
            let cursor = pane.last_frame.as_ref().map(|f| f.cursor).unwrap_or_default();
            let run = TextRun {
                len: text.len(),
                font: silo_render::terminal_font(),
                color: rgb(theme::foreground()).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window.text_system().shape_line(text.clone().into(), px(metrics.font_size), &[run], None);
            let height = px(metrics.height).min(bounds.size.height);
            let x = (bounds.left() + px(f32::from(cursor.column) * metrics.width))
                .min((bounds.right() - line.width().min(bounds.size.width)).max(bounds.left()));
            let y = (bounds.top() + px(f32::from(cursor.row) * metrics.height)).min((bounds.bottom() - height).max(bounds.top()));
            (entity, ImeLayout { text, line, origin: point(x, y), bounds, height })
        },
        move |bounds, (entity, layout), window, cx| {
            entity.update(cx, |pane, cx| pane.layout(bounds, metrics, cx));
            capture_drag(&entity, bounds, window);
            if !layout.text.is_empty() {
                window.paint_quad(fill(Bounds::new(layout.origin, size(layout.line.width(), layout.height)), rgb(theme::tab_active())));
                let _ = layout.line.paint(layout.origin, layout.height, gpui::TextAlign::Left, None, window, cx);
                window.paint_quad(fill(
                    Bounds::new(point(layout.origin.x, layout.origin.y + layout.height - px(1.0)), size(layout.line.width().max(px(1.0)), px(1.0))),
                    rgb(theme::foreground()),
                ));
            }
            let changed = entity.read(cx).ime.as_ref().is_none_or(|old| old.text != layout.text || old.origin != layout.origin);
            let focus = entity.read(cx).focus.clone();
            entity.update(cx, |pane, _| pane.ime = Some(layout));
            if changed {
                window.invalidate_character_coordinates();
            }
            window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Keep a selection drag going when the pointer leaves the pane: GPUI only
/// delivers moves over the element, so listen window-wide while dragging.
fn capture_drag(entity: &Entity<Pane>, bounds: Bounds<Pixels>, window: &mut Window) {
    let entity = entity.downgrade();
    window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, window, cx| {
        if phase != gpui::DispatchPhase::Bubble || bounds.contains(&event.position) {
            return;
        }
        let _ = entity.update(cx, |pane, cx| {
            if !pane.dragging {
                return;
            }
            if event.pressed_button != Some(gpui::MouseButton::Left) {
                pane.end_drag();
                return;
            }
            pane.pointer(event.position, MouseAction::Motion, MouseButton::Left, event.modifiers, window.scale_factor(), cx);
        });
    });
}

impl Pane {
    fn composition_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ime = None;
        window.invalidate_character_coordinates();
        cx.notify();
    }
}

impl EntityInputHandler for Pane {
    fn accepts_text_input(&self, window: &mut Window, _: &mut Context<Self>) -> bool {
        self.accepts_input(window)
    }

    fn text_input_purpose(&self, _: &mut Window, _: &mut Context<Self>) -> TextInputPurpose {
        TextInputPurpose::Terminal
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        if !self.accepts_input(window) {
            return None;
        }
        let text = self.composition.text_range(range.clone())?.to_owned();
        *adjusted = Some(range);
        Some(text)
    }

    fn selected_text_range(&mut self, _: bool, window: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        self.accepts_input(window).then(|| UTF16Selection { range: self.composition.selected(), reversed: false })
    }

    fn marked_text_range(&self, window: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (self.accepts_input(window) && self.composition.is_active()).then(|| 0..self.composition.len_utf16())
    }

    fn text_length_utf16(&mut self, window: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        self.accepts_input(window).then(|| self.composition.len_utf16())
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composition.cancel();
        self.composition_changed(window, cx);
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.accepts_input(window) {
            self.unmark_text(window, cx);
            return;
        }
        if text.is_empty()
            && let Some(range) = range.clone()
        {
            if self.composition.delete(range).is_err() {
                self.composition.cancel();
            }
            self.composition_changed(window, cx);
            return;
        }
        match self.composition.commit(range, text) {
            Ok(text) => self.commit_text(&text, cx),
            Err(_) => self.composition.cancel(),
        }
        self.composition_changed(window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.accepts_input(window) {
            self.unmark_text(window, cx);
            return;
        }
        if self.composition.mark(range, text, selected).is_err() {
            self.composition.cancel();
        }
        self.composition_changed(window, cx);
    }

    fn apply_ime_text_batch(&mut self, batch: ImeTextBatch, window: &mut Window, cx: &mut Context<Self>) {
        if !self.accepts_input(window) {
            self.unmark_text(window, cx);
            return;
        }
        match self.composition.batch(batch.delete_utf16, batch.commit.as_deref(), batch.preedit.as_deref()) {
            Ok(Some(text)) => self.commit_text(&text, cx),
            Ok(None) => {}
            Err(_) => self.composition.cancel(),
        }
        self.composition_changed(window, cx);
    }

    fn set_selected_text_range(&mut self, range: Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.accepts_input(window) && self.composition.select(range).is_ok() {
            self.composition_changed(window, cx);
        }
    }

    fn bounds_for_range(&mut self, range: Range<usize>, _: Bounds<Pixels>, window: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        if !self.accepts_input(window) {
            return None;
        }
        let layout = self.ime.as_ref()?;
        if layout.text != self.composition.text() {
            return None;
        }
        Some(layout.range_bounds(byte_range(&layout.text, range)?))
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, window: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        if !self.accepts_input(window) {
            return None;
        }
        let layout = self.ime.as_ref()?;
        if layout.text != self.composition.text() || !layout.bounds.contains(&point) {
            return None;
        }
        let byte = layout.line.closest_index_for_x(point.x - layout.origin.x);
        Some(layout.text.get(..byte)?.encode_utf16().count())
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::model::{BlockKind, Lifecycle, TermSize};

    use super::*;

    fn info(lifecycle: Lifecycle, exited: Option<i32>) -> BlockInfo {
        BlockInfo {
            id: "b".into(),
            session_id: "s".into(),
            kind: BlockKind::Terminal,
            title: String::new(),
            cwd: None,
            exited,
            lifecycle,
            size: TermSize::new(80, 24),
            owner: None,
            owner_name: None,
            pid: None,
            browser: None,
        }
    }

    #[test]
    fn block_title_shows_last_cwd_component() {
        let shell = |title: &str, cwd: &str| BlockInfo { title: title.into(), cwd: Some(cwd.into()), ..info(Lifecycle::Running, None) };
        let home = std::env::var("HOME").unwrap();
        assert_eq!(block_title(&shell("-zsh", &home)), "~");
        assert_eq!(block_title(&shell("zsh", &format!("{home}/dev/term/crates"))), "crates");
        assert_eq!(block_title(&shell("", "/")), "/");
        assert_eq!(block_title(&shell("", "/usr/local/")), "local");
        assert_eq!(block_title(&shell("vim notes.md", "/tmp")), "vim notes.md");
    }

    #[test]
    fn badge_names_the_owner_briefly() {
        use silo_proto::model::short_client_name;
        assert_eq!(short_client_name("Silo 0.1.0 (macOS, pid 42)"), "Silo");
        assert_eq!(short_client_name("silo cli"), "silo cli");
    }

    #[test]
    fn only_running_blocks_take_input() {
        assert!(takes_input(None));
        assert!(takes_input(Some(&info(Lifecycle::Running, None))));
        assert!(!takes_input(Some(&info(Lifecycle::Exited, Some(0)))));
        assert!(!takes_input(Some(&info(Lifecycle::Restored, None))));
    }

    #[test]
    fn lifecycle_labels() {
        assert_eq!(lifecycle_label(Lifecycle::Running, None), None);
        assert_eq!(lifecycle_label(Lifecycle::Exited, Some(3)).as_deref(), Some("Process exited (code 3)"));
        assert_eq!(lifecycle_label(Lifecycle::Exited, None).as_deref(), Some("Process exited"));
        assert_eq!(lifecycle_label(Lifecycle::Restored, None).as_deref(), Some("Restored · process exited"));
    }

    /// Types `abc` with a delayed echo and returns the most cells shown.
    fn most_shown(mut predictor: Predictor) -> usize {
        let mut vt = silo_vt::TerminalCore::new(GridSize { columns: 20, rows: 5, cell_width: 8, cell_height: 16 }).unwrap();
        predictor.server(&ServerData::TtyMode { offset: 0, echo: true, icanon: true });
        let (mut offset, mut most) = (0, 0);
        for (count, key) in (1..).zip(b"abc") {
            predictor.input(&[*key], &vt.frame().unwrap());
            most = most.max(predictor.overlay().0.len());
            predictor.server(&ServerData::InputAck { count, offset });
            vt.write_output(&[*key]).unwrap();
            offset += 1;
            predictor.judge(&vt.frame().unwrap(), offset);
        }
        most
    }

    #[test]
    fn local_connection_never_predicts() {
        assert_eq!(most_shown(predictor_for(None)), 0);
        assert!(most_shown(Predictor::remote(Some(Duration::from_secs(1)))) > 0, "the same typing predicts remotely");
    }

    /// `judge_painted`: judged against the painted frame and its offset, a
    /// prediction stays shown until that frame holds the echo.
    #[test]
    fn prediction_stays_until_painted_echo() {
        let mut vt = silo_vt::TerminalCore::new(GridSize { columns: 20, rows: 5, cell_width: 8, cell_height: 16 }).unwrap();
        let mut p = Predictor::remote(Some(Duration::from_secs(1)));
        p.server(&ServerData::TtyMode { offset: 0, echo: true, icanon: true });
        p.input(b"a", &vt.frame().unwrap());
        p.server(&ServerData::InputAck { count: 1, offset: 0 });
        vt.write_output(b"a").unwrap();
        p.judge(&vt.frame().unwrap(), 1);
        let painted = vt.frame().unwrap();
        p.input(b"b", &painted);
        p.server(&ServerData::InputAck { count: 2, offset: 1 });
        vt.write_output(b"b").unwrap();
        p.judge(&painted, 1);
        assert_eq!(p.overlay().0.len(), 1, "the painted frame lacks the echo");
        p.judge(&vt.frame().unwrap(), 2);
        assert_eq!(p.overlay(), (vec![], None));
    }
}
