//! The Files block (docs/FILES_BLOCK.md §4): tabs in the header, a viewer
//! on the left, the folder tree on the right. Text files are edited in the
//! viewer (`editor.rs`, `edit.rs`) and saved with `files.write` on servers
//! that have it. The server holds the view state (`FilesState`: tabs, the
//! tree, scroll, highlights) and answers `files.*` calls; this pane draws
//! it, changes it with `files.set` and re-reads what `files.changed` names.
//! Unsaved text lives here only.

pub(crate) mod edit;
mod editor;
pub(crate) mod model;
#[cfg(test)]
mod tests;
mod text;
mod view;

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use gpui::{AppContext as _, Context, Entity, EventEmitter, FocusHandle, Subscription, Task, UniformListScrollHandle, Window};
use silo_proto::{
    control::{
        ContentKind, FileEntry, FileFound, FileImage, FileList, FileRead, FilesFindParams, FilesListParams, FilesReadImageParams, FilesReadParams,
        FilesSetParams, Method,
    },
    model::{BlockInfo, FileHighlight, FilesState},
};

pub(crate) use self::editor::save_all;
use self::{
    edit::EditState,
    model::{Doc, Selection, Shown},
};
use crate::text_field::{FieldEvent, TextField};

/// The tree's width to start with, and its bounds.
pub const TREE_WIDTH: f32 = 240.0;
pub const TREE_MIN: f32 = 140.0;
pub const TREE_MAX: f32 = 640.0;
/// Paths the finder asks for.
const FIND_LIMIT: u32 = 200;
/// Lines kept above a line scrolled to (as `files_show` does).
const CONTEXT_LINES: f32 = 5.0;
/// How long the view stays put before its top line is saved.
const SCROLL_SAVE: Duration = Duration::from_millis(600);

pub enum FilesPaneEvent {
    /// Focus came in: the server's focus follows.
    Activated,
    /// Something its header shows changed.
    Header,
    /// Closing these tabs would lose unsaved edits: ask first (Save / Don't
    /// Save / Cancel), then [`FilesPane::close_now`].
    Unsaved(Vec<String>),
}

/// What a tab holds once read.
pub(crate) enum Content {
    Loading,
    Failed(String),
    Text(Box<TextFile>),
    Image { image: Arc<gpui::Image>, mime: String, size: u64, dims: Option<(u32, u32)> },
    Binary { mime: String, size: u64 },
}

pub(crate) struct TextFile {
    pub doc: Rc<Doc>,
    pub shown: Shown,
    /// Highlighted lines once parsed (off the UI thread); `None` meanwhile,
    /// or for plain text.
    pub syntax: Option<crate::syntax::Lines>,
    pub truncated: bool,
    pub size: u64,
    pub markdown: Option<crate::agent::MarkdownView>,
    /// The markdown view shows an older text (edited since).
    pub markdown_stale: bool,
    pub lang: Option<crate::syntax::Language>,
    /// Bumped by every change of `doc`: a highlight for an older one is
    /// dropped.
    pub version: u64,
    /// Edits: present once the whole file is here (not truncated) and small
    /// enough to save; used when the server can write.
    pub edit: Option<EditState>,
    /// The selection while another tab is shown.
    pub cursor: Option<Selection>,
    /// The highlight waiting for typing to pause.
    highlighting: Option<Task<()>>,
}

/// The find bar (⌘F).
pub(crate) struct Find {
    pub field: Entity<TextField>,
    pub matches: Vec<(usize, std::ops::Range<usize>)>,
    pub current: Option<usize>,
    _sub: Subscription,
}

/// The ⌘P finder.
pub(crate) struct Finder {
    pub field: Entity<TextField>,
    pub found: Vec<model::Found>,
    pub selected: usize,
    pub truncated: bool,
    generation: u64,
    _sub: Subscription,
}

/// A text field bar (go to line).
pub(crate) struct Goto {
    pub field: Entity<TextField>,
    _sub: Subscription,
}

/// A press in the viewer or on the tree's edge, followed until release.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Drag {
    Select,
    /// Resizing the tree: its width when pressed, and where.
    Tree {
        width: f32,
        x: f32,
    },
}

