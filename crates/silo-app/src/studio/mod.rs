//! The Studio block (docs/STUDIO.md §4): a media project's world and canvases. The header holds the studio's
//! name (the Production view's button), the canvas tabs, `+`, the format chip and the World toggle (`header.rs`);
//! the body is Setup before `studio.toml` exists (`setup.rs`), the World view (`world.rs`), the Production view
//! (`production.rs`: asked for, or no canvas open) or the active canvas ([`canvas::CanvasView`]).
//! The server holds the view state (`StudioState`, changed with `studio.set` so every client follows); the data
//! and the asset cache live in the [`store::Store`], shared with the canvas views. Inline edits live here only.

// The board's pure logic (camera, layout, hit testing, wires, interaction, undo); drawn by `canvas`.
pub mod board;
pub mod canvas;
mod documents;
mod header;
pub mod mentions;
pub mod model;
mod production;
mod setup;
pub mod store;
pub mod tiles;
pub mod viewer;
mod world;

#[cfg(test)]
mod tests;

use std::{cell::Cell, collections::HashMap, rc::Rc};

use gpui::{AnyElement, AppContext as _, Context, Entity, EventEmitter, FocusHandle, KeyDownEvent, Subscription, Window, div, prelude::*, px, rgb};
use silo_proto::{
    model::BlockInfo,
    studio::{StudioSetParams, StudioState, StudioView},
};

use self::{
    canvas::{CanvasEvent, CanvasView},
    store::{Store, StoreEvent},
};
use crate::{
    agent::editor::{Editor, EditorEvent},
    commands::act,
    context_menu::{Key, Menu},
    studio_block::Then,
    text_field::{FieldEvent, TextField},
    theme,
};

pub enum StudioPaneEvent {
    /// Focus came in: the server's focus follows.
    Activated,
    /// Something its header shows changed.
    Header,
    /// The linked agent gets this (found, or split off the block).
    Agent(Then),
    /// Open this root-relative file in a Files block beside it.
    OpenFile(String),
}

/// What a menu row does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MenuAction {
    /// The active canvas's format: a preset, `None` for the project's.
    CanvasFormat(Option<String>),
    ProjectFormat(String),
    EditFormats,
    NewCanvas,
    RenameCanvas(String),
    OpenCanvas(String),
    NewEntity(String),
    CreateWithAgent(String),
    /// Entity, file (root-relative).
    SetSheet(String, String),
    /// Entity, reference id: a one-line ask for its name or what it defines; take it off the list (the file
    /// stays); move it to a place (0: first).
    RenameRef(String, String),
    DescribeRef(String, String),
    RemoveRef(String, String),
    MoveRef(String, String, usize),
    /// Entity, other entity: link them (`true`) or unlink them.
    Link(String, String, bool),
    /// A mention token to the clipboard.
    CopyText(String),
    /// A file (root-relative), to fix by hand: in Finder, its default app (this Mac's host), a Files block.
    RevealInFinder(String),
    OpenInApp(String),
    OpenInFiles(String),
    /// The project's model pick for a kind (`image`, `video`, `speech`): a handle, `None` for Automatic.
    SetModel(String, Option<String>),
}

/// A one-line ask over the body.
pub(crate) enum Dialog {
    /// New canvas: its title; an episode number (`None`: not one); its format (`None`: the project's).
    NewCanvas {
        field: Entity<TextField>,
        episode: Option<u32>,
        format: Option<String>,
    },
    Rename {
        canvas: String,
        field: Entity<TextField>,
    },
    /// A new entity in `section`; `agent`: described for the agent to make.
    NewEntity {
        section: String,
        field: Entity<TextField>,
        agent: bool,
    },
    NewSection {
        field: Entity<TextField>,
    },
    /// A reference of `entity` (by id `look`): its name (`defines: false`) or what it defines.
    Ref {
        entity: String,
        look: String,
        defines: bool,
        field: Entity<TextField>,
    },
}

/// What an inline edit saves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EditTarget {
    /// A markdown file (root-relative): `world/overview.md`, `world/style.md`.
    File(String),
    /// An entity's description.
    EntityBody(String),
}

/// What the pane's own media viewer is open on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Viewing {
    /// An entity's page (the World view): its sheet and refs.
    Entity(String),
    /// An episode's cuts (the Production view's ▶ Play), by canvas.
    Cut(String),
}

/// The pane's media viewer (the World view's and the Production view's; a canvas's is its view's).
pub(crate) struct PaneViewer {
    pub on: Viewing,
    pub viewer: Entity<crate::media_viewer::MediaViewer>,
    _sub: Subscription,
}

pub(crate) struct Editing {
    pub target: EditTarget,
    pub editor: Entity<Editor>,
    pub saving: bool,
    _sub: Subscription,
}

