//! One terminal block: a local mirror restored from the server snapshot and
//! fed the block's output, rendered with the single-canvas painter. Keys,
//! mouse, paste, wheel and selection are encoded locally and sent as input.

use std::{
    ops::Range,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui::{
    Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, ImeTextBatch,
    IntoElement, Keystroke, Pixels, Point, Render, ShapedLine, Task, TextInputPurpose, TextRun, UTF16Selection, Window,
    canvas, div, fill, point, prelude::*, px, rgb, size,
};
use silo_client::{Applied, Control, DataSender, DataStream, Mirror};
use silo_proto::{
    control::{Method, OwnMode},
    data::{ClientData, ServerData},
    model::{BlockInfo, TermSize},
};
use silo_render::{CellMetrics, CursorPresentation, FrameOptions, FramePreparer, Overlay, PreparedFrame, terminal_canvas};
use silo_vt::{Frame, GridSize, KeyEvent, Modifiers, MouseAction, MouseButton, MouseEvent};

use crate::{
    composition::{Composition, InputGate, RawDisposition, byte_range},
    theme,
};

const PADDING: f32 = 6.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Live,
    Exited(i32),
    Lost,
}

pub enum PaneEvent {
    /// The user focused or typed in this pane.
    Activated,
}

pub struct Pane {
    pub block_id: String,
    rt: tokio::runtime::Handle,
    control: Option<Control>,
    mirror: Mirror,
    sender: Option<DataSender>,
    pub status: Status,
    pub info: Option<BlockInfo>,
    /// Bumped whenever the mirror changes; preparation follows it.
    generation: u64,
    preparer: Arc<Mutex<FramePreparer>>,
    prepared: Option<Arc<PreparedFrame>>,
    prepared_key: Option<(u64, u32, u32, u32, u32)>,
    preparing: bool,
    last_frame: Option<Arc<Frame>>,
    content_bounds: Option<Bounds<Pixels>>,
    /// Grid this client wants for the block (from its pixel layout).
    grid: Option<GridSize>,
    pub focus: FocusHandle,
    composition: Composition,
    input_gate: InputGate,
    ime: Option<ImeLayout>,
    dragging: bool,
    /// Lines per autoscroll tick while a selection drag is outside the grid.
    autoscroll: i32,
    drag_event: Option<MouseEvent>,
    autoscroll_task: Option<Task<()>>,
    wheel: f32,
    own_requested: Option<Instant>,
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
    pub fn new(block_id: String, rt: tokio::runtime::Handle, control: Option<Control>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        cx.on_focus(&focus, window, |this, _, cx| {
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
        let mut mirror = Mirror::new();
        mirror.set_colors(theme::terminal_colors());
        let mut pane = Self {
            block_id,
            rt,
            control,
            mirror,
            sender: None,
            status: Status::Connecting,
            info: None,
            generation: 0,
            preparer: Arc::new(Mutex::new(FramePreparer::new(cx.text_system().clone()))),
            prepared: None,
            prepared_key: None,
            preparing: false,
            last_frame: None,
            content_bounds: None,
            grid: None,
            focus,
            composition: Composition::default(),
            input_gate: InputGate::default(),
            ime: None,
            dragging: false,
            autoscroll: 0,
            drag_event: None,
            autoscroll_task: None,
            wheel: 0.0,
            own_requested: None,
            active: false,
            attempt: 0,
            stream_task: None,
        };
        pane.connect(cx);
        pane
    }

    /// A new control connection (after reconnecting to the server).
    pub fn set_control(&mut self, control: Control, cx: &mut Context<Self>) {
        self.control = Some(control);
        self.own_requested = None;
        if !matches!(self.status, Status::Exited(_)) {
            self.connect(cx);
        }
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

    fn is_owner(&self) -> bool {
        let me = self.control.as_ref().map(Control::client_id);
        self.info.as_ref().is_some_and(|i| i.owner.is_some() && i.owner == me)
    }

    /// Ask for size ownership: `Take` on focus and input, `Request` when
    /// merely visible.
    pub fn own(&mut self, mode: OwnMode) {
        if mode == OwnMode::Take && self.is_owner() {
            return;
        }
        if self.own_requested.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return;
        }
        let Some(control) = self.control.clone() else { return };
        self.own_requested = Some(Instant::now());
        let block = self.block_id.clone();
        self.rt.spawn(async move {
            let _ = control.call_value(Method::OwnBlock { block_ids: vec![block], mode }).await;
        });
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let Some(control) = self.control.clone() else { return };
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
        self.sender = Some(stream.sender());
        self.status = Status::Live;
        self.send_size();
        self.own_requested = None;
        self.own(if self.active { OwnMode::Take } else { OwnMode::Request });
        cx.notify();
        true
    }

    fn reset_mirror(&mut self) {
        self.mirror = Mirror::new();
        self.mirror.set_colors(theme::terminal_colors());
        if let Some(grid) = self.grid {
            self.mirror.set_cell_size(grid.cell_width, grid.cell_height);
        }
    }

    /// Apply server frames; returns false to stop the stream task.
    fn apply(&mut self, batch: Vec<ServerData>, cx: &mut Context<Self>) -> bool {
        let mut dirty = false;
        for data in batch {
            match self.mirror.apply(data) {
                Ok(Applied::Redraw) => dirty = true,
                Ok(Applied::None) => {}
                Ok(Applied::Exited(code)) => {
                    self.status = Status::Exited(code);
                    dirty = true;
                }
                Err(error) => {
                    tracing::warn!(block = %self.block_id, "re-sync after: {error:#}");
                    self.reset_mirror();
                    self.connect(cx);
                    return false;
                }
            }
        }
        if dirty {
            self.generation += 1;
            cx.notify();
        }
        true
    }

    fn lost(&mut self, attempt: u64, cx: &mut Context<Self>) {
        if attempt != self.attempt || matches!(self.status, Status::Exited(_)) {
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

    fn send_input(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() || !matches!(self.status, Status::Live) {
            return;
        }
        if let Some(sender) = &self.sender {
            sender.send(&ClientData::Input(bytes));
        }
    }

    fn send_focus(&mut self, focused: bool) {
        if let Some(vt) = self.mirror.terminal()
            && let Ok(bytes) = vt.encode_focus(focused)
        {
            self.send_input(bytes);
        }
    }

    fn send_size(&self) {
        if let (Some(sender), Some(g)) = (&self.sender, self.grid) {
            sender.send(&ClientData::Resize(TermSize { columns: g.columns, rows: g.rows, cell_width: g.cell_width, cell_height: g.cell_height }));
        }
    }

    fn typed(&mut self, cx: &mut Context<Self>) {
        self.own(OwnMode::Take);
        let Some(vt) = self.mirror.terminal() else { return };
        let mut changed = vt.clear_selection();
        if vt.scroll_state().is_ok_and(|s| !s.at_bottom()) {
            vt.scroll_to_row(u64::MAX);
            changed = true;
        }
        if changed {
            self.generation += 1;
            cx.notify();
        }
    }

    fn accepts_input(&self, window: &Window) -> bool {
        self.status == Status::Live && self.focus.is_focused(window) && window.is_window_active()
    }

    // ---- keyboard ----

    fn route_key(&mut self, key: &Keystroke, release: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // Command chords belong to the app (bindings run before this).
        if key.modifiers.platform {
            return false;
        }
        if !self.accepts_input(window) {
            return true;
        }
        let printable = key.key_char.as_ref().is_some_and(|t| !t.is_empty())
            && !key.modifiers.control
            && !key.modifiers.alt
            && !key.modifiers.platform;
        match self.input_gate.route(&key.key, printable, release, self.composition.is_active()) {
            RawDisposition::Encode => self.encode_key(key, release, cx),
            RawDisposition::PlatformText => return false,
            RawDisposition::Ignore => {}
            RawDisposition::Cancel => {
                self.composition.cancel();
                self.ime = None;
                window.invalidate_character_coordinates();
                cx.notify();
            }
        }
        true
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
        let Some(vt) = self.mirror.terminal() else { return };
        if let Ok(bytes) = vt.encode_key(&event)
            && !bytes.is_empty()
        {
            self.send_input(bytes);
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
        let Some(vt) = self.mirror.terminal() else { return };
        if let Ok(bytes) = vt.encode_key(&event) {
            self.send_input(bytes);
            self.typed(cx);
        }
    }

    // ---- clipboard ----

    pub fn copy(&mut self, cx: &mut Context<Self>) {
        let Some(vt) = self.mirror.terminal() else { return };
        if let Ok(text) = vt.copy_selection()
            && !text.is_empty()
        {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        let Some(vt) = self.mirror.terminal() else { return };
        if let Ok(bytes) = vt.paste(&text) {
            self.send_input(bytes);
            self.typed(cx);
        }
    }

    // ---- pointer ----

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn pointer(&mut self, position: Point<Pixels>, action: MouseAction, button: MouseButton, modifiers: gpui::Modifiers, scale: f32, cx: &mut Context<Self>) {
        let Some(bounds) = self.content_bounds else { return };
        let scale = scale.max(0.5);
        let Some(vt) = self.mirror.terminal() else { return };
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
        let exited = matches!(self.status, Status::Exited(_));
        // An exited block has no program to report to: always select.
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
            self.send_input(bytes);
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
        if tracking && self.status == Status::Live {
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

    fn layout(&mut self, bounds: Bounds<Pixels>, metrics: CellMetrics) {
        self.content_bounds = Some(bounds);
        let grid = metrics.grid(f32::from(bounds.size.width), f32::from(bounds.size.height));
        if self.grid != Some(grid) {
            self.grid = Some(grid);
            self.mirror.set_cell_size(grid.cell_width, grid.cell_height);
            self.send_size();
        }
    }

    fn request_prepare(&mut self, metrics: CellMetrics, cx: &mut Context<Self>) {
        let key = (self.generation, metrics.width.to_bits(), metrics.height.to_bits(), metrics.scale.to_bits(), metrics.font);
        if self.preparing || self.prepared_key == Some(key) {
            return;
        }
        let Some(vt) = self.mirror.terminal() else { return };
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
                pane.prepared_key = Some(key);
                cx.notify();
            });
        })
        .detach();
    }

    fn status_text(&self) -> Option<String> {
        match &self.status {
            Status::Live => None,
            Status::Connecting => Some("attaching…".into()),
            Status::Lost => Some("reconnecting…".into()),
            Status::Exited(code) => Some(format!("process exited ({code})")),
        }
    }

    /// The server applies another client's size to this block.
    fn sized_elsewhere(&self) -> bool {
        let applied = self.mirror.size();
        self.status == Status::Live
            && self.grid.is_some_and(|g| g.columns != applied.columns || g.rows != applied.rows)
            && self.info.as_ref().is_some_and(|i| i.owner.is_some()) && !self.is_owner()
    }
}

/// Tab title: a program's title or foreground command, else the
/// abbreviated cwd while a shell sits at its prompt.
pub fn block_title(info: &BlockInfo) -> String {
    let shell = info.title.is_empty()
        || matches!(info.title.trim_start_matches('-'), "zsh" | "bash" | "fish" | "sh" | "nu" | "shell" | "login");
    match info.cwd.as_deref() {
        Some(cwd) if shell => {
            let home = std::env::var("HOME").unwrap_or_default();
            match cwd.strip_prefix(home.as_str()) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => cwd.to_owned(),
            }
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
        let cursor = CursorPresentation { focused, phase_visible: true };
        let selection = rgb(theme::selection()).opacity(0.55);
        let status = self.status_text();
        let sized_elsewhere = self.sized_elsewhere();
        let entity = cx.entity();
        let mut element = div()
            .id(gpui::ElementId::Name(format!("pane-{}", self.block_id).into()))
            .key_context("Terminal")
            .track_focus(&self.focus)
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(theme::background()))
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
                        this.pointer(event.position, MouseAction::Press, mouse_button(Some(event.button)), event.modifiers, window.scale_factor(), cx);
                    }),
                )
                .on_mouse_up(
                    button,
                    cx.listener(|this, event: &gpui::MouseUpEvent, window, cx| {
                        this.pointer(event.position, MouseAction::Release, mouse_button(Some(event.button)), event.modifiers, window.scale_factor(), cx);
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
                    el.child(div().absolute().top_0().left_0().size_full().child(terminal_canvas(prepared, metrics, cursor, selection, Overlay::default())))
                })
                .child(input_canvas(entity, metrics))
                .when_some(status, |el, text| {
                    el.child(
                        div()
                            .absolute()
                            .bottom(px(4.0))
                            .right(px(4.0))
                            .px(px(8.0))
                            .py(px(3.0))
                            .rounded(px(4.0))
                            .bg(rgb(theme::tab_active()))
                            .text_color(rgb(theme::muted()))
                            .text_size(px(11.0))
                            .child(text),
                    )
                })
                .when(sized_elsewhere, |el| {
                    el.child(
                        div()
                            .id("take-size")
                            .absolute()
                            .top(px(2.0))
                            .right(px(2.0))
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(rgb(theme::tab_active()))
                            .text_color(rgb(theme::muted()))
                            .text_size(px(11.0))
                            .cursor_pointer()
                            .child("sized by another client · take size")
                            .on_click(cx.listener(|this, _, _, _| {
                                this.own_requested = None;
                                this.own(OwnMode::Take);
                            })),
                    )
                }),
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
            entity.update(cx, |pane, _| pane.layout(bounds, metrics));
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

    fn text_for_range(&mut self, range: Range<usize>, adjusted: &mut Option<Range<usize>>, window: &mut Window, _: &mut Context<Self>) -> Option<String> {
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

    fn replace_and_mark_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, selected: Option<Range<usize>>, window: &mut Window, cx: &mut Context<Self>) {
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