pub struct FilesPane {
    pub block_id: String,
    rt: tokio::runtime::Handle,
    control: Option<silo_client::Control>,
    pub focus: FocusHandle,
    pub(crate) tree_focus: FocusHandle,
    pub info: Option<BlockInfo>,
    /// The view state: the server's, with local changes not confirmed yet.
    pub(crate) state: FilesState,
    /// `files.set` calls not answered yet: the server's state events wait.
    in_flight: u32,
    pub(crate) listings: HashMap<String, Vec<FileEntry>>,
    pub(crate) list_errors: HashMap<String, String>,
    pub(crate) contents: HashMap<String, Content>,
    /// Tabs whose file changed on disk since they were last shown.
    pub(crate) changed: HashSet<String>,
    /// Markdown tabs shown as source.
    pub(crate) source: HashSet<String>,
    /// Each tab's scroll position.
    pub(crate) scrolls: HashMap<String, UniformListScrollHandle>,
    /// The top line each tab last took from the state, or reported.
    applied: HashMap<String, u64>,
    save_scroll: Option<Task<()>>,
    pub(crate) tree_width: f32,
    pub(crate) tree_selected: Option<String>,
    pub(crate) tree_scroll: UniformListScrollHandle,
    pub(crate) selection: Option<Selection>,
    pub(crate) drag: Option<Drag>,
    pub(crate) find: Option<Find>,
    pub(crate) goto: Option<Goto>,
    pub(crate) finder: Option<Finder>,
    /// The viewer's text area (window coordinates), from the last paint.
    pub(crate) text_bounds: Rc<Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    /// The body's bounds, from the last paint.
    pub(crate) body_bounds: Rc<Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) metrics: Cell<text::Metrics>,
    autoscroll: Option<Task<()>>,
    pub(crate) autoscroll_speed: f32,
    pub(crate) notice: Option<String>,
    /// The caret's blink, and its timer while a caret shows.
    blink: crate::blink::Blink,
    caret_on: bool,
    blink_task: Option<Task<()>>,
    /// The last `TextFile::version` given out.
    versions: u64,
    _subs: Vec<Subscription>,
}

impl EventEmitter<FilesPaneEvent> for FilesPane {}

impl FilesPane {
    pub fn new(
        block_id: String,
        info: Option<BlockInfo>,
        rt: tokio::runtime::Handle,
        control: Option<silo_client::Control>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let tree_focus = cx.focus_handle();
        let state = info.as_ref().and_then(|i| i.files.as_deref().cloned()).unwrap_or_else(|| FilesState::new(String::new()));
        // The tree, the bars and the finder are inside `focus`'s element: focus anywhere in the block.
        let subs = vec![cx.on_focus_in(&focus, window, |_, _, cx| cx.emit(FilesPaneEvent::Activated))];
        let mut pane = Self {
            block_id,
            rt,
            control,
            focus,
            tree_focus,
            info,
            state,
            in_flight: 0,
            listings: HashMap::new(),
            list_errors: HashMap::new(),
            contents: HashMap::new(),
            changed: HashSet::new(),
            source: HashSet::new(),
            scrolls: HashMap::new(),
            applied: HashMap::new(),
            save_scroll: None,
            tree_width: TREE_WIDTH,
            tree_selected: None,
            tree_scroll: UniformListScrollHandle::new(),
            selection: None,
            drag: None,
            find: None,
            goto: None,
            finder: None,
            text_bounds: Rc::default(),
            body_bounds: Rc::default(),
            metrics: Cell::new(text::Metrics::default()),
            autoscroll: None,
            autoscroll_speed: 0.0,
            notice: None,
            blink: crate::blink::Blink::default(),
            caret_on: false,
            blink_task: None,
            versions: 0,
            _subs: subs,
        };
        // Unsaved tabs kept while the block was out of view (another session shown).
        pane.unstash(cx);
        pane.load_all(cx);
        pane
    }

    /// A new control connection: read everything again.
    pub fn set_control(&mut self, control: silo_client::Control, cx: &mut Context<Self>) {
        self.control = Some(control);
        self.in_flight = 0;
        self.load_all(cx);
    }

    /// The server's view of the block (a session view, `block.changed`).
    pub fn set_info(&mut self, info: BlockInfo, cx: &mut Context<Self>) {
        let state = info.files.as_deref().cloned();
        self.info = Some(info);
        if let Some(state) = state
            && (self.in_flight == 0 || state.root != self.state.root)
        {
            self.adopt(state, cx);
        }
    }

    /// Take `state` as the view state, loading what it newly shows.
    fn adopt(&mut self, state: FilesState, cx: &mut Context<Self>) {
        if state == self.state {
            return;
        }
        if state.root != self.state.root {
            self.listings.clear();
            self.list_errors.clear();
            self.contents.clear();
            self.changed.clear();
            self.scrolls.clear();
            self.applied.clear();
            self.selection = None;
            self.state = state;
            self.load_all(cx);
        } else {
            // A tab closed elsewhere (another window, a client) stays open here while it holds edits.
            let mut state = state;
            let kept: Vec<String> = self.unsaved().into_iter().filter(|p| !state.open.contains(p)).collect();
            state.open.extend(kept.iter().cloned());
            let active_changed = state.active != self.state.active;
            if active_changed {
                self.park_selection();
            }
            self.state = state;
            if active_changed {
                self.unpark_selection();
                self.refind(cx);
            }
            if !kept.is_empty() {
                let open = self.state.open.clone();
                self.set(FilesSetParams { open: Some(open), ..FilesSetParams::default() }, cx);
            }
            self.forget_closed();
            self.load_needed(cx);
        }
        cx.emit(FilesPaneEvent::Header);
        cx.notify();
    }

