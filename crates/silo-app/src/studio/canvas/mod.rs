//! A canvas tab's body: the board (docs/STUDIO.md §4.2). Its logic is [`super::board::Board`] (camera, layout,
//! hit testing, wires, menus, optimistic edits and undo); this view feeds it the store's canvas, project and
//! world, turns GPUI input into board events, carries out the effects it answers with and draws what it reads
//! back. Pieces: `paint.rs` (the dot grid, wires and drafts, and the input layer), `nodes.rs` (frames, cards,
//! assets, notes, entities, ports, dots, selection), `overlays.rs` (toolbar, selection actions, menus, the
//! canvas prompt, minimap, pickers), `settings_panel.rs` (a card's settings), `run.rs` (otto: run, stop,
//! models, preview, the media setup card), `media.rs` (hover playback, import, drop, save).
//!
//! The pane keeps one [`CanvasView`] per open tab and handles [`CanvasEvent`].

mod markdown;
mod media;
mod mentions;
mod nodes;
mod overlays;
mod paint;
mod pump;
mod run;
mod settings_panel;
#[cfg(test)]
mod tests;

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::Instant,
};

use gpui::{
    AppContext as _, Bounds, Context, Entity, EventEmitter, FocusHandle, KeyDownEvent, KeyUpEvent, Pixels, Subscription, Window, div, prelude::*, px,
    rgb,
};
use silo_proto::studio::{CanvasEdited, NodeBody, StudioCanvasEditParams};

use super::{
    board::{self, Board, Effect, Event},
    model,
    store::{self, Store, StoreEvent},
    viewer::{self as host, Layer},
};
use crate::{
    agent::editor::{Editor, EditorEvent},
    context_menu::{self, Key as MenuKey},
    media_viewer::{MediaViewer, ViewerEvent},
    theme,
};

/// What the canvas asks of the pane.
#[derive(Clone, Debug)]
pub enum CanvasEvent {
    /// Show entity `id` in the World view.
    OpenEntity(String),
    /// Put these node ids (empty: the canvas itself) in the linked agent's composer.
    Ask(Vec<String>),
    /// Send the linked agent this message.
    #[allow(dead_code, reason = "prepared asks; the board has none yet")]
    Send(String),
    /// Open this root-relative file in a Files block beside the studio.
    OpenFile(String),
    /// Show this document (root-relative) in the document panel.
    OpenDocument(String),
}

/// A row of one of the canvas's menus.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Pick {
    Board(board::Action),
    Setting(String, board::SettingChange),
    Take(String, u32),
    /// A title row: does nothing.
    Nothing,
}

/// What an inline editor edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InlineKind {
    Prompt,
    Note,
    Title,
}

/// A card's prompt, a note's text or a frame's title, edited in place.
pub(crate) struct Inline {
    pub node: String,
    pub kind: InlineKind,
    pub editor: Entity<Editor>,
    /// ⌘↩ in a prompt: run once saved.
    run: bool,
    _subs: Vec<Subscription>,
}

/// The canvas prompt (double-click, `/`, `G`): a kind and a text field at a view point.
pub(crate) struct CanvasPrompt {
    pub view: board::Point,
    pub makes: silo_proto::studio::GenKind,
    pub editor: Entity<Editor>,
    _subs: Vec<Subscription>,
}