pub struct StudioPane {
    pub block_id: String,
    pub focus: FocusHandle,
    pub info: Option<BlockInfo>,
    pub(crate) state: StudioState,
    pub store: Entity<Store>,
    canvases: HashMap<String, (Entity<CanvasView>, Subscription)>,
    pub(crate) menu: Option<Menu<MenuAction>>,
    pub(crate) dialog: Option<(Dialog, Subscription)>,
    pub(crate) editing: Option<Editing>,
    /// Setup's fields.
    pub(crate) setup: setup::Setup,
    /// Sections made here and still empty (their folder is made at once; listed until the world is read again).
    pub(crate) new_sections: Vec<String>,
    /// A node to reveal when its canvas's view is made: canvas, node.
    reveal: Option<(String, String)>,
    /// The document shown in the document panel (root-relative), over its canvas.
    pub(crate) document: Option<String>,
    /// The Documents list is open.
    pub(crate) docs_open: bool,
    /// Rendered markdown by source, for Overview, Style and descriptions.
    pub(crate) markdown: HashMap<String, (std::sync::Arc<str>, crate::agent::MarkdownView)>,
    /// The last failure, shown until the next action.
    pub(crate) notice: Option<String>,
    body_bounds: Rc<Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    /// The media viewer open on an entity's page or an episode's cuts (a canvas's is its view's).
    pub(crate) pane_viewer: Option<PaneViewer>,
    pub(crate) surface: crate::pane_surface::PaneSurface,
    /// The block's host is this Mac: its files open in Finder and their default apps.
    pub local: bool,
    _subs: Vec<Subscription>,
}

impl EventEmitter<StudioPaneEvent> for StudioPane {}

impl StudioPane {
    pub fn new(
        block_id: String,
        info: Option<BlockInfo>,
        rt: tokio::runtime::Handle,
        control: Option<silo_client::Control>,
        otto: Option<crate::agent::client::Otto>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let state = info.as_ref().and_then(|i| i.studio.as_deref().cloned()).unwrap_or_else(|| StudioState::new(String::new()));
        let store = cx.new(|cx| {
            let mut s = Store::new(block_id.clone(), state.root.clone(), rt, control, otto);
            s.reload(cx);
            s
        });
        let subs = vec![
            cx.on_focus_in(&focus, window, |_, _, cx| cx.emit(StudioPaneEvent::Activated)),
            cx.subscribe(&store, |this, _, e: &StoreEvent, cx| this.store_event(e, cx)),
        ];
        let setup = setup::Setup::new(cx);
        Self {
            block_id,
            focus,
            info,
            state,
            store,
            canvases: HashMap::new(),
            menu: None,
            dialog: None,
            editing: None,
            setup,
            new_sections: Vec::new(),
            reveal: None,
            document: None,
            docs_open: false,
            markdown: HashMap::new(),
            notice: None,
            body_bounds: Rc::default(),
            pane_viewer: None,
            surface: crate::pane_surface::PaneSurface::default(),
            local: false,
            _subs: subs,
        }
    }