    /// Keep the shown tab's selection with it while another tab shows.
    fn park_selection(&mut self) {
        let selection = self.selection.take();
        if let Some(Content::Text(file)) = self.state.active.as_ref().and_then(|a| self.contents.get_mut(a)) {
            file.cursor = selection;
        }
    }

    /// The shown tab's selection, as it was left.
    fn unpark_selection(&mut self) {
        self.selection = match self.state.active.as_ref().and_then(|a| self.contents.get(a)) {
            Some(Content::Text(file)) => file.cursor.map(|s| Selection { anchor: file.doc.clamp(s.anchor), head: file.doc.clamp(s.head), ..s }),
            _ => None,
        };
    }

    /// Drop what belongs to tabs no longer open.
    fn forget_closed(&mut self) {
        let open: HashSet<&String> = self.state.open.iter().collect();
        self.contents.retain(|p, _| open.contains(p));
        self.changed.retain(|p| open.contains(p));
        self.scrolls.retain(|p, _| open.contains(p));
        self.applied.retain(|p, _| open.contains(p));
        self.source.retain(|p| open.contains(p));
    }

    fn load_all(&mut self, cx: &mut Context<Self>) {
        if self.state.root.is_empty() {
            return;
        }
        for dir in std::iter::once(String::new()).chain(self.state.expanded.clone()) {
            self.list(dir, cx);
        }
        if let Some(active) = self.state.active.clone() {
            self.read(active, None, cx);
        }
    }

    /// List what's expanded but not listed, and read the shown tab.
    fn load_needed(&mut self, cx: &mut Context<Self>) {
        for dir in std::iter::once(String::new()).chain(self.state.expanded.clone()) {
            if !self.listings.contains_key(&dir) && !self.list_errors.contains_key(&dir) {
                self.list(dir, cx);
            }
        }
        if let Some(active) = self.state.active.clone()
            && !self.contents.contains_key(&active)
        {
            self.read(active, None, cx);
        }
    }

    // ------------------------------------------------------------ io