pub struct CanvasView {
    pub name: String,
    pub store: Entity<Store>,
    pub focus: FocusHandle,
    pub(crate) board: Board,
    /// The board's bounds in the window (last paint).
    pub(crate) bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The media viewer open on this canvas (drawn by the pane, over the whole body), and the node it stepped to.
    viewer: Option<(Entity<MediaViewer>, Subscription)>,
    viewed: Option<String>,
    pub(crate) menu: Option<context_menu::Menu<Pick>>,
    /// Batches waiting to be sent (one is out at a time, so they apply in order), and runs waiting for them.
    pub(crate) pump: pump::Pump,
    pub(crate) notice: Option<(String, Instant)>,
    pub(crate) inline: Option<Inline>,
    pub(crate) prompt: Option<CanvasPrompt>,
    /// The `@` popup of the prompt being edited (inline or the canvas prompt), and the `@` esc closed it at.
    pub(crate) mention: Option<mentions::Popup>,
    mention_dismissed: Option<(gpui::EntityId, usize)>,
    /// Where the resting cards' quiet (unwired) chips are, measured at paint, by card: their tooltips.
    pub(crate) chip_tips: Rc<RefCell<HashMap<String, Vec<mentions::Tip>>>>,
    pub(crate) settings: Option<settings_panel::Panel>,
    pub(crate) runs: run::Runs,
    pub(crate) media: media::Hover,
    pub(crate) picker: Option<overlays::Picker>,
    /// A file to move to the Trash, asked first: node, path.
    pub(crate) confirm: Option<(String, String)>,
    /// Prompt heights measured at paint (view points at the zoom drawn), by card.
    pub(crate) measured: Rc<RefCell<HashMap<String, f64>>>,
    /// Notes' and documents' markdown, parsed once per text.
    pub(crate) md: RefCell<markdown::Cache>,
    /// A node to reveal once the canvas is read.
    want_reveal: Option<String>,
    /// A press went to the board (its moves and release follow it, wherever they are).
    pub(crate) pressed: bool,
    /// Its window, for effects carried out after a call answers.
    window: Option<gpui::AnyWindowHandle>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<CanvasEvent> for CanvasView {}

/// How long a notice shows.
const NOTICE: std::time::Duration = std::time::Duration::from_secs(4);

impl CanvasView {
    pub fn new(name: String, store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sub = cx.subscribe_in(&store, window, |this, _, e: &StoreEvent, window, cx| this.store_event(e, window, cx));
        let canvas = store.update(cx, |s, cx| s.want_canvas(&name, cx));
        let seed =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(7, |d| d.as_nanos() as u64) ^ (cx.entity_id().as_u64() << 17);
        let mut board = Board::new(seed);
        {
            let s = store.read(cx);
            board.set_context(s.project(), s.sections.as_deref().unwrap_or_default());
        }
        if let Some(c) = canvas {
            board.set_canvas(&c);
        }
        Self {
            name,
            store,
            focus: cx.focus_handle(),
            board,
            bounds: Rc::default(),
            viewer: None,
            viewed: None,
            menu: None,
            pump: pump::Pump::default(),
            notice: None,
            inline: None,
            prompt: None,
            mention: None,
            mention_dismissed: None,
            chip_tips: Rc::default(),
            settings: None,
            runs: run::Runs::default(),
            media: media::Hover::default(),
            picker: None,
            confirm: None,
            measured: Rc::default(),
            md: RefCell::default(),
            want_reveal: None,
            pressed: false,
            window: Some(window.window_handle()),
            _subs: vec![sub],
        }
    }

    fn store_event(&mut self, e: &StoreEvent, window: &mut Window, cx: &mut Context<Self>) {
        match e {
            StoreEvent::Canvas(n) if *n == self.name => {
                let canvas = self.store.read(cx).canvas(&self.name).cloned();
                if let Some(c) = canvas {
                    let mut fx = self.board.set_canvas(&c);
                    fx.extend(reveal_wanted(&mut self.board, &mut self.want_reveal));
                    self.apply(fx, window, cx);
                    self.settings_changed(cx);
                    self.resync_mentions(cx);
                }
                cx.notify();
            }
            StoreEvent::Project | StoreEvent::World => {
                let s = self.store.read(cx);
                self.board.set_context(s.project(), s.sections.as_deref().unwrap_or_default());
                self.resync_mentions(cx);
                cx.notify();
            }
            StoreEvent::Models => {
                self.models_listed(cx);
                cx.notify();
            }
            StoreEvent::Assets | StoreEvent::Entity(_) => cx.notify(),
            _ => {}
        }
    }

    /// Show and select node `id` (a placed result's link): now, or once the canvas is read and the view sized.
    pub fn reveal_node(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let fx = reveal_when_placed(&mut self.board, &mut self.want_reveal, id);
        self.apply(fx, window, cx);
    }

    /// An entity from the World on this canvas, at the view's centre.
    pub fn add_entity(&mut self, entity: &str, window: &mut Window, cx: &mut Context<Self>) {
        let aspect = self.sheet_aspect(entity, cx);
        let fx = self.board.add_entity(entity, None, aspect);
        self.apply(fx, window, cx);
    }