    pub fn set_control(&mut self, control: silo_client::Control, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| s.set_control(control, cx));
    }

    pub(crate) fn set_card(&mut self, card: crate::adaptive::Chrome, cx: &mut Context<Self>) {
        let surface = crate::pane_surface::PaneSurface(Some(card));
        if self.surface != surface {
            self.surface = surface;
            cx.notify();
        }
    }

    pub fn bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.body_bounds.get()
    }

    /// What a close would lose: an inline edit in progress.
    pub fn unsaved(&self, cx: &gpui::App) -> Vec<String> {
        let Some(e) = &self.editing else { return Vec::new() };
        if e.editor.read(cx).text().is_empty() && matches!(e.target, EditTarget::EntityBody(_)) {
            return Vec::new();
        }
        vec![match &e.target {
            EditTarget::File(path) => path.clone(),
            EditTarget::EntityBody(id) => format!("{id} (description)"),
        }]
    }

    /// The server's view of the block (a session view, `block.changed`).
    pub fn set_info(&mut self, info: BlockInfo, cx: &mut Context<Self>) {
        if let Some(state) = info.studio.as_deref().cloned()
            && state != self.state
        {
            let root = state.root.clone();
            self.state = state;
            self.store.update(cx, |s, cx| s.set_root(root, cx));
            self.canvases.retain(|name, _| self.state.open.contains(name));
            cx.emit(StudioPaneEvent::Header);
            cx.notify();
        }
        self.info = Some(info);
    }

    fn store_event(&mut self, e: &StoreEvent, cx: &mut Context<Self>) {
        match e {
            StoreEvent::Failed(why) => self.notice = Some(why.clone()),
            StoreEvent::Project => cx.emit(StudioPaneEvent::Header),
            _ => {}
        }
        cx.notify();
    }

    /// Change the view state: shown at once, then `studio.set` (every client follows).
    pub(crate) fn set(&mut self, params: StudioSetParams, cx: &mut Context<Self>) {
        let s = &mut self.state;
        if let Some(view) = params.view {
            s.view = view;
        }
        if let Some(open) = &params.open {
            s.open.clone_from(open);
        }
        if let Some(active) = &params.active {
            s.active.clone_from(active);
            if let Some(a) = active
                && !s.open.contains(a)
            {
                s.open.push(a.clone());
            }
        }
        if let Some(section) = &params.world_section {
            s.world_section.clone_from(section);
        }
        if let Some(entity) = &params.world_entity {
            s.world_entity.clone_from(entity);
        }
        let method = silo_proto::control::Method::StudioSet(StudioSetParams { block_id: self.block_id.clone(), ..params });
        self.store.update(cx, |s, cx| s.send(method, cx));
        cx.emit(StudioPaneEvent::Header);
        cx.notify();
    }

    pub(crate) fn caller(&self, cx: &gpui::App) -> store::Caller {
        self.store.read(cx).caller()
    }

    pub(crate) fn toggle_world(&mut self, cx: &mut Context<Self>) {
        let view = if self.state.view == StudioView::World { StudioView::Canvas } else { StudioView::World };
        self.set(StudioSetParams { view: Some(view), ..StudioSetParams::default() }, cx);
    }

    /// Show canvas `name` (opened as a tab if it isn't).
    pub(crate) fn show_canvas(&mut self, name: String, cx: &mut Context<Self>) {
        self.set(StudioSetParams { view: Some(StudioView::Canvas), active: Some(Some(name)), ..StudioSetParams::default() }, cx);
    }

    /// Show canvas `canvas` with node `node` revealed and selected (a placed result's link), once it is read.
    pub(crate) fn reveal_node(&mut self, canvas: String, node: String, window: &mut Window, cx: &mut Context<Self>) {
        self.show_canvas(canvas.clone(), cx);
        if node.is_empty() {
            return;
        }
        let view = self.canvas_view(&canvas, window, cx);
        view.update(cx, |v, cx| v.reveal_node(&node, window, cx));
    }

    /// Reveal `node` of `canvas` when its view is next shown (the pane was made for it, or before it shows).
    pub(crate) fn reveal_later(&mut self, canvas: String, node: String) {
        if !node.is_empty() {
            self.reveal = Some((canvas, node));
        }
    }

    /// The node to reveal on canvas `name`'s view, taken once.
    fn take_reveal(&mut self, name: &str) -> Option<String> {
        take_reveal(&mut self.reveal, name)
    }

    /// Put entity `id` on the active canvas (the board places it at the view's centre) and show it.
    pub(crate) fn add_to_canvas(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(canvas) = self.state.active.clone() else { return };
        let view = self.canvas_view(&canvas, window, cx);
        view.update(cx, |v, cx| v.add_entity(id, window, cx));
        self.show_canvas(canvas, cx);
    }

    /// Show entity `id` in the World view.
    pub(crate) fn show_entity(&mut self, id: String, cx: &mut Context<Self>) {
        let section = id.split_once('/').map(|(s, _)| s.to_owned());
        self.set(
            StudioSetParams {
                view: Some(StudioView::World),
                world_section: Some(section),
                world_entity: Some(Some(id)),
                ..StudioSetParams::default()
            },
            cx,
        );
    }

    /// The canvas view of tab `name`, if made.
    fn canvas_entity(&self, name: &str) -> Option<&Entity<CanvasView>> {
        self.canvases.get(name).map(|(v, _)| v)
    }

    /// The canvas view of tab `name`, made on first show.
    fn canvas_view(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) -> Entity<CanvasView> {
        if let Some((v, _)) = self.canvases.get(name) {
            let view = v.clone();
            // Made before the reveal was asked (the view state came first): it takes it now.
            if let Some(node) = self.take_reveal(name) {
                view.update(cx, |v, cx| v.reveal_node(&node, window, cx));
            }
            return view;
        }
        let store = self.store.clone();
        let view = cx.new(|cx| CanvasView::new(name.to_owned(), store, window, cx));
        let sub = cx.subscribe(&view, |this, view, e: &CanvasEvent, cx| {
            let canvas = view.read(cx).name.clone();
            match e {
                CanvasEvent::OpenEntity(id) => this.show_entity(id.clone(), cx),
                CanvasEvent::Ask(nodes) => {
                    let mentions = if nodes.is_empty() {
                        vec![(otto_proto::MentionKind::Canvas, canvas)]
                    } else {
                        nodes.iter().map(|n| (otto_proto::MentionKind::Canvas, model::node_target(&canvas, n))).collect()
                    };
                    cx.emit(StudioPaneEvent::Agent(Then::Mentions(mentions)));
                }
                CanvasEvent::Send(text) => cx.emit(StudioPaneEvent::Agent(Then::Send(text.clone()))),
                CanvasEvent::OpenFile(path) => cx.emit(StudioPaneEvent::OpenFile(path.clone())),
                CanvasEvent::OpenDocument(path) => this.open_document(path.clone(), cx),
            }
        });
        self.canvases.insert(name.to_owned(), (view.clone(), sub));
        if let Some(node) = self.take_reveal(name) {
            view.update(cx, |v, cx| v.reveal_node(&node, window, cx));
        }
        view
    }

    /// Markdown for `source`, rendered once per text, `pad` points in from each side.
    pub(crate) fn markdown(&mut self, key: &str, source: &str, pad: f32) -> AnyElement {
        let fresh = self.markdown.get(key).is_none_or(|(s, _)| &**s != source);
        if fresh {
            self.markdown.insert(key.to_owned(), (source.into(), crate::agent::MarkdownView::new(source)));
        }
        self.markdown[key].1.render(pad)
    }

    /// Markdown for `source` at its full height, for a page that scrolls as a whole.
    pub(crate) fn markdown_flat(&mut self, key: &str, source: &str, cx: &gpui::App) -> AnyElement {
        let fresh = self.markdown.get(key).is_none_or(|(s, _)| &**s != source);
        if fresh {
            self.markdown.insert(key.to_owned(), (source.into(), crate::agent::MarkdownView::new(source)));
        }
        self.markdown[key].1.render_flat(cx)
    }

    // ------------------------------------------------------------ inline edits

    pub(crate) fn start_edit(&mut self, target: EditTarget, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            let mut e = Editor::new("Markdown…", cx);
            e.multiline = true;
            e.set_text(text.to_owned(), cx);
            e
        });
        let sub = cx.subscribe(&editor, |this, _, e: &EditorEvent, cx| match e {
            EditorEvent::Submit => this.save_edit(cx),
            EditorEvent::Escape => {
                this.editing = None;
                cx.notify();
            }
            _ => {}
        });
        editor.read(cx).focus.clone().focus(window, cx);
        self.editing = Some(Editing { target, editor, saving: false, _sub: sub });
        cx.notify();
    }

    /// Save the inline edit: a file with `files.write` on this block (Files' rules), a description with
    /// `studio.entity_update`.
    pub(crate) fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some(e) = self.editing.as_mut().filter(|e| !e.saving) else { return };
        e.saving = true;
        let text = e.editor.read(cx).text().to_owned();
        let target = e.target.clone();
        let block_id = self.block_id.clone();
        let method = match &target {
            EditTarget::File(path) => silo_proto::control::Method::FilesWrite(silo_proto::control::FilesWriteParams {
                block_id,
                path: path.clone(),
                text,
                base: None,
                force: false,
            }),
            EditTarget::EntityBody(id) => silo_proto::control::Method::StudioEntityUpdate(silo_proto::studio::StudioEntityUpdateParams {
                block_id,
                id: id.clone(),
                fields: None,
                body: Some(text),
            }),
        };
        let caller = self.store.read(cx).caller();
        caller.call(method, cx, move |this: &mut Self, r: anyhow::Result<serde_json::Value>, cx| match r {
            Ok(_) => {
                this.editing = None;
                if let EditTarget::File(path) = &target {
                    this.store.update(cx, |s, cx| s.stale(path, cx));
                }
            }
            Err(error) => {
                if let Some(e) = this.editing.as_mut() {
                    e.saving = false;
                }
                this.notice = Some(store::describe(&error));
            }
        });
    }

    // ------------------------------------------------------------ dialogs

    pub(crate) fn open_dialog(&mut self, dialog: Dialog, window: &mut Window, cx: &mut Context<Self>) {
        let field = match &dialog {
            Dialog::NewCanvas { field, .. }
            | Dialog::Rename { field, .. }
            | Dialog::NewEntity { field, .. }
            | Dialog::NewSection { field }
            | Dialog::Ref { field, .. } => field.clone(),
        };
        let sub = cx.subscribe_in(&field, window, |this, _, e: &FieldEvent, window, cx| match e {
            FieldEvent::Submit => this.submit_dialog(window, cx),
            FieldEvent::Cancel => this.close_dialog(window, cx),
            _ => {}
        });
        field.read(cx).focus.clone().focus(window, cx);
        self.dialog = Some((dialog, sub));
        self.menu = None;
        cx.notify();
    }

    pub(crate) fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn submit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((dialog, _)) = self.dialog.take() else { return };
        window.focus(&self.focus, cx);
        let text = |f: &Entity<TextField>, cx: &mut Context<Self>| f.read(cx).text.trim().to_owned();
        let block_id = self.block_id.clone();
        use silo_proto::{control::Method, studio as s};
        match dialog {
            Dialog::NewCanvas { field, episode, format } => {
                let title = text(&field, cx);
                if title.is_empty() {
                    return;
                }
                let method = Method::StudioCanvasCreate(s::StudioCanvasCreateParams { block_id, title, episode, format, name: None });
                self.caller(cx).call(method, cx, |this: &mut Self, r: anyhow::Result<s::CanvasInfo>, cx| match r {
                    Ok(info) => {
                        this.store.update(cx, |s, cx| s.load_project(cx));
                        this.show_canvas(info.name, cx);
                    }
                    Err(e) => this.notice = Some(store::describe(&e)),
                });
            }
            Dialog::Rename { canvas, field } => {
                let title = text(&field, cx);
                if title.is_empty() {
                    return;
                }
                let method = Method::StudioCanvasUpdate(s::StudioCanvasUpdateParams {
                    block_id,
                    name: canvas,
                    title: Some(title),
                    episode: None,
                    format: None,
                });
                self.store.update(cx, |s, cx| s.send(method, cx));
            }
            Dialog::NewEntity { section, field, agent } => {
                let name = text(&field, cx);
                if name.is_empty() {
                    return;
                }
                if agent {
                    let title = self.section_title(&section, cx);
                    cx.emit(StudioPaneEvent::Agent(Then::Send(model::create_with_agent(&title, &name))));
                    return;
                }
                let method = Method::StudioEntityCreate(s::StudioEntityCreateParams { block_id, section, name, fields: None, body: None });
                self.caller(cx).call(method, cx, |this: &mut Self, r: anyhow::Result<s::EntityInfo>, cx| match r {
                    Ok(info) => {
                        this.store.update(cx, |s, cx| s.load_world(cx));
                        this.show_entity(info.id, cx);
                    }
                    Err(e) => this.notice = Some(store::describe(&e)),
                });
            }
            Dialog::NewSection { field } => {
                let Some(slug) = section_slug(&text(&field, cx)) else { return };
                if !self.new_sections.contains(&slug) {
                    self.new_sections.push(slug.clone());
                }
                // The folder now (`files.*` work on a Studio block), so the section is there for everyone.
                let mkdir = Method::FilesMkdir(silo_proto::files::FilesMkdirParams { block_id, path: format!("world/{slug}"), unique: false });
                self.caller(cx).call(mkdir, cx, |this: &mut Self, r: anyhow::Result<serde_json::Value>, cx| match made_or_exists(r) {
                    Ok(()) => this.store.update(cx, |s, cx| s.load_world(cx)),
                    Err(e) => this.notice = Some(store::describe(&e)),
                });
                self.set(StudioSetParams { world_section: Some(Some(slug)), world_entity: Some(None), ..StudioSetParams::default() }, cx);
            }
            // Empty: back to its file's name, or nothing said (the server's rule for `None`).
            Dialog::Ref { entity, look, defines, field } => {
                let said = Some(text(&field, cx)).filter(|t| !t.is_empty());
                let change =
                    if defines { s::RefChange::Describe { ref_id: look, defines: said } } else { s::RefChange::Rename { ref_id: look, name: said } };
                self.store.update(cx, |st, cx| st.edit_ref(&entity, change, cx));
            }
        }
        cx.notify();
    }

    /// Reference `look` of entity `entity` as last read.
    fn look_of(&self, entity: &str, look: &str, cx: &gpui::App) -> Option<silo_proto::studio::RefInfo> {
        self.store.read(cx).entity_info(entity).and_then(|e| model::entity_looks(e).into_iter().find(|l| l.id == look))
    }

    /// The one-line ask for reference `look`'s name (`defines: false`) or what it defines, filled with it now.
    fn ask_ref(&mut self, entity: String, look: String, defines: bool, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.look_of(&entity, &look, cx);
        let (hint, text) = if defines {
            ("What to take from it (his bare head and hair, without the crown)", now.and_then(|l| l.defines))
        } else {
            ("Name (Crown off)", now.map(|l| l.name))
        };
        let field = cx.new(|cx| {
            let mut f = TextField::new(hint, cx);
            f.set_text(text.unwrap_or_default(), cx);
            f.select_all(cx);
            f
        });
        self.open_dialog(Dialog::Ref { entity, look, defines, field }, window, cx);
    }

    /// Pick files on this Mac and add each to entity `id`'s references (copied into its `refs/` folder).
    pub(crate) fn add_ref_files(&mut self, id: String, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Add".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let _ = this.update(cx, |this, cx| {
                for p in paths {
                    let change = silo_proto::studio::RefChange::Add { path: p.to_string_lossy().into_owned(), name: None, defines: None, at: None };
                    this.store.update(cx, |s, cx| s.edit_ref(&id, change, cx));
                }
            });
        })
        .detach();
    }

    /// A section's title (`Characters`), its id when not listed.
    pub(crate) fn section_title(&self, id: &str, cx: &gpui::App) -> String {
        self.store.read(cx).sections.iter().flatten().find(|s| s.id == id).map_or_else(|| id.to_owned(), |s| s.title.clone())
    }

    // ------------------------------------------------------------ menus

    pub(crate) fn open_menu(&mut self, menu: Menu<MenuAction>, cx: &mut Context<Self>) {
        self.menu = Some(menu);
        cx.notify();
    }

    fn pick(&mut self, action: Option<MenuAction>, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let Some(action) = action else { return cx.notify() };
        use silo_proto::{control::Method, studio as s};
        let block_id = self.block_id.clone();
        match action {
            MenuAction::CanvasFormat(format) => {
                let Some(canvas) = self.state.active.clone() else { return };
                let method = Method::StudioSetFormat(s::StudioSetFormatParams { block_id, canvas: Some(canvas), format });
                self.set_format(method, cx);
            }
            MenuAction::ProjectFormat(format) => {
                let method = Method::StudioSetFormat(s::StudioSetFormatParams { block_id, canvas: None, format: Some(format) });
                self.set_format(method, cx);
            }
            MenuAction::EditFormats => cx.emit(StudioPaneEvent::OpenFile("studio.toml".into())),
            MenuAction::NewCanvas => {
                let field = cx.new(|cx| TextField::new("Title", cx));
                self.open_dialog(Dialog::NewCanvas { field, episode: None, format: None }, window, cx);
            }
            MenuAction::RenameCanvas(canvas) => {
                let title = self.store.read(cx).project().and_then(|p| p.canvases.iter().find(|c| c.name == canvas)).map(|c| c.title.clone());
                let field = cx.new(|cx| {
                    let mut f = TextField::new("Title", cx);
                    f.set_text(title.unwrap_or_default(), cx);
                    f.select_all(cx);
                    f
                });
                self.open_dialog(Dialog::Rename { canvas, field }, window, cx);
            }
            MenuAction::OpenCanvas(name) => self.show_canvas(name, cx),
            MenuAction::NewEntity(section) => {
                let field = cx.new(|cx| TextField::new("Name", cx));
                self.open_dialog(Dialog::NewEntity { section, field, agent: false }, window, cx);
            }
            MenuAction::CreateWithAgent(section) => {
                let field = cx.new(|cx| TextField::new("Describe it for the agent", cx));
                self.open_dialog(Dialog::NewEntity { section, field, agent: true }, window, cx);
            }
            MenuAction::SetSheet(id, path) => {
                let rel = world::entity_relative(&id, &path);
                let fields = std::collections::BTreeMap::from([("sheet".to_owned(), serde_json::Value::String(rel))]);
                let method = Method::StudioEntityUpdate(s::StudioEntityUpdateParams { block_id, id, fields: Some(fields), body: None });
                self.store.update(cx, |s, cx| s.send(method, cx));
            }
            MenuAction::RenameRef(id, look) => self.ask_ref(id, look, false, window, cx),
            MenuAction::DescribeRef(id, look) => self.ask_ref(id, look, true, window, cx),
            MenuAction::RemoveRef(id, look) => {
                let change = s::RefChange::Remove { ref_id: look };
                self.store.update(cx, |st, cx| st.edit_ref(&id, change, cx));
            }
            MenuAction::MoveRef(id, look, to) => {
                let change = s::RefChange::Move { ref_id: look, to };
                self.store.update(cx, |st, cx| st.edit_ref(&id, change, cx));
            }
            MenuAction::Link(id, other, linked) => self.store.update(cx, |s, cx| s.link(&id, &other, linked, cx)),
            MenuAction::CopyText(text) => cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)),
            MenuAction::RevealInFinder(path) => {
                if let Some(file) = self.local_file(&path) {
                    cx.reveal_path(&file);
                }
            }
            MenuAction::OpenInApp(path) => {
                if let Some(file) = self.local_file(&path) {
                    cx.open_with_system(&file);
                }
            }
            MenuAction::OpenInFiles(path) => cx.emit(StudioPaneEvent::OpenFile(path)),
            MenuAction::SetModel(kind, model) => self.store.update(cx, |s, cx| s.set_model(&kind, model, cx)),
        }
        cx.notify();
    }

    /// File `path` (root-relative) on this Mac, when the block's host is this Mac.
    fn local_file(&self, path: &str) -> Option<std::path::PathBuf> {
        (self.local && !self.state.root.is_empty()).then(|| std::path::Path::new(&self.state.root).join(path))
    }

    /// A format change; its answer is the project.
    fn set_format(&mut self, method: silo_proto::control::Method, cx: &mut Context<Self>) {
        self.caller(cx).call(method, cx, |this: &mut Self, r: anyhow::Result<silo_proto::studio::Project>, cx| match r {
            Ok(p) => this.store.update(cx, |s, cx| s.took_project(p, cx)),
            Err(e) => this.notice = Some(store::describe(&e)),
        });
    }

    // ------------------------------------------------------------ keys

    fn key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        if let Some(menu) = self.menu.as_mut() {
            match menu.key(k.key.as_str()) {
                Key::Pick(a) => self.pick(Some(a), window, cx),
                Key::Close => self.pick(None, window, cx),
                Key::Stay => cx.notify(),
            }
            return cx.stop_propagation();
        }
        // Esc with the focus on the pane itself: the Documents list, then the document panel.
        if k.key == "escape" && self.editing.is_none() && (self.docs_open || self.document.is_some()) {
            if self.docs_open {
                self.docs_open = false;
                cx.notify();
            } else {
                self.close_document(cx);
            }
            return cx.stop_propagation();
        }
        // ⌥⌘↩ outside a canvas: the studio in the agent's composer.
        if k.key == "enter" && k.modifiers.platform && k.modifiers.alt && self.state.view != StudioView::Canvas {
            let mention = match (&self.state.view, &self.state.world_entity) {
                (StudioView::World, Some(id)) => (otto_proto::MentionKind::Entity, id.clone()),
                _ => return,
            };
            cx.emit(StudioPaneEvent::Agent(Then::Mentions(vec![mention])));
            cx.stop_propagation();
        }
    }

    // ------------------------------------------------------------ drawing

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (missing, project_error) = {
            let s = self.store.read(cx);
            let error = match &s.project {
                Some(store::Load::Failed(e)) if !s.missing => Some(e.clone()),
                _ => None,
            };
            (s.missing, error)
        };
        if missing || self.state.view == StudioView::Setup {
            return self.render_setup(window, cx);
        }
        if let Some(error) = project_error {
            return centered(error);
        }
        if self.state.view == StudioView::World {
            return self.render_world(window, cx);
        }
        match self.state.active.clone().filter(|_| !production::shows_production(self.state.view, self.state.active.as_deref())) {
            Some(name) => {
                let view = self.canvas_view(&name, window, cx);
                self.render_canvas(&name, view, cx)
            }
            None => self.render_production(window, cx),
        }
    }

    /// Whether the body shows the Production view (not Setup, nor the project's error).
    fn production_shown(&self, cx: &gpui::App) -> bool {
        let s = self.store.read(cx);
        !s.missing
            && !matches!(s.project, Some(store::Load::Failed(_)))
            && production::shows_production(self.state.view, self.state.active.as_deref())
    }

    /// Whether the body shows `view` (not Setup, nor the project's error).
    fn body_shows(&self, view: StudioView, cx: &gpui::App) -> bool {
        let s = self.store.read(cx);
        !s.missing && self.state.view == view && !matches!(s.project, Some(store::Load::Failed(_)))
    }

    /// Open the media viewer on file `path` of entity `entity`'s page (its sheet or a ref): its items are the
    /// sheet, then the refs. Closing gives the pane the focus back.
    pub(crate) fn view_entity_media(&mut self, entity: &str, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let source = viewer::EntityHost::source(self.store.clone(), entity.to_owned());
        if !source.items(cx).iter().any(|i| i.id == path) {
            return;
        }
        self.menu = None;
        let media = cx.new(|cx| crate::media_viewer::MediaViewer::new(source, path, window, cx));
        let sub = self.viewer_events(&media, window, cx);
        self.pane_viewer = Some(PaneViewer { on: Viewing::Entity(entity.to_owned()), viewer: media, _sub: sub });
        cx.notify();
    }

    /// Open the media viewer on canvas `canvas`'s newest cut (the Production view's ▶ Play): its items are that
    /// cut, then the older versions. Closing gives the pane the focus back.
    pub(crate) fn view_cut(&mut self, canvas: &str, window: &mut Window, cx: &mut Context<Self>) {
        let source = viewer::CutHost::source(self.store.clone(), canvas.to_owned());
        let Some(first) = source.items(cx).first().map(|i| i.id.clone()) else { return };
        self.menu = None;
        let media = cx.new(|cx| crate::media_viewer::MediaViewer::new(source, &first, window, cx));
        let sub = self.viewer_events(&media, window, cx);
        self.pane_viewer = Some(PaneViewer { on: Viewing::Cut(canvas.to_owned()), viewer: media, _sub: sub });
        cx.notify();
    }

    /// The pane's viewer telling: closed (the pane takes the focus), faded out (dropped).
    fn viewer_events(&mut self, media: &Entity<crate::media_viewer::MediaViewer>, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe_in(media, window, |this, media, e: &crate::media_viewer::ViewerEvent, window, cx| {
            use crate::media_viewer::ViewerEvent;
            match e {
                ViewerEvent::Closed { refocus } => {
                    if *refocus {
                        window.focus(&this.focus, cx);
                    }
                    cx.notify();
                }
                ViewerEvent::Gone if this.pane_viewer.as_ref().is_some_and(|w| w.viewer == *media) => {
                    this.pane_viewer = None;
                    cx.notify();
                }
                _ => {}
            }
        })
    }

    /// Whether the pane's viewer still has its page.
    fn viewer_kept(&self, on: &Viewing, cx: &gpui::App) -> bool {
        match on {
            Viewing::Entity(entity) => {
                let world = self.body_shows(StudioView::World, cx);
                viewer::entity_viewer_kept(world, self.state.view, self.state.world_entity.as_deref(), entity)
            }
            Viewing::Cut(canvas) => {
                let has_cut = self.store.read(cx).production().is_some_and(|p| p.episodes.iter().any(|e| e.canvas == *canvas && e.cut.is_some()));
                viewer::cut_viewer_kept(self.production_shown(cx), has_cut)
            }
        }
    }

    /// The pane's viewer closes once its page is gone: the entity's (another entity, a section, a canvas), or
    /// the Production view (another view, the episode's cut gone).
    fn keep_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(w) = &self.pane_viewer else { return };
        if self.viewer_kept(&w.on, cx) {
            return;
        }
        // Dropped at once (its page is gone), so the pane takes the focus it held.
        let had = self.focus.contains_focused(window, cx);
        let Some(w) = self.pane_viewer.take() else { return };
        w.viewer.update(cx, |v, cx| v.close(window, cx));
        if had {
            window.focus(&self.focus, cx);
        }
    }

    /// The media viewer over the whole body: the pane's (the World view's, the Production view's), else the
    /// active canvas's, above the board and everything floating over it (docs/STUDIO.md §4.2, [`viewer::layers`]).
    fn render_viewer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if let Some(w) = self.pane_viewer.as_ref().filter(|w| self.viewer_kept(&w.on, cx)) {
            return Some(w.viewer.clone().into_any_element());
        }
        let s = self.store.read(cx);
        let canvas_shown = !s.missing
            && !matches!(self.state.view, StudioView::World | StudioView::Setup)
            && !production::shows_production(self.state.view, self.state.active.as_deref())
            && !matches!(s.project, Some(store::Load::Failed(_)));
        let view = self.canvas_entity(self.state.active.as_deref().filter(|_| canvas_shown)?)?.read(cx);
        let viewer = view.viewer().filter(|_| view.layers(cx).contains(&viewer::Layer::Viewer))?;
        Some(viewer.clone().into_any_element())
    }

    fn render_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let text = self.notice.clone()?;
        Some(
            div()
                .id("studio-notice")
                .flex_none()
                .px(px(12.0))
                .py(px(6.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .bg(theme::well_fill())
                .text_size(px(12.0))
                .text_color(rgb(theme::danger()))
                .child(div().flex_1().min_w_0().child(text))
                .child(div().id("studio-notice-x").cursor_pointer().child(crate::assets::icon("close", 11.0, theme::muted())).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.notice = None;
                        cx.notify();
                    },
                )))
                .into_any_element(),
        )
    }
}