    /// Call `method` on the server, then `done` with the result here.
    fn request<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        method: Method,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, anyhow::Result<T>, &mut Context<Self>) + 'static,
    ) {
        let Some(control) = self.control.clone() else { return };
        let task = self.rt.spawn(async move { control.call::<T>(method).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| done(this, result, cx));
        })
        .detach();
    }

    fn list(&mut self, dir: String, cx: &mut Context<Self>) {
        let method = Method::FilesList(FilesListParams { block_id: self.block_id.clone(), path: dir.clone() });
        let root = self.state.root.clone();
        self.request(method, cx, move |this, result: anyhow::Result<FileList>, cx| {
            if this.state.root != root {
                return;
            }
            match result {
                Ok(list) => {
                    this.list_errors.remove(&dir);
                    this.listings.insert(dir, list.entries);
                }
                Err(error) => {
                    this.listings.remove(&dir);
                    this.list_errors.insert(dir, describe(&error));
                }
            }
            cx.notify();
        });
    }

    /// Read `path` (at most `max_bytes`; the server's default when `None`).
    fn read(&mut self, path: String, max_bytes: Option<u64>, cx: &mut Context<Self>) {
        if !self.contents.contains_key(&path) {
            self.contents.insert(path.clone(), Content::Loading);
        }
        let method = Method::FilesRead(FilesReadParams { block_id: self.block_id.clone(), path: path.clone(), max_bytes });
        let root = self.state.root.clone();
        self.request(method, cx, move |this, result: anyhow::Result<FileRead>, cx| {
            if this.state.root != root || !this.state.open.contains(&path) {
                return;
            }
            match result {
                // Unsaved edits stay; the bar or a notice says what happened.
                _ if this.dirty(&path) => match result {
                    Ok(read) => this.disk_read(&path, read, cx),
                    Err(error) => this.notice = Some(format!("Couldn't check {} on disk: {}", model::name(&path), describe(&error))),
                },
                Err(error) => {
                    this.contents.insert(path, Content::Failed(describe(&error)));
                }
                Ok(read) if this.same_as_disk(&path, &read) => {}
                Ok(read) => {
                    let text = read.text.clone().filter(|_| read.kind == ContentKind::Text);
                    match model::shown_as(&path, &read.mime, text.is_some()) {
                        Shown::Image => return this.read_image(path, cx),
                        Shown::Binary => {
                            this.contents.insert(path, Content::Binary { mime: read.mime, size: read.size });
                        }
                        shown => this.set_text(path, text.unwrap_or_default(), shown, &read, cx),
                    }
                }
            }
            cx.emit(FilesPaneEvent::Header);
            cx.notify();
        });
    }

    fn set_text(&mut self, path: String, text: String, shown: Shown, read: &FileRead, cx: &mut Context<Self>) {
        let (truncated, size) = (read.truncated, read.size);
        // A re-read keeps the rendered markdown's place, the old colours until the new ones are ready, and
        // where the caret was.
        let (markdown, syntax, cursor) = match self.contents.remove(&path) {
            Some(Content::Text(old)) => (old.markdown, old.syntax.filter(|_| old.doc.text.len() == text.len()), old.cursor),
            _ => (None, None, None),
        };
        let markdown = (shown == Shown::Markdown).then(|| match markdown {
            Some(mut view) => {
                view.set_source(&text);
                view
            }
            None => crate::agent::MarkdownView::new(&text),
        });
        let lang = crate::syntax::Language::detect(&path, text.lines().next());
        let doc = Rc::new(Doc::new(text));
        let edit = editor::editable_read(read).map(|base| EditState::new(base, &doc));
        // A re-read keeps the selection while its lines are still there.
        if self.state.active.as_deref() == Some(path.as_str()) && self.selection.is_some_and(|s| s.anchor.line.max(s.head.line) >= doc.line_count()) {
            self.selection = None;
        }
        self.versions += 1;
        let file = TextFile {
            doc,
            shown,
            syntax,
            truncated,
            size,
            markdown,
            markdown_stale: false,
            lang,
            version: self.versions,
            edit,
            cursor,
            highlighting: None,
        };
        self.contents.insert(path.clone(), Content::Text(Box::new(file)));
        self.rehighlight(&path, Duration::ZERO, cx);
        if self.state.active.as_deref() == Some(path.as_str()) {
            self.refind(cx);
        }
    }

    fn read_image(&mut self, path: String, cx: &mut Context<Self>) {
        let method = Method::FilesReadImage(FilesReadImageParams { block_id: self.block_id.clone(), path: path.clone() });
        self.request(method, cx, move |this, result: anyhow::Result<FileImage>, cx| {
            let content = match result.and_then(|image| {
                let bytes = data_encoding::BASE64.decode(image.data_base64.as_bytes())?;
                Ok((image.mime, image.size, bytes))
            }) {
                Ok((mime, size, bytes)) => match gpui::ImageFormat::from_mime_type(&mime) {
                    Some(format) => {
                        let dims = image_dims(&bytes);
                        Content::Image { image: Arc::new(gpui::Image::from_bytes(format, bytes)), mime, size, dims }
                    }
                    None => Content::Binary { mime, size },
                },
                Err(error) => Content::Failed(describe(&error)),
            };
            if this.state.open.contains(&path) {
                this.contents.insert(path, content);
                cx.notify();
            }
        });
    }

    /// Send `params` (the block id filled in), having applied it here.
    fn set(&mut self, params: FilesSetParams, cx: &mut Context<Self>) {
        if self.control.is_none() {
            return;
        }
        self.in_flight += 1;
        let params = FilesSetParams { block_id: self.block_id.clone(), ..params };
        self.request(Method::FilesSet(params), cx, |this, result: anyhow::Result<FilesState>, cx| {
            this.in_flight = this.in_flight.saturating_sub(1);
            match result {
                Ok(state) if this.in_flight == 0 => this.adopt(state, cx),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("files.set: {error:#}");
                    this.notice = Some(describe(&error));
                    cx.notify();
                }
            }
        });
    }

    /// `files.changed`: read open files again and list changed folders again.
    pub fn files_changed(&mut self, paths: &[String], truncated: bool, cx: &mut Context<Self>) {
        let listed: Vec<String> = self.listings.keys().cloned().collect();
        let (reread, relist) = model::affected(paths, truncated, &self.state.open, listed.iter());
        for dir in relist {
            self.list(dir, cx);
        }
        for path in reread {
            if self.saving(&path) {
                // Its own save, likely: looked at once the save is answered.
                self.recheck(&path);
            } else if self.state.active.as_deref() == Some(path.as_str()) || self.dirty(&path) {
                // Shown, or holding edits: read it again (keeping how much was loaded). A clean tab takes
                // the new text; edits stay and the bar asks.
                let all = matches!(self.contents.get(&path), Some(Content::Text(t)) if !t.truncated && t.size > FileRead::DEFAULT_BYTES);
                self.read(path, all.then_some(FileRead::MAX_BYTES), cx);
            } else {
                self.contents.remove(&path);
                self.changed.insert(path);
            }
        }
        cx.emit(FilesPaneEvent::Header);
        cx.notify();
    }

    // ------------------------------------------------------------ tabs

    pub(crate) fn active(&self) -> Option<&str> {
        self.state.active.as_deref()
    }

    /// Show `path`, opening a tab for it; `show: false` opens it behind.
    pub fn open(&mut self, path: &str, show: bool, cx: &mut Context<Self>) {
        let mut open = self.state.open.clone();
        if !open.iter().any(|p| p == path) {
            if open.len() >= FilesState::OPEN_MAX {
                self.notice = Some(format!("At most {} tabs: close some first", FilesState::OPEN_MAX));
                return cx.notify();
            }
            open.push(path.to_owned());
        }
        self.save_scroll_now(cx);
        self.state.open.clone_from(&open);
        if show && self.state.active.as_deref() != Some(path) {
            self.park_selection();
            self.state.active = Some(path.to_owned());
            self.unpark_selection();
        }
        if show {
            self.changed.remove(path);
            self.refind(cx);
        }
        let params = FilesSetParams { open: Some(open), active: show.then(|| path.to_owned()), ..FilesSetParams::default() };
        self.set(params, cx);
        self.load_needed(cx);
        cx.emit(FilesPaneEvent::Header);
        cx.notify();
    }

    /// Close tab `path`; with unsaved edits, ask first.
    pub(crate) fn close(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.dirty(path) {
            cx.emit(FilesPaneEvent::Unsaved(vec![path.to_owned()]));
            return;
        }
        self.close_now(path, cx);
    }

    /// Close tab `path`, dropping any unsaved edits.
    pub fn close_now(&mut self, path: &str, cx: &mut Context<Self>) {
        let (open, active) = model::close_tab(&self.state.open, self.active(), path);
        if open == self.state.open {
            return;
        }
        if self.active() == Some(path) {
            self.selection = None;
        }
        self.state.open.clone_from(&open);
        self.state.highlights.remove(path);
        let shown_changed = active != self.state.active;
        self.state.active.clone_from(&active);
        self.forget_closed();
        if shown_changed {
            if let Some(a) = &active {
                self.changed.remove(a);
            }
            self.unpark_selection();
            self.refind(cx);
        }
        let params = FilesSetParams { open: Some(open), active: Some(active.unwrap_or_default()), ..FilesSetParams::default() };
        self.set(params, cx);
        self.load_needed(cx);
        cx.emit(FilesPaneEvent::Header);
        cx.notify();
    }

    pub(crate) fn step_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(next) = model::step_tab(&self.state.open, self.active(), delta) {
            self.open(&next, true, cx);
        }
    }

    pub(crate) fn toggle_tree(&mut self, cx: &mut Context<Self>) {
        self.state.tree = !self.state.tree;
        let tree = self.state.tree;
        self.set(FilesSetParams { tree: Some(tree), ..FilesSetParams::default() }, cx);
        cx.emit(FilesPaneEvent::Header);
        cx.notify();
    }

    pub(crate) fn toggle_source(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.state.active.clone() else { return };
        if !self.source.remove(&path) {
            self.source.insert(path);
        }
        self.selection = None;
        cx.emit(FilesPaneEvent::Header);
        cx.notify();
    }

    // ------------------------------------------------------------ tree

    pub(crate) fn tree_rows(&self) -> Vec<model::TreeRow> {
        let expanded: HashSet<String> = self.state.expanded.iter().cloned().collect();
        model::tree_rows(&self.listings, &expanded)
    }

    pub(crate) fn set_expanded(&mut self, dir: &str, open: bool, cx: &mut Context<Self>) {
        let expanded = if open {
            let mut e = self.state.expanded.clone();
            if !e.iter().any(|p| p == dir) {
                if e.len() >= FilesState::EXPANDED_MAX {
                    e.remove(0);
                }
                e.push(dir.to_owned());
            }
            e
        } else {
            model::collapse(&self.state.expanded, dir)
        };
        if expanded == self.state.expanded {
            return;
        }
        self.state.expanded.clone_from(&expanded);
        self.set(FilesSetParams { expanded: Some(expanded), ..FilesSetParams::default() }, cx);
        if open && !self.listings.contains_key(dir) {
            self.list(dir.to_owned(), cx);
        }
        cx.notify();
    }

    /// A click on tree row `row`: a folder opens or closes, a file shows
    /// (`behind`: opens without showing, ⌘-click).
    pub(crate) fn tree_click(&mut self, row: &model::TreeRow, behind: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.tree_selected = Some(row.path.clone());
        window.focus(&self.tree_focus, cx);
        if row.dir {
            self.set_expanded(&row.path, !row.expanded, cx);
        } else {
            self.open(&row.path, !behind, cx);
        }
        cx.notify();
    }

    pub(crate) fn tree_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let rows = self.tree_rows();
        let at = self.tree_selected.as_ref().and_then(|s| rows.iter().position(|r| &r.path == s));
        if key == "enter" {
            if let Some(row) = at.map(|i| rows[i].clone()) {
                if row.dir {
                    self.set_expanded(&row.path, !row.expanded, cx);
                } else {
                    self.open(&row.path, true, cx);
                    window.focus(&self.focus, cx);
                }
            }
            return true;
        }
        match model::tree_key(&rows, at, key) {
            Some(model::TreeMove::Select(i)) => {
                self.tree_selected = Some(rows[i].path.clone());
                self.tree_scroll.scroll_to_item(i, gpui::ScrollStrategy::Nearest);
                cx.notify();
                true
            }
            Some(model::TreeMove::Toggle(dir, open)) => {
                self.set_expanded(&dir, open, cx);
                true
            }
            None => false,
        }
    }

    // ------------------------------------------------------------ scroll

    pub(crate) fn scroll_handle(&mut self, path: &str) -> UniformListScrollHandle {
        self.scrolls.entry(path.to_owned()).or_default().clone()
    }

    /// The shown tab's top line as the state has it, applied once per change.
    pub(crate) fn wanted_top(&mut self, path: &str) -> Option<u64> {
        let want = self.state.scroll.get(path).copied()?;
        (self.applied.get(path) != Some(&want)).then(|| {
            self.applied.insert(path.to_owned(), want);
            want
        })
    }

    /// The shown tab's top line moved to `top`: saved once it stays put.
    pub(crate) fn scrolled(&mut self, path: &str, top: u64, cx: &mut Context<Self>) {
        let saved = self.applied.get(path).copied().or_else(|| Some(self.state.scroll.get(path).copied().unwrap_or(0)));
        if saved == Some(top) {
            self.save_scroll = None;
            return;
        }
        let path = path.to_owned();
        self.save_scroll = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SCROLL_SAVE).await;
            let _ = this.update(cx, |this, cx| this.save_scroll_of(&path, top, cx));
        }));
    }

    fn save_scroll_of(&mut self, path: &str, top: u64, cx: &mut Context<Self>) {
        if !self.state.open.iter().any(|p| p == path) || self.applied.get(path) == Some(&top) {
            return;
        }
        self.applied.insert(path.to_owned(), top);
        self.state.scroll.insert(path.to_owned(), top);
        self.set(FilesSetParams { scroll: Some([(path.to_owned(), top)].into()), ..FilesSetParams::default() }, cx);
    }

    /// Before leaving a tab: save where it is now.
    fn save_scroll_now(&mut self, cx: &mut Context<Self>) {
        self.save_scroll = None;
        let Some(path) = self.state.active.clone() else { return };
        let Some(top) = self.top_line(&path) else { return };
        self.save_scroll_of(&path, top, cx);
    }

    /// The first line `path`'s viewer shows.
    pub(crate) fn top_line(&self, path: &str) -> Option<u64> {
        let handle = self.scrolls.get(path)?;
        let offset = handle.0.borrow().base_handle.offset();
        let lh = self.metrics.get().line;
        (lh > 0.0).then(|| (-f32::from(offset.y) / lh).max(0.0).round() as u64)
    }

    /// Scroll the shown text so line `line` (0-based) is in view, near the top
    /// with a few lines above it when it was out of view.
    pub(crate) fn reveal_line(&mut self, line: usize, cx: &mut Context<Self>) {
        let Some(path) = self.state.active.clone() else { return };
        let handle = self.scroll_handle(&path);
        let lh = self.metrics.get().line;
        let height = self.text_bounds.get().map_or(400.0, |b| f32::from(b.size.height));
        let offset = handle.0.borrow().base_handle.offset();
        #[allow(clippy::cast_precision_loss)]
        let (y, top) = (line as f32 * lh, -f32::from(offset.y));
        if y < top || y + lh > top + height {
            let lines = self.text_lines(&path);
            #[allow(clippy::cast_precision_loss)]
            let max = (lines as f32 * lh - height).max(0.0);
            let target = (y - lh * CONTEXT_LINES).clamp(0.0, max);
            handle.0.borrow().base_handle.set_offset(gpui::point(offset.x, gpui::px(-target)));
        }
        cx.notify();
    }

    fn text_lines(&self, path: &str) -> usize {
        match self.contents.get(path) {
            Some(Content::Text(t)) => t.doc.line_count(),
            _ => 0,
        }
    }

    // ------------------------------------------------------------ bars

    pub(crate) fn open_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(finder) = &self.finder {
            let focus = finder.field.read(cx).focus.clone();
            return focus.focus(window, cx);
        }
        let field = cx.new(|cx| TextField::new("Find a file by name or path", cx));
        let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| this.finder_event(event, window, cx));
        let focus = field.read(cx).focus.clone();
        self.finder = Some(Finder { field, found: Vec::new(), selected: 0, truncated: false, generation: 0, _sub: sub });
        self.finder_query(String::new(), cx);
        focus.focus(window, cx);
        cx.notify();
    }

    fn finder_query(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(finder) = self.finder.as_mut() else { return };
        finder.generation += 1;
        let generation = finder.generation;
        let method = Method::FilesFind(FilesFindParams { block_id: self.block_id.clone(), query: query.clone(), limit: Some(FIND_LIMIT) });
        self.request(method, cx, move |this, result: anyhow::Result<FileFound>, cx| {
            let open = this.state.open.clone();
            let Some(finder) = this.finder.as_mut().filter(|f| f.generation == generation) else { return };
            match result {
                Ok(found) => {
                    finder.truncated = found.truncated;
                    finder.found = model::rank(&query, found.paths, &open);
                }
                Err(error) => {
                    finder.found.clear();
                    this.notice = Some(describe(&error));
                }
            }
            if let Some(finder) = this.finder.as_mut() {
                finder.selected = 0;
            }
            cx.notify();
        });
    }

    fn finder_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(finder) = self.finder.as_mut() else { return };
        let len = finder.found.len();
        match event {
            FieldEvent::Changed(text) => {
                let text = text.clone();
                self.finder_query(text, cx);
            }
            FieldEvent::Up => finder.selected = crate::launcher::step(finder.selected, len, -1),
            FieldEvent::Down => finder.selected = crate::launcher::step(finder.selected, len, 1),
            FieldEvent::Cancel => self.close_bars(window, cx),
            FieldEvent::Submit => {
                let index = finder.selected;
                self.finder_pick(index, window, cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn finder_pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.finder.as_ref().and_then(|f| f.found.get(index)).map(|f| f.path.clone()) else { return };
        self.finder = None;
        self.open(&path, true, cx);
        self.tree_selected = Some(path.clone());
        let expanded = model::reveal(&self.state.expanded, &path);
        if expanded != self.state.expanded {
            let unlisted: Vec<String> = expanded.iter().filter(|d| !self.listings.contains_key(*d)).cloned().collect();
            for dir in unlisted {
                self.list(dir, cx);
            }
            self.state.expanded.clone_from(&expanded);
            self.set(FilesSetParams { expanded: Some(expanded), ..FilesSetParams::default() }, cx);
        }
        window.focus(&self.focus, cx);
    }

    pub(crate) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.goto = None;
        if let Some(find) = &self.find {
            let focus = find.field.read(cx).focus.clone();
            find.field.update(cx, |f, cx| f.select_all(cx));
            return focus.focus(window, cx);
        }
        let field = cx.new(|cx| TextField::new("Find in file", cx));
        // A selection on one line starts the search.
        let seed = self.selected_text().filter(|t| !t.is_empty() && !t.contains('\n'));
        let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| this.find_event(event, window, cx));
        let focus = field.read(cx).focus.clone();
        self.find = Some(Find { field: field.clone(), matches: Vec::new(), current: None, _sub: sub });
        if let Some(seed) = seed {
            field.update(cx, |f, cx| {
                f.set_text(seed, cx);
                f.select_all(cx);
            });
        }
        focus.focus(window, cx);
        cx.notify();
    }

    /// Search the shown text again for the find bar's query (another tab,
    /// the file changed), staying where the view is.
    fn refind(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_ref() else { return };
        let query = find.field.read(cx).text.clone();
        let doc = match self.state.active.as_ref().and_then(|a| self.contents.get(a)) {
            Some(Content::Text(file)) => Some(file.doc.clone()),
            _ => None,
        };
        let Some(find) = self.find.as_mut() else { return };
        find.matches = doc.map(|d| model::find_all(&d, &query)).unwrap_or_default();
        find.current = find.current.filter(|&c| c < find.matches.len());
    }

    fn find_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            FieldEvent::Changed(text) => {
                let text = text.clone();
                self.search(&text, cx);
            }
            FieldEvent::Down | FieldEvent::Submit => self.find_step(1, cx),
            FieldEvent::Up => self.find_step(-1, cx),
            FieldEvent::Cancel => self.close_bars(window, cx),
        }
        cx.notify();
    }

    /// Find `query` in the shown text; the first match from the top line shows.
    fn search(&mut self, query: &str, cx: &mut Context<Self>) {
        let top = self.state.active.as_deref().and_then(|a| self.top_line(a)).unwrap_or(0);
        let doc = match self.state.active.as_ref().and_then(|a| self.contents.get(a)) {
            Some(Content::Text(file)) => Some(file.doc.clone()),
            _ => None,
        };
        let Some(find) = self.find.as_mut() else { return };
        find.matches = doc.map(|d| model::find_all(&d, query)).unwrap_or_default();
        #[allow(clippy::cast_possible_truncation)]
        let from = top as usize;
        find.current = model::first_from(&find.matches, from);
        if let Some(line) = find.current.map(|i| find.matches[i].0) {
            self.reveal_line(line, cx);
        }
    }

    pub(crate) fn find_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else { return };
        if find.matches.is_empty() {
            return;
        }
        let at = find.current.map_or(0, |c| crate::launcher::step(c, find.matches.len(), delta));
        find.current = Some(at);
        let line = find.matches[at].0;
        self.reveal_line(line, cx);
    }

    pub(crate) fn open_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.state.active.as_ref().and_then(|a| self.contents.get(a)), Some(Content::Text(_))) {
            return;
        }
        self.find = None;
        if let Some(goto) = &self.goto {
            let focus = goto.field.read(cx).focus.clone();
            return focus.focus(window, cx);
        }
        let field = cx.new(|cx| TextField::new("Go to line (line or line:column)", cx));
        let sub = cx.subscribe_in(&field, window, |this, field, event: &FieldEvent, window, cx| match event {
            FieldEvent::Submit => {
                let text = field.read(cx).text.clone();
                if let Some((line, col)) = model::parse_goto(&text) {
                    this.goto_line(line, col, window, cx);
                }
            }
            FieldEvent::Cancel => this.close_bars(window, cx),
            _ => {}
        });
        let focus = field.read(cx).focus.clone();
        self.goto = Some(Goto { field, _sub: sub });
        focus.focus(window, cx);
        cx.notify();
    }

    fn goto_line(&mut self, line: usize, col: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.state.active.clone() else { return };
        let lines = self.text_lines(&path);
        let line = line.min(lines.saturating_sub(1));
        self.goto = None;
        let at = model::Pos { line, col: col.unwrap_or(0) };
        self.selection = Some(Selection { anchor: at, head: at, unit: model::Unit::Char });
        self.reveal_line(line, cx);
        window.focus(&self.focus, cx);
    }

    /// Esc: close the finder, find and go-to bars, back to the viewer.
    pub(crate) fn close_bars(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.finder = None;
        self.find = None;
        self.goto = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    // ------------------------------------------------------------ selection

    pub(crate) fn shown_doc(&self) -> Option<Rc<Doc>> {
        let path = self.state.active.as_ref()?;
        if self.source_hidden(path) {
            return None;
        }
        match self.contents.get(path)? {
            Content::Text(file) => Some(file.doc.clone()),
            _ => None,
        }
    }

    /// Markdown shown rendered (its text isn't selectable here).
    pub(crate) fn source_hidden(&self, path: &str) -> bool {
        matches!(self.contents.get(path), Some(Content::Text(f)) if f.shown == Shown::Markdown && !self.source.contains(path))
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        let doc = self.shown_doc()?;
        self.selection.as_ref().filter(|s| !s.is_empty(&doc)).map(|s| s.text(&doc))
    }

    pub(crate) fn select_all(&mut self, cx: &mut Context<Self>) {
        if let Some(doc) = self.shown_doc() {
            self.selection = Some(Selection { anchor: model::Pos::default(), head: doc.end(), unit: model::Unit::Char });
            cx.notify();
        }
    }

    /// Keep scrolling while a selection drag is past the top or bottom.
    pub(crate) fn run_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.autoscroll.is_some() {
            return;
        }
        self.autoscroll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                let go = this.update(cx, |this, cx| this.autoscroll_tick(cx)).unwrap_or(false);
                if !go {
                    break;
                }
            }
            let _ = this.update(cx, |this, _| this.autoscroll = None);
        }));
    }

    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if self.drag != Some(Drag::Select) || self.autoscroll_speed == 0.0 {
            return false;
        }
        let Some(path) = self.state.active.clone() else { return false };
        let handle = self.scroll_handle(&path);
        let base = handle.0.borrow().base_handle.clone();
        let (offset, max) = (base.offset(), base.max_offset());
        let y = (f32::from(offset.y) - self.autoscroll_speed).clamp(-f32::from(max.y), 0.0);
        base.set_offset(gpui::point(offset.x, gpui::px(y)));
        // The selection follows to the edge line.
        if let (Some(doc), Some(bounds), Some(sel)) = (self.shown_doc(), self.text_bounds.get(), self.selection.as_mut()) {
            let edge = if self.autoscroll_speed < 0.0 { bounds.top() } else { bounds.bottom() - gpui::px(1.0) };
            let m = self.metrics.get();
            let line = text::line_at(f32::from(edge), f32::from(bounds.top()), y, m.line, doc.line_count());
            sel.head = model::Pos { line, col: if self.autoscroll_speed < 0.0 { 0 } else { doc.line(line).len() } };
        }
        cx.notify();
        true
    }
}

/// A failed call, for people: the server's message, else the error chain.
pub(crate) fn describe(error: &anyhow::Error) -> String {
    match error.downcast_ref::<silo_proto::control::RpcError>() {
        Some(rpc) => rpc.message.clone(),
        None => format!("{error:#}"),
    }
}

/// An image's pixel size from its header, when the decoder knows the format.
fn image_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok()
}

/// Highlights for line `line` (1-based) of the shown tab.
pub(crate) fn marks_of<'a>(state: &'a FilesState, path: &str) -> &'a [FileHighlight] {
    state.highlights.get(path).map_or(&[], Vec::as_slice)
}