    /// A document card for `path` (root-relative) at the view's centre, selected.
    pub fn add_document(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let fx = self.board.add_document(path, None);
        self.apply(fx, window, cx);
    }

    /// Entity `id`'s sheet's aspect, when it has one decoded.
    fn sheet_aspect(&self, id: &str, cx: &mut Context<Self>) -> Option<f64> {
        let sheet = self.store.read(cx).sections.iter().flatten().flat_map(|s| &s.entities).find(|e| e.id == id).and_then(|e| e.sheet.clone())?;
        match self.store.update(cx, |s, cx| s.asset(&sheet, store::AssetKind::Image, cx)) {
            store::Asset::Image(l) if l.dims.1 > 0 => Some(f64::from(l.dims.0) / f64::from(l.dims.1)),
            _ => None,
        }
    }

    /// Open the media viewer on `node` (an image or video; a document shows in the document panel, audio plays
    /// on its node). Closing gives the board the focus back, the node shown last selected.
    pub fn view(&mut self, node: String, window: &mut Window, cx: &mut Context<Self>) {
        if host::document(self.board.canvas(), &node) {
            if let Some(path) = self.board.canvas().document_of(&node) {
                cx.emit(CanvasEvent::OpenDocument(path.to_owned()));
            }
            return;
        }
        let source = host::Host::source(self.store.clone(), self.name.clone());
        if !source.items(cx).iter().any(|i| i.id == node) {
            return;
        }
        self.menu = None;
        let viewer = cx.new(|cx| MediaViewer::new(source, &node, window, cx));
        let sub = cx.subscribe_in(&viewer, window, |this, viewer, e: &ViewerEvent, window, cx| match e {
            ViewerEvent::Closed { .. } => {
                window.focus(&this.focus, cx);
                if let Some(node) = this.viewed.take() {
                    this.reveal_node(&node, window, cx);
                }
                cx.notify();
            }
            ViewerEvent::Stepped(node) => this.viewed = Some(node.clone()),
            ViewerEvent::Gone if this.viewer.as_ref().is_some_and(|(v, _)| v == viewer) => {
                this.viewer = None;
                cx.notify();
            }
            ViewerEvent::Gone | ViewerEvent::Note(_) => {}
        });
        self.viewed = None;
        self.viewer = Some((viewer, sub));
        cx.notify();
    }

    /// The media viewer, while open or fading out.
    pub(crate) fn viewer(&self) -> Option<&Entity<MediaViewer>> {
        self.viewer.as_ref().map(|(v, _)| v)
    }

    /// What draws over the board now ([`host::layers`]).
    pub(crate) fn layers(&self, cx: &gpui::App) -> &'static [Layer] {
        host::layers(self.viewer().map(|v| v.read(cx).open()))
    }

    pub(crate) fn say(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.notice = Some((text.into(), Instant::now()));
        cx.notify();
    }

    // ------------------------------------------------------------ effects