/// The node `reveal` (canvas, node) holds for canvas `name`, taken; another canvas's stays.
fn take_reveal(reveal: &mut Option<(String, String)>, name: &str) -> Option<String> {
    reveal.take_if(|(c, _)| c == name).map(|(_, node)| node)
}

/// `+ Section`'s folder name for what was typed (`Old Ships!` → `old-ships`); `None` when nothing is left.
fn section_slug(name: &str) -> Option<String> {
    let slug: String = name.trim().to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let slug = slug.trim_matches('-');
    (!slug.is_empty()).then(|| slug.to_owned())
}

/// A `files.mkdir` for a section: a folder already there is the section, as wanted.
fn made_or_exists<T>(r: anyhow::Result<T>) -> anyhow::Result<()> {
    match r {
        Ok(_) => Ok(()),
        Err(e) if e.downcast_ref::<silo_proto::control::RpcError>().is_some_and(|e| e.code == silo_proto::control::RpcError::EXISTS) => Ok(()),
        Err(e) => Err(e),
    }
}

/// A quiet line in the middle of the body.
pub(crate) fn centered(text: String) -> AnyElement {
    div().size_full().flex().items_center().justify_center().p(px(24.0)).text_color(rgb(theme::muted())).child(text).into_any_element()
}

impl Render for StudioPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.keep_viewer(window, cx);
        let notice = self.render_notice(cx);
        let body = self.render_body(window, cx);
        let viewer = self.render_viewer(cx);
        let dialog = self.render_dialog(cx);
        let menu = self.menu.as_ref().map(|m| m.render("studio-menu", cx, |this: &mut Self, action, window, cx| this.pick(action, window, cx)));
        let bounds = self.body_bounds.clone();
        let probe = gpui::canvas(move |b, _, _| b, move |b, _, _, _| bounds.set(Some(b))).absolute().top_0().left_0().size_full();
        let surface = self.surface;
        let playing = self.store.read(cx).audio_playing();
        if playing {
            window.request_animation_frame();
        }
        div()
            .id("studio-pane")
            .key_context("StudioPane")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .map(|el| surface.fill(el))
            .flex()
            .flex_col()
            .text_size(px(13.0))
            .text_color(rgb(theme::foreground()))
            .on_action(cx.listener(|this, _: &act::ClosePane, window, cx| {
                if this.dialog.is_some() || this.menu.is_some() {
                    this.menu = None;
                    this.close_dialog(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !this.focus.contains_focused(window, cx) {
                        window.focus(&this.focus, cx);
                    }
                }),
            )
            .children(notice)
            .child(div().flex_1().min_h_0().relative().child(body).children(viewer))
            .children(dialog)
            .children(menu)
            .child(probe)
    }
}