    /// Carry out the board's effects.
    pub(crate) fn apply(&mut self, fx: Vec<Effect>, window: &mut Window, cx: &mut Context<Self>) {
        for e in fx {
            match e {
                Effect::Redraw => cx.notify(),
                Effect::Selection => {
                    if let Some(p) = &self.settings
                        && self.board.selection() != [p.node.clone()]
                    {
                        self.settings = None;
                    }
                    cx.notify();
                }
                Effect::Send { batch, ops, base_rev } => {
                    self.pump.push((batch, ops, base_rev));
                    self.send_next(window, cx);
                }
                Effect::Menu => self.open_board_menu(),
                Effect::CloseMenu => self.menu = None,
                Effect::OpenPrompt { view, .. } => self.open_prompt(view, window, cx),
                Effect::View { node } => self.view(node, window, cx),
                Effect::OpenDocument { path } => cx.emit(CanvasEvent::OpenDocument(path)),
                Effect::EditNote { node } => self.start_inline(node, InlineKind::Note, window, cx),
                Effect::FocusPrompt { node } => self.start_inline(node, InlineKind::Prompt, window, cx),
                Effect::RenameFrame { node } => self.start_inline(node, InlineKind::Title, window, cx),
                Effect::Run { node } => {
                    if !self.run_or_connect(node.clone(), cx) {
                        self.pump.run(node);
                        self.release_runs(cx);
                    }
                }
                Effect::Stop { node } => self.stop(node, cx),
                Effect::Settings { node } => self.open_settings(node, window, cx),
                Effect::Takes { node } => self.takes_menu(&node),
                Effect::AskAgent { nodes } => cx.emit(CanvasEvent::Ask(nodes)),
                Effect::OpenWorld { entity } => cx.emit(CanvasEvent::OpenEntity(entity)),
                Effect::PickEntity { at } => self.open_picker(overlays::PickerFor::Place(at), window, cx),
                Effect::Import { at } => self.import_dialog(at, cx),
                Effect::SetAsEntitySheet { path } => self.open_picker(overlays::PickerFor::Sheet(path), window, cx),
                Effect::RevealInFiles { path } => cx.emit(CanvasEvent::OpenFile(path)),
                Effect::Copy { nodes } => {
                    let text: Vec<String> = nodes.iter().map(|n| model::node_target(&self.name, n)).collect();
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.join("\n")));
                }
                Effect::Save { path } => self.save_file(path, cx),
                Effect::DeleteFile { node, path } => {
                    self.confirm = Some((node, path));
                    cx.notify();
                }
                Effect::Notice(text) => self.say(text, cx),
            }
        }
    }

    /// Send the next batch when none is out.
    fn send_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((batch, ops, base_rev)) = self.pump.next() else {
            self.release_runs(cx);
            return;
        };
        let block_id = self.store.read(cx).block_id.clone();
        let method = silo_proto::control::Method::StudioCanvasEdit(StudioCanvasEditParams {
            block_id,
            name: self.name.clone(),
            ops,
            base_rev: Some(base_rev),
        });
        let caller = self.store.read(cx).caller();
        let _ = window;
        caller.call(method, cx, move |this: &mut Self, r: anyhow::Result<CanvasEdited>, cx| {
            let fx = this.pump.answered(&mut this.board, batch, r.map_err(|e| store::describe(&e)));
            this.later(cx, move |this, window, cx| {
                this.apply(fx, window, cx);
                this.send_next(window, cx);
            });
        });
    }

    /// Runs whose edits are all saved go to otto.
    pub(crate) fn release_runs(&mut self, cx: &mut Context<Self>) {
        for node in self.pump.ready_runs() {
            self.run(node, cx);
        }
    }

    /// The board's menu as the app's context menu.
    fn open_board_menu(&mut self) {
        let Some(m) = self.board.menu() else { return };
        let origin = self.bounds.get().origin;
        let at = origin + gpui::point(px(m.at.x as f32), px(m.at.y as f32));
        self.menu = Some(context_menu::Menu::new(at, overlays::menu_items(m)));
    }

    fn takes_menu(&mut self, node: &str) {
        let Some(NodeBody::Gen { takes, pick, .. }) = self.board.canvas().node(node).map(|n| &n.body) else { return };
        let picked = pick.map_or(takes.len().saturating_sub(1), |p| p as usize);
        let items = takes
            .iter()
            .enumerate()
            .rev()
            .map(|(i, t)| {
                let model = t.model.as_deref().map(model::short_model).unwrap_or_default();
                let label = if model.is_empty() { format!("Take {}", i + 1) } else { format!("Take {} \u{b7} {model}", i + 1) };
                context_menu::Item::action(label, "", Pick::Take(node.to_owned(), i as u32)).checked(i == picked)
            })
            .collect();
        let at = self.board.laid(node).map_or(board::Point::ZERO, |l| self.board.camera().to_view(board::Point::new(l.rect.right(), l.rect.y)));
        let at = self.bounds.get().origin + gpui::point(px(at.x as f32), px(at.y as f32));
        self.menu = Some(context_menu::Menu::new(at, items));
    }

    /// A row of a canvas menu was picked (`None`: closed).
    pub(crate) fn pick(&mut self, pick: Option<Pick>, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let fx = match pick {
            None => self.board.close_menu(),
            Some(Pick::Board(a)) => self.board.choose(&a),
            Some(Pick::Setting(node, change)) => self.board.change_setting(&node, &change),
            Some(Pick::Take(node, i)) => self.board.pick_take(&node, i),
            Some(Pick::Nothing) => Vec::new(),
        };
        self.apply(fx, window, cx);
        cx.notify();
    }

    // ------------------------------------------------------------ inline edits

    fn start_inline(&mut self, node: String, kind: InlineKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.inline.as_ref().is_some_and(|i| i.node == node && i.kind == kind) {
            return;
        }
        self.commit_inline(window, cx);
        let (text, placeholder) = match self.board.canvas().node(&node).map(|n| &n.body) {
            Some(NodeBody::Gen { prompt, makes, .. }) => (prompt.clone(), format!("Describe the {}\u{2026}", makes.label().to_lowercase())),
            Some(NodeBody::Note { text }) => (text.clone(), "Write a note\u{2026}".to_owned()),
            Some(NodeBody::Frame { title, .. }) => (title.clone(), "Title".to_owned()),
            _ => return,
        };
        let editor = cx.new(|cx| {
            let mut e = Editor::new(placeholder, cx);
            e.multiline = kind != InlineKind::Title;
            e.single_line = kind == InlineKind::Title;
            e.set_text(text, cx);
            e
        });
        let subs = vec![
            cx.subscribe_in(&editor, window, |this, editor, e: &EditorEvent, window, cx| match e {
                _ if this.mention_event(editor, e, window, cx) => {}
                EditorEvent::Changed | EditorEvent::Moved => {
                    if let Some(node) = this.inline.as_ref().filter(|i| i.kind == InlineKind::Prompt && i.editor == *editor).map(|i| i.node.clone()) {
                        this.sync_mentions(editor, Some(&node), cx);
                    }
                    cx.notify();
                }
                EditorEvent::Submit => {
                    if let Some(i) = this.inline.as_mut()
                        && i.kind == InlineKind::Prompt
                    {
                        i.run = true;
                    }
                    window.focus(&this.focus, cx);
                    this.commit_inline(window, cx);
                }
                EditorEvent::Escape => {
                    window.focus(&this.focus, cx);
                    this.commit_inline(window, cx);
                }
                _ => {}
            }),
            cx.on_focus_out(&editor.read(cx).focus.clone(), window, |this, _, window, cx| this.commit_inline(window, cx)),
        ];
        editor.read(cx).focus.clone().focus(window, cx);
        if kind == InlineKind::Prompt {
            // Its card opens to the full prompt while edited.
            self.board.set_editing(Some(&node));
        }
        if kind == InlineKind::Prompt {
            self.sync_mentions(&editor, Some(&node), cx);
        }
        self.inline = Some(Inline { node, kind, editor, run: false, _subs: subs });
        cx.notify();
    }

    /// Save the inline edit (one batch, one undo step).
    pub(crate) fn commit_inline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(i) = self.inline.take() else { return };
        if self.mention.as_ref().is_some_and(|p| p.editor == i.editor) {
            self.close_mention(cx);
        }
        let text = i.editor.read(cx).text().to_owned();
        let mut fx = match i.kind {
            InlineKind::Prompt => self.board.set_prompt(&i.node, &text),
            InlineKind::Note => self.board.set_note(&i.node, &text),
            InlineKind::Title => self.board.set_frame_title(&i.node, &text),
        };
        if i.kind == InlineKind::Prompt {
            fx.extend(self.board.set_editing(None));
        }
        if i.run {
            fx.push(Effect::Run { node: i.node.clone() });
        }
        self.apply(fx, window, cx);
        cx.notify();
    }

    // ------------------------------------------------------------ input

    /// A board event; the effects carried out.
    pub(crate) fn input(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        let fx = self.board.handle(event);
        self.apply(fx, window, cx);
    }

    fn key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        if let Some(menu) = self.menu.as_mut() {
            match menu.key(k.key.as_str()) {
                MenuKey::Pick(p) => self.pick(Some(p), window, cx),
                MenuKey::Close => self.pick(None, window, cx),
                MenuKey::Stay => cx.notify(),
            }
            return cx.stop_propagation();
        }
        // Keys typed into a field are the field's.
        if !self.focus.is_focused(window) {
            return;
        }
        if k.key == "escape" && (self.confirm.is_some() || self.picker.is_some() || self.runs.setup.is_some() || self.settings.is_some()) {
            if self.confirm.take().is_none() && self.picker.take().is_none() && self.runs.setup.take().is_none() {
                self.settings = None;
            }
            cx.notify();
            return cx.stop_propagation();
        }
        // `[` / `]`: the node before / after the selected one in reading order.
        if (k.key == "[" || k.key == "]") && !k.modifiers.modified() {
            let order = model::walk(self.board.canvas());
            let at = self.board.selection().first().cloned();
            let next = match at {
                Some(at) => model::step(&order, &at, if k.key == "[" { -1 } else { 1 }).cloned(),
                None => order.first().cloned(),
            };
            if let Some(next) = next {
                self.reveal_node(&next, window, cx);
            }
            return cx.stop_propagation();
        }
        let Some(key) = board_key(&k.key, k.modifiers.shift) else { return };
        let fx = self.board.handle(Event::KeyDown { key, mods: mods(&k.modifiers) });
        if !fx.is_empty() || !k.modifiers.platform {
            cx.stop_propagation();
        }
        self.apply(fx, window, cx);
    }

    fn key_up(&mut self, e: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = board_key(&e.keystroke.key, e.keystroke.modifiers.shift) {
            let fx = self.board.handle(Event::KeyUp { key, mods: mods(&e.keystroke.modifiers) });
            self.apply(fx, window, cx);
        }
    }

    // ------------------------------------------------------------ the canvas prompt

    fn open_prompt(&mut self, view: board::Point, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_inline(window, cx);
        let editor = cx.new(|cx| {
            let mut e = Editor::new("Describe what to make\u{2026}  \u{21a9} make and run, \u{2325}\u{21a9} card only", cx);
            e.multiline = false;
            e
        });
        let subs = vec![cx.subscribe_in(&editor, window, |this, editor, e: &EditorEvent, window, cx| match e {
            _ if this.mention_event(editor, e, window, cx) => {}
            EditorEvent::Submit => this.submit_prompt(true, window, cx),
            EditorEvent::Escape => this.close_prompt(window, cx),
            EditorEvent::Changed | EditorEvent::Moved => this.sync_mentions(editor, None, cx),
            _ => cx.notify(),
        })];
        editor.read(cx).focus.clone().focus(window, cx);
        let makes = self.prompt.as_ref().map_or(silo_proto::studio::GenKind::Image, |p| p.makes);
        self.prompt = Some(CanvasPrompt { view, makes, editor, _subs: subs });
        cx.notify();
    }

    pub(crate) fn submit_prompt(&mut self, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.prompt.take() else { return };
        if self.mention.as_ref().is_some_and(|m| m.editor == p.editor) {
            self.close_mention(cx);
        }
        let text = p.editor.read(cx).text().trim().to_owned();
        window.focus(&self.focus, cx);
        let fx = self.board.submit_prompt(p.makes, &text, run);
        self.apply(fx, window, cx);
    }

    pub(crate) fn close_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.prompt.take()
            && self.mention.as_ref().is_some_and(|m| m.editor == p.editor)
        {
            self.close_mention(cx);
        }
        self.board.close_prompt();
        window.focus(&self.focus, cx);
        cx.notify();
    }
}

/// Reveal node `id` when the board can (the camera placed, the node there), else keep it in `want` for later: a
/// reveal before the first fit would be undone by it.
fn reveal_when_placed(board: &mut Board, want: &mut Option<String>, id: &str) -> Vec<Effect> {
    if !board.placed() || board.laid(id).is_none() {
        *want = Some(id.to_owned());
        return Vec::new();
    }
    *want = None;
    board.reveal(id)
}

/// The node kept by [`reveal_when_placed`], revealed if the board can now.
fn reveal_wanted(board: &mut Board, want: &mut Option<String>) -> Vec<Effect> {
    match want.take() {
        Some(id) => reveal_when_placed(board, want, &id),
        None => Vec::new(),
    }
}

/// GPUI modifiers as the board's.
pub(crate) fn mods(m: &gpui::Modifiers) -> board::Mods {
    board::Mods { shift: m.shift, cmd: m.platform, alt: m.alt, ctrl: m.control }
}

/// A key name as the board's key.
fn board_key(key: &str, shift: bool) -> Option<board::Key> {
    use board::Key as K;
    Some(match key {
        "delete" => K::Delete,
        "backspace" => K::Backspace,
        "escape" => K::Escape,
        "enter" => K::Enter,
        "left" => K::Left,
        "right" => K::Right,
        "up" => K::Up,
        "down" => K::Down,
        "space" => K::Space,
        _ => {
            let mut chars = key.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            K::Char(if shift { c.to_ascii_uppercase() } else { c })
        }
    })
}

impl Render for CanvasView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (has_canvas, error) = {
            let s = self.store.read(cx);
            (s.canvas(&self.name).is_some(), s.canvas_error(&self.name).map(str::to_owned))
        };
        // ⌘C, ⌘A, ⌘D are app commands (bound before key listeners run): the board's while it has the focus.
        let board_key_action = |key: char| {
            move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                if !this.focus.is_focused(window) {
                    return cx.propagate();
                }
                let fx = this.board.handle(Event::KeyDown { key: board::Key::Char(key), mods: board::Mods { cmd: true, ..board::Mods::default() } });
                if fx.is_empty() && key == 'd' {
                    return cx.propagate();
                }
                this.apply(fx, window, cx);
            }
        };
        let (copy, all, dup) = (board_key_action('c'), board_key_action('a'), board_key_action('d'));
        let root = div()
            .id("studio-canvas")
            .key_context("StudioCanvas")
            .track_focus(&self.focus)
            .on_action(cx.listener(move |this, _: &crate::commands::act::Copy, window, cx| copy(this, window, cx)))
            .on_action(cx.listener(move |this, _: &crate::commands::act::SelectAll, window, cx| all(this, window, cx)))
            .on_action(cx.listener(move |this, _: &crate::commands::act::SplitRight, window, cx| dup(this, window, cx)))
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .relative()
            .size_full()
            .overflow_hidden();
        if !has_canvas {
            let text = error.unwrap_or_else(|| "Loading\u{2026}".into());
            return root.child(div().size_full().flex().items_center().justify_center().text_color(rgb(theme::muted())).child(text));
        }
        // The board's size, known after the first paint.
        let b = self.bounds.get();
        let size = board::Size::new(f64::from(f32::from(b.size.width)), f64::from(f32::from(b.size.height)));
        if size.w > 0.0 {
            self.board.set_view_size(size);
            self.board.set_minimap_frame(overlays::minimap_frame(size));
            let fx = reveal_wanted(&mut self.board, &mut self.want_reveal);
            self.apply(fx, window, cx);
        }
        // Prompt heights measured at the last paint.
        let measured: Vec<(String, f64)> = self.measured.borrow_mut().drain().collect();
        let zoom = self.board.camera().zoom;
        for (id, h) in measured {
            let h = h / zoom + 2.0 * board::layout::PROMPT_PAD;
            let fx = self.board.measure_prompt(&id, h);
            self.apply(fx, window, cx);
        }
        if self.notice.as_ref().is_some_and(|(_, at)| at.elapsed() > NOTICE) {
            self.notice = None;
        }
        self.media.tick(&self.board, &self.store, window, cx);
        let under = paint::under(self, cx);
        let nodes = nodes::render(self, window, cx);
        let over = paint::over(self);
        // Under an open media viewer none of them (the pane draws it over the whole body).
        let drawn = self.layers(cx).contains(&Layer::Overlays);
        let overlays = if drawn { overlays::render(self, window, cx) } else { Vec::new() };
        let menu = self
            .menu
            .as_ref()
            .filter(|_| drawn)
            .map(|m| m.render("studio-canvas-menu", cx, |this: &mut Self, p, window, cx| this.pick(p, window, cx)));
        root.on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| this.drop_paths(paths.paths().to_vec(), window, cx)))
            .on_drop(cx.listener(|this, d: &crate::files::DraggedFiles, window, cx| this.drop_files(d, window, cx)))
            .child(under)
            .children(nodes)
            .child(over)
            .children(overlays)
            .children(menu)
    }
}

/// An element at view rect `r` (board coordinates, absolutely placed).
pub(crate) fn at(r: board::Rect) -> gpui::Div {
    div().absolute().left(px(r.x as f32)).top(px(r.y as f32)).w(px(r.w as f32)).h(px(r.h as f32))
}
