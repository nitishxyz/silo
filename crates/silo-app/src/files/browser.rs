//! The Files block's browser (docs/FILES_BLOCK.md §8): its state in the
//! pane and what it does — navigation, search, file operations through
//! the server (`files.mkdir/rename/copy/move/trash/search`), uploads into a
//! folder, the file clipboard, Quick Look, and drags in, within and out of
//! the block. Drawing is `browser_view.rs`; the pure parts are `browse.rs`.

use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{AppContext as _, Context, Entity, FocusHandle, Subscription, Task, UniformListScrollHandle, Window};
use silo_proto::{
    compat::feature,
    control::{
        ContentKind, FileImage, FileRead, FileSearch, FileWritten, FilesCreateParams, FilesDone, FilesMkdirParams, FilesReadImageParams,
        FilesReadParams, FilesRenameParams, FilesSearchParams, FilesSetParams, FilesTransferParams, FilesTrashParams, Method,
    },
    model::FilesView,
};

use super::{
    Content, FilesPane, FilesPaneEvent,
    browse::{self, History, Picked, Row, Sort, SortKey},
    describe, model,
};
use crate::text_field::{FieldEvent, TextField};

/// How long typed letters gather into one type-to-select prefix.
const TYPE_PAUSE: Duration = Duration::from_millis(900);
/// Bytes Quick Look reads of a text file.
const PREVIEW_BYTES: u64 = 256 * 1024;

/// The search field and its latest answer.
pub(crate) struct BrowseSearch {
    pub field: Entity<TextField>,
    pub query: String,
    pub hits: Vec<silo_proto::control::FileHit>,
    pub truncated: bool,
    pub searching: bool,
    generation: u64,
    _sub: Subscription,
}

/// An item being renamed in place.
pub(crate) struct Rename {
    pub path: String,
    pub field: Entity<TextField>,
    _sub: Subscription,
}

/// The context menu: where it opened (window coordinates) and on what
/// (`None`: the folder's background).
pub(crate) struct Menu {
    pub at: gpui::Point<gpui::Pixels>,
    pub target: Option<String>,
}

/// Quick Look (Space).
pub(crate) struct Preview {
    pub path: String,
    pub content: Content,
}

/// An upload into the block, shown with its progress.
pub(crate) struct Transfer {
    pub label: String,
    pub progress: Arc<(AtomicU64, AtomicU64)>,
    pub abort: tokio::task::AbortHandle,
    _task: Task<()>,
}

pub(crate) struct Browser {
    pub picked: Picked,
    pub history: History,
    pub sort: Sort,
    pub hidden: bool,
    pub scroll: UniformListScrollHandle,
    pub focus: FocusHandle,
    pub search: Option<BrowseSearch>,
    pub rename: Option<Rename>,
    /// An item just made, renamed in place at the next frame.
    pub rename_next: Option<String>,
    pub menu: Option<Menu>,
    pub preview: Option<Preview>,
    /// Items ⌘C copied, and the text it put on the clipboard (a paste
    /// whose clipboard no longer holds it pastes what's there instead).
    pub clipboard: Vec<String>,
    clip_text: Option<String>,
    pub transfer: Option<Transfer>,
    typed: (String, Option<Instant>),
    /// Tests: act as if the host had `files_ops`.
    #[cfg(test)]
    pub test_ops: bool,
    /// Bumped by whatever changes the rows: listings, expanded folders,
    /// sort, hidden items, search hits.
    generation: Cell<u64>,
    rows: RefCell<Option<(u64, Rc<Vec<Row>>)>>,
}

impl Browser {
    pub fn new(focus: FocusHandle) -> Self {
        Self {
            picked: Picked::default(),
            history: History::default(),
            sort: Sort::default(),
            hidden: false,
            scroll: UniformListScrollHandle::new(),
            focus,
            search: None,
            rename: None,
            rename_next: None,
            menu: None,
            preview: None,
            clipboard: Vec::new(),
            clip_text: None,
            transfer: None,
            typed: (String::new(), None),
            #[cfg(test)]
            test_ops: false,
            generation: Cell::new(0),
            rows: RefCell::new(None),
        }
    }

    /// The rows are stale.
    pub fn dirty(&self) {
        self.generation.set(self.generation.get() + 1);
    }

    /// Search hits shown instead of the folder.
    pub fn searching(&self) -> bool {
        self.search.as_ref().is_some_and(|s| !s.query.trim().is_empty())
    }
}

/// A drag of items of a Files block: within it (onto a folder: move, ⌥
/// copy), and out of the window (the local host's files, see
/// [`FilesPane::external_drag`]).
#[derive(Clone)]
pub(crate) struct DraggedFiles {
    pub block: String,
    pub paths: Rc<Vec<String>>,
    /// The block's root on this Mac, when its host is this Mac: what a drag
    /// out of the window hands to the platform.
    pub local_root: Option<PathBuf>,
}

impl DraggedFiles {
    pub fn label(&self) -> String {
        match self.paths.as_slice() {
            [one] => model::name(one).to_owned(),
            many => format!("{} items", many.len()),
        }
    }

    /// The platform payload when the drag leaves the window: the local
    /// host's paths; `None` for a remote host (in-app only for now, see
    /// [`DragOutItem`]).
    pub fn external(&self) -> Option<gpui::ExternalDragPayload> {
        let root = self.local_root.as_ref()?;
        let entries = self.paths.iter().map(|p| {
            let path = root.join(p);
            let dir = path.is_dir();
            (path, dir)
        });
        Some(gpui::ExternalDragPayload::Files(gpui::FileDragPaths::new(entries)))
    }
}

/// The drag preview: the item's name, or how many.
pub(crate) struct DragPreview {
    pub label: String,
    pub icon: &'static str,
}

/// An item a drag out of a remote host's block would offer the platform
/// (a promised file, fetched only once dropped).
///
/// TODO(gpui drag-out): our gpui fork offers real paths only
/// (`ExternalDragPayload::Files`, used for this Mac's host). A promised-file
/// payload is being added to the fork; once it is there, a remote block's
/// rows attach `external_drag_payload` with these items and, for each
/// item the platform asks for, run [`DragOutFetch::fetch`] into the
/// destination folder it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragOutItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Relative to the block's root.
    pub rel_path: String,
}

/// Downloads a dragged-out item from the block's host (the download stream,
/// `silo_client::download`).
#[derive(Clone)]
pub(crate) struct DragOutFetch {
    control: silo_client::Control,
    block_id: String,
    rt: tokio::runtime::Handle,
}

impl DragOutFetch {
    /// Download `item` into local folder `dest_dir` (recursively for a
    /// folder; a taken name gets `name 2`); resolves to where it landed.
    /// Dropping the handle's task cancels it, removing what was written.
    #[allow(dead_code, reason = "the seam for gpui's promised-file drags (see DragOutItem)")]
    pub fn fetch(&self, item: DragOutItem, dest_dir: PathBuf) -> tokio::task::JoinHandle<anyhow::Result<PathBuf>> {
        let (control, block) = (self.control.clone(), self.block_id.clone());
        self.rt.spawn(async move {
            let got = silo_client::download(&control, &block, vec![item.rel_path.clone()], &dest_dir, |_, _| {}).await?;
            got.into_iter().next().ok_or_else(|| anyhow::anyhow!("{} was not downloaded", item.name))
        })
    }
}

impl FilesPane {
    /// The host browses and manages files (`files_ops`).
    pub(crate) fn ops(&self) -> bool {
        #[cfg(test)]
        if self.browser.test_ops {
            return true;
        }
        self.control.as_ref().is_some_and(|c| c.peer_supports(feature::FILES_OPS))
    }

    /// The browser is shown.
    pub(crate) fn browsing(&self) -> bool {
        self.state.view == FilesView::Browse && self.ops()
    }

    /// The list's rows (cached until something changes them).
    pub(crate) fn browse_rows(&self) -> Rc<Vec<Row>> {
        let generation = self.browser.generation.get();
        if let Some((g, rows)) = &*self.browser.rows.borrow()
            && *g == generation
        {
            return rows.clone();
        }
        let rows = Rc::new(match &self.browser.search {
            Some(search) if self.browser.searching() => browse::search_rows(&search.hits, self.browser.sort, self.browser.hidden),
            _ => {
                let expanded: HashSet<String> = self.state.expanded.iter().cloned().collect();
                browse::rows(&self.listings, &self.state.browse, &expanded, self.browser.sort, self.browser.hidden)
            }
        });
        *self.browser.rows.borrow_mut() = Some((generation, rows.clone()));
        rows
    }

    /// Editor or browser, kept with the block.
    pub(crate) fn set_view(&mut self, view: FilesView, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.view == view {
            return;
        }
        self.state.view = view;
        self.set(FilesSetParams { view: Some(view), ..FilesSetParams::default() }, cx);
        if view == FilesView::Browse {
            self.load_needed(cx);
            self.browser.dirty();
            window.focus(&self.browser.focus, cx);
        } else {
            self.browser.menu = None;
            self.browser.preview = None;
            window.focus(&self.focus, cx);
        }
        cx.emit(FilesPaneEvent::Header);
        cx.notify();
    }

    /// Show folder `dir`; `remember`: ⌘[ comes back.
    pub(crate) fn navigate(&mut self, dir: String, remember: bool, cx: &mut Context<Self>) {
        if self.browser.searching() {
            self.browser.search = None;
        }
        if dir == self.state.browse {
            self.browser.dirty();
            return cx.notify();
        }
        if remember {
            let from = self.state.browse.clone();
            self.browser.history.went(&from);
        }
        let from = std::mem::replace(&mut self.state.browse, dir.clone());
        self.browser.picked.clear();
        // Coming up out of a folder selects it, as Finder does.
        if from.starts_with(&format!("{dir}/")) || (dir.is_empty() && !from.is_empty()) {
            let child = from.split('/').take(dir.split('/').filter(|p| !p.is_empty()).count() + 1).collect::<Vec<_>>().join("/");
            self.browser.picked.only(&child);
        }
        self.browser.rename = None;
        self.browser.menu = None;
        self.set(FilesSetParams { browse: Some(dir.clone()), ..FilesSetParams::default() }, cx);
        if !self.listings.contains_key(&dir) {
            self.list(dir, cx);
        }
        self.browser.dirty();
        self.browser.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    pub(crate) fn go_parent(&mut self, cx: &mut Context<Self>) {
        if self.browser.searching() {
            return self.clear_search(cx);
        }
        if self.state.browse.is_empty() {
            return;
        }
        let up = model::parent(&self.state.browse).to_owned();
        self.navigate(up, true, cx);
    }

    pub(crate) fn go_back(&mut self, cx: &mut Context<Self>) {
        let current = self.state.browse.clone();
        if let Some(to) = self.browser.history.back(&current) {
            self.navigate(to, false, cx);
        }
    }

    pub(crate) fn go_forward(&mut self, cx: &mut Context<Self>) {
        let current = self.state.browse.clone();
        if let Some(to) = self.browser.history.forward(&current) {
            self.navigate(to, false, cx);
        }
    }

    /// Open `row`: a folder is shown, a file opens in the editor.
    pub(crate) fn open_row(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        if row.dir {
            self.navigate(row.path.clone(), true, cx);
        } else {
            self.set_view(FilesView::Edit, window, cx);
            self.open(&row.path, true, cx);
            self.tree_selected = Some(row.path.clone());
        }
    }

    /// ⌘↓ / double-click on the selection: one folder is shown; files open.
    pub(crate) fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.browse_rows();
        let picked = self.browser.picked.ordered(&rows);
        let picked: Vec<&Row> = rows.iter().filter(|r| picked.contains(&r.path)).collect();
        if let [row] = picked.as_slice() {
            let row = (*row).clone();
            return self.open_row(&row, window, cx);
        }
        let files: Vec<String> = picked.iter().filter(|r| !r.dir).map(|r| r.path.clone()).collect();
        if !files.is_empty() {
            self.set_view(FilesView::Edit, window, cx);
            for (i, path) in files.iter().enumerate() {
                self.open(path, i + 1 == files.len(), cx);
            }
        }
    }

    pub(crate) fn toggle_folder(&mut self, path: &str, cx: &mut Context<Self>) {
        let open = !self.state.expanded.iter().any(|p| p == path);
        self.set_expanded(path, open, cx);
        self.browser.dirty();
    }

    pub(crate) fn set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.browser.sort = self.browser.sort.clicked(key);
        self.browser.dirty();
        cx.notify();
    }

    pub(crate) fn toggle_folders_first(&mut self, cx: &mut Context<Self>) {
        self.browser.sort.folders_first = !self.browser.sort.folders_first;
        self.browser.dirty();
        cx.notify();
    }

    pub(crate) fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.browser.hidden = !self.browser.hidden;
        self.browser.dirty();
        let rows = self.browse_rows();
        self.browser.picked.retain_listed(&rows);
        cx.notify();
    }

    // ------------------------------------------------------------ selection

    pub(crate) fn browse_click(&mut self, at: usize, cmd: bool, shift: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.browse_rows();
        self.browser.picked.click(&rows, at, cmd, shift);
        self.browser.menu = None;
        self.commit_rename(cx);
        window.focus(&self.browser.focus, cx);
        self.follow_preview(cx);
        cx.notify();
    }

    pub(crate) fn browse_step(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let rows = self.browse_rows();
        if let Some(at) = self.browser.picked.step(&rows, delta, extend) {
            self.browser.scroll.scroll_to_item(at, gpui::ScrollStrategy::Nearest);
        }
        self.follow_preview(cx);
        cx.notify();
    }

    pub(crate) fn browse_select_all(&mut self, cx: &mut Context<Self>) {
        let rows = self.browse_rows();
        self.browser.picked.all(&rows);
        cx.notify();
    }

    /// A letter typed with nothing else held: jump to the name it starts.
    pub(crate) fn type_to_select(&mut self, text: &str, cx: &mut Context<Self>) {
        let (typed, at) = &mut self.browser.typed;
        if at.is_none_or(|at| at.elapsed() > TYPE_PAUSE) {
            typed.clear();
        }
        typed.push_str(text);
        *at = Some(Instant::now());
        let prefix = typed.clone();
        let rows = self.browse_rows();
        if let Some(i) = browse::type_select(&rows, &prefix) {
            self.browser.picked.only(&rows[i].path);
            self.browser.scroll.scroll_to_item(i, gpui::ScrollStrategy::Nearest);
            self.follow_preview(cx);
            cx.notify();
        }
    }

    /// ←/→ on a folder row: close or open it in place.
    pub(crate) fn browse_disclose(&mut self, open: bool, cx: &mut Context<Self>) {
        let rows = self.browse_rows();
        let Some(row) = self.browser.picked.cursor.as_ref().and_then(|c| rows.iter().find(|r| &r.path == c)).cloned() else { return };
        if row.dir && row.expanded != open && !self.browser.searching() {
            self.toggle_folder(&row.path, cx);
        } else if !open && row.depth > 0 {
            // ← inside an open folder selects that folder.
            let up = model::parent(&row.path).to_owned();
            self.browser.picked.only(&up);
        }
        cx.notify();
    }

    /// The selection, in the list's order.
    pub(crate) fn picked_paths(&self) -> Vec<String> {
        let rows = self.browse_rows();
        self.browser.picked.ordered(&rows)
    }

    /// Where a paste or a new item goes: the folder shown.
    fn target_dir(&self) -> String {
        self.state.browse.clone()
    }

    // ------------------------------------------------------------ search

    pub(crate) fn open_browse_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = &self.browser.search {
            let focus = search.field.read(cx).focus.clone();
            search.field.update(cx, |f, cx| f.select_all(cx));
            return focus.focus(window, cx);
        }
        let field = cx.new(|cx| TextField::new("Search this folder", cx));
        let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| match event {
            FieldEvent::Changed(text) => {
                let text = text.clone();
                this.browse_search(text, cx);
            }
            FieldEvent::Cancel => {
                this.clear_search(cx);
                window.focus(&this.browser.focus, cx);
            }
            FieldEvent::Submit | FieldEvent::Down => {
                let rows = this.browse_rows();
                if !rows.is_empty() {
                    this.browser.picked.only(&rows[0].path);
                }
                window.focus(&this.browser.focus, cx);
                cx.notify();
            }
            FieldEvent::Up => {}
        });
        let focus = field.read(cx).focus.clone();
        self.browser.search =
            Some(BrowseSearch { field, query: String::new(), hits: Vec::new(), truncated: false, searching: false, generation: 0, _sub: sub });
        focus.focus(window, cx);
        cx.notify();
    }

    fn browse_search(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(search) = self.browser.search.as_mut() else { return };
        search.generation += 1;
        search.query.clone_from(&query);
        search.searching = !query.trim().is_empty();
        let generation = search.generation;
        self.browser.picked.clear();
        self.browser.dirty();
        cx.notify();
        if query.trim().is_empty() {
            return;
        }
        let method = Method::FilesSearch(FilesSearchParams {
            block_id: self.block_id.clone(),
            query,
            path: self.state.browse.clone(),
            limit: None,
            ignored: false,
            hidden: self.browser.hidden,
        });
        self.request(method, cx, move |this, result: anyhow::Result<FileSearch>, cx| {
            let Some(search) = this.browser.search.as_mut().filter(|s| s.generation == generation) else { return };
            search.searching = false;
            match result {
                Ok(found) => {
                    search.hits = found.hits;
                    search.truncated = found.truncated;
                }
                Err(error) => {
                    search.hits.clear();
                    this.notice = Some(describe(&error));
                }
            }
            this.browser.dirty();
            cx.notify();
        });
    }

    pub(crate) fn clear_search(&mut self, cx: &mut Context<Self>) {
        if self.browser.search.take().is_some() {
            self.browser.picked.clear();
            self.browser.dirty();
            cx.notify();
        }
    }

    // ------------------------------------------------------------ operations

    /// Run a `files.*` operation answering [`FilesDone`]; failures become
    /// the notice (`what` failed: why), then `then`.
    fn file_op(
        &mut self,
        method: Method,
        what: &'static str,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &FilesDone, &mut Context<Self>) + 'static,
    ) {
        self.request(method, cx, move |this, result: anyhow::Result<FilesDone>, cx| {
            match result {
                Ok(done) => {
                    if let Some(failed) = done.failed.first() {
                        let more = done.failed.len() - 1;
                        let more = if more > 0 { format!(" (and {more} more)") } else { String::new() };
                        this.notice = Some(format!("{what} {}: {}{more}", model::name(&failed.path), failed.message));
                    }
                    then(this, &done, cx);
                }
                Err(error) => this.notice = Some(format!("{what}: {}", describe(&error))),
            }
            this.browser.dirty();
            cx.notify();
        });
    }

    /// List again the folders `done` touched that are listed here (the
    /// server's `files.changed` follows too; this is sooner).
    fn relist_touched(&mut self, done: &FilesDone, cx: &mut Context<Self>) {
        let mut dirs: Vec<String> = done.done.iter().flat_map(|m| [model::parent(&m.from).to_owned(), model::parent(&m.to).to_owned()]).collect();
        dirs.sort();
        dirs.dedup();
        dirs.retain(|d| self.listings.contains_key(d));
        for dir in dirs {
            self.list(dir, cx);
        }
    }

    /// Select `paths` (once listed) and show the first.
    fn select_paths(&mut self, paths: &[String]) {
        self.browser.picked.paths = paths.to_vec();
        self.browser.picked.anchor = paths.first().cloned();
        self.browser.picked.cursor = paths.last().cloned();
    }

    /// New Folder (⇧⌘N): `untitled folder` (or the next free name) in the
    /// folder shown, then renamed in place.
    pub(crate) fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = model::join(&self.target_dir(), "untitled folder");
        let method = Method::FilesMkdir(FilesMkdirParams { block_id: self.block_id.clone(), path, unique: true });
        let _ = window;
        self.file_op(method, "Couldn't create a folder", cx, move |this, done, cx| {
            this.relist_touched(done, cx);
            if let Some(made) = done.done.first() {
                this.sound(crate::sound::Sound::Open);
                let path = made.to.clone();
                this.select_paths(std::slice::from_ref(&path));
                this.browser.rename_next = Some(path);
            }
        });
    }

    /// New File: `untitled.txt` (or the next free name), renamed in place.
    pub(crate) fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.target_dir();
        let taken = self.listings.get(&dir).map(|l| l.iter().map(|e| e.name.clone()).collect::<Vec<_>>()).unwrap_or_default();
        let name = browse::free_name("untitled.txt", taken.iter().map(String::as_str));
        let path = model::join(&dir, &name);
        let method = Method::FilesCreate(FilesCreateParams { block_id: self.block_id.clone(), path: path.clone(), text: String::new() });
        let _ = window;
        self.request(method, cx, move |this, result: anyhow::Result<FileWritten>, cx| {
            match result {
                Ok(written) => {
                    this.sound(crate::sound::Sound::Open);
                    let dir = model::parent(&written.path).to_owned();
                    if this.listings.contains_key(&dir) {
                        this.list(dir, cx);
                    }
                    this.select_paths(std::slice::from_ref(&written.path));
                    this.browser.rename_next = Some(written.path);
                }
                Err(error) => this.notice = Some(format!("Couldn't create a file: {}", describe(&error))),
            }
            cx.notify();
        });
    }

    /// Return on the selection: rename it in place.
    pub(crate) fn rename_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = self.picked_paths();
        if let [one] = picked.as_slice() {
            self.start_rename(one.clone(), window, cx);
        }
    }

    pub(crate) fn start_rename(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let field = cx.new(|cx| TextField::new("Name", cx));
        let name = model::name(&path).to_owned();
        field.update(cx, |f, cx| {
            f.set_text(name, cx);
            f.select_all(cx);
        });
        let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| match event {
            FieldEvent::Submit => {
                this.commit_rename(cx);
                window.focus(&this.browser.focus, cx);
            }
            FieldEvent::Cancel => {
                this.browser.rename = None;
                window.focus(&this.browser.focus, cx);
                cx.notify();
            }
            _ => {}
        });
        let focus = field.read(cx).focus.clone();
        self.browser.rename = Some(Rename { path, field, _sub: sub });
        focus.focus(window, cx);
        cx.notify();
    }

    /// Finish an in-place rename (Return, or a click elsewhere).
    pub(crate) fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(rename) = self.browser.rename.take() else { return };
        let text = rename.field.read(cx).text.clone();
        let Some(to) = browse::renamed(&rename.path, &text) else { return cx.notify() };
        let method = Method::FilesRename(FilesRenameParams { block_id: self.block_id.clone(), from: rename.path.clone(), to, replace: false });
        self.file_op(method, "Couldn't rename", cx, |this, done, cx| {
            this.relist_touched(done, cx);
            let to: Vec<String> = done.done.iter().map(|m| m.to.clone()).collect();
            // An open tab follows its file (quietly: nothing opened or closed).
            this.quiet = true;
            for moved in &done.done {
                if this.state.open.contains(&moved.from) {
                    let was_active = this.active() == Some(moved.from.as_str());
                    this.close_now(&moved.from, cx);
                    this.open(&moved.to, was_active, cx);
                }
            }
            this.quiet = false;
            this.select_paths(&to);
        });
    }

    /// ⌘⌫: the selection to the host's Trash (recoverable there).
    pub(crate) fn trash_selected(&mut self, cx: &mut Context<Self>) {
        let paths = self.picked_paths();
        if paths.is_empty() {
            return;
        }
        let method = Method::FilesTrash(FilesTrashParams { block_id: self.block_id.clone(), paths });
        self.file_op(method, "Couldn't move to the Trash", cx, |this, done, cx| {
            this.relist_touched(done, cx);
            let n = done.done.len();
            if n > 0 {
                this.sound(crate::sound::Sound::Close);
            }
            if n > 0 && done.failed.is_empty() {
                this.notice = Some(if n == 1 {
                    format!("Moved \u{201c}{}\u{201d} to the Trash", model::name(&done.done[0].from))
                } else {
                    format!("Moved {n} items to the Trash")
                });
            }
            this.browser.picked.clear();
        });
    }

    /// ⌘D: copies of the selection beside them (`name copy`).
    pub(crate) fn duplicate_selected(&mut self, cx: &mut Context<Self>) {
        let paths = self.picked_paths();
        let mut by_dir: Vec<(String, Vec<String>)> = Vec::new();
        for path in paths {
            let dir = model::parent(&path).to_owned();
            match by_dir.iter_mut().find(|(d, _)| *d == dir) {
                Some((_, items)) => items.push(path),
                None => by_dir.push((dir, vec![path])),
            }
        }
        for (to_dir, from) in by_dir {
            self.transfer(from, to_dir, true, cx);
        }
    }

    /// Copy or move `from` into `to_dir` (within the block).
    pub(crate) fn transfer(&mut self, from: Vec<String>, to_dir: String, copy: bool, cx: &mut Context<Self>) {
        if from.is_empty() {
            return;
        }
        let params = FilesTransferParams { block_id: self.block_id.clone(), from, to_dir, replace: false };
        let (method, what) = if copy { (Method::FilesCopy(params), "Couldn't copy") } else { (Method::FilesMove(params), "Couldn't move") };
        self.file_op(method, what, cx, move |this, done, cx| {
            this.relist_touched(done, cx);
            if !done.done.is_empty() {
                this.sound(if copy { crate::sound::Sound::Open } else { crate::sound::Sound::Drop });
            }
            let shown = this.state.browse.clone();
            let landed: Vec<String> = done.done.iter().filter(|m| model::parent(&m.to) == shown).map(|m| m.to.clone()).collect();
            if !landed.is_empty() {
                this.select_paths(&landed);
            }
        });
    }

    /// ⌘C: remember the selection for ⌘V, and put its names on the
    /// clipboard (paths, for this Mac's host).
    pub(crate) fn copy_selected(&mut self, cx: &mut Context<Self>) {
        let paths = self.picked_paths();
        if paths.is_empty() {
            return;
        }
        let text = self.path_text(&paths);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
        self.browser.clipboard = paths;
        self.browser.clip_text = Some(text);
    }

    /// The paths as Copy Path gives them: absolute on the block's host,
    /// one per line.
    fn path_text(&self, paths: &[String]) -> String {
        let root = std::path::Path::new(&self.state.root);
        paths.iter().map(|p| root.join(p).to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n")
    }

    pub(crate) fn copy_paths(&mut self, paths: &[String], cx: &mut Context<Self>) {
        if !paths.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.path_text(paths)));
        }
    }

    /// ⌘V: files copied in Finder upload into the folder shown; items ⌘C
    /// copied here are copied there.
    pub(crate) fn paste_files(&mut self, cx: &mut Context<Self>) {
        let contents = cx.read_from_clipboard().map(|item| crate::clipboard::Contents::of(&item)).unwrap_or_default();
        let ours = self.browser.clip_text.is_some() && contents.text == self.browser.clip_text;
        if !contents.paths.is_empty() && !ours {
            let to_dir = self.target_dir();
            return self.upload_into(contents.paths, to_dir, cx);
        }
        if !self.browser.clipboard.is_empty() {
            let (from, to_dir) = (self.browser.clipboard.clone(), self.target_dir());
            self.transfer(from, to_dir, true, cx);
        }
    }

    /// Reveal in Finder (this Mac's host only).
    pub(crate) fn reveal_in_finder(&self, path: &str) {
        if !self.local {
            return;
        }
        let path = std::path::Path::new(&self.state.root).join(path);
        if let Err(error) = std::process::Command::new("/usr/bin/open").arg("-R").arg(&path).spawn() {
            tracing::warn!("reveal {}: {error}", path.display());
        }
    }

    /// Open in Terminal: a terminal block at the folder (a file's folder).
    pub(crate) fn open_terminal_at(&self, path: Option<&str>, cx: &mut Context<Self>) {
        let rows = self.browse_rows();
        let dir = match path {
            Some(path) if rows.iter().any(|r| r.path == path && r.dir) => path.to_owned(),
            Some(path) => model::parent(path).to_owned(),
            None => self.state.browse.clone(),
        };
        let abs = std::path::Path::new(&self.state.root).join(&dir).to_string_lossy().into_owned();
        cx.emit(FilesPaneEvent::OpenTerminal(abs));
    }

    // ------------------------------------------------------------ Quick Look

    /// Space: Quick Look the selected item, or close it.
    pub(crate) fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        if self.browser.preview.take().is_some() {
            return cx.notify();
        }
        let Some(path) = self.browser.picked.cursor.clone().or_else(|| self.picked_paths().into_iter().next()) else { return };
        self.preview(path, cx);
    }

    /// While Quick Look shows, it follows the selection.
    fn follow_preview(&mut self, cx: &mut Context<Self>) {
        let Some(shown) = self.browser.preview.as_ref().map(|p| p.path.clone()) else { return };
        if let Some(cursor) = self.browser.picked.cursor.clone().filter(|c| *c != shown) {
            self.preview(cursor, cx);
        }
    }

    fn preview(&mut self, path: String, cx: &mut Context<Self>) {
        let rows = self.browse_rows();
        let row = rows.iter().find(|r| r.path == path).cloned();
        if let Some(row) = row.filter(|r| r.dir) {
            let content = Content::Binary { mime: format!("Folder · {}", row.kind), size: 0 };
            self.browser.preview = Some(Preview { path, content });
            return cx.notify();
        }
        self.browser.preview = Some(Preview { path: path.clone(), content: Content::Loading });
        cx.notify();
        let method = Method::FilesRead(FilesReadParams { block_id: self.block_id.clone(), path: path.clone(), max_bytes: Some(PREVIEW_BYTES) });
        self.request(method, cx, move |this, result: anyhow::Result<FileRead>, cx| {
            if this.browser.preview.as_ref().is_none_or(|p| p.path != path) {
                return;
            }
            let content = match result {
                Ok(read) if read.mime.starts_with("image/") => return this.preview_image(path, cx),
                Ok(read) if read.kind == ContentKind::Text => {
                    let text = read.text.unwrap_or_default();
                    let doc = Rc::new(model::Doc::new(text));
                    Content::Text(Box::new(super::TextFile::plain(doc, read.truncated, read.size)))
                }
                Ok(read) => Content::Binary { mime: read.mime, size: read.size },
                Err(error) => Content::Failed(describe(&error)),
            };
            this.browser.preview = Some(Preview { path, content });
            cx.notify();
        });
    }

    fn preview_image(&mut self, path: String, cx: &mut Context<Self>) {
        let method = Method::FilesReadImage(FilesReadImageParams { block_id: self.block_id.clone(), path: path.clone() });
        self.request(method, cx, move |this, result: anyhow::Result<FileImage>, cx| {
            if this.browser.preview.as_ref().is_none_or(|p| p.path != path) {
                return;
            }
            let content = match result.and_then(|image| Ok((data_encoding::BASE64.decode(image.data_base64.as_bytes())?, image.mime, image.size))) {
                Ok((bytes, mime, size)) => match gpui::ImageFormat::from_mime_type(&mime) {
                    Some(format) => {
                        let dims = super::image_dims(&bytes);
                        Content::Image { image: Arc::new(gpui::Image::from_bytes(format, bytes)), mime, size, dims }
                    }
                    None => Content::Binary { mime, size },
                },
                Err(error) => Content::Failed(describe(&error)),
            };
            this.browser.preview = Some(Preview { path, content });
            cx.notify();
        });
    }

    // ------------------------------------------------------------ drags

    /// The drag of a row: the selection when the row is in it, else the
    /// row alone.
    pub(crate) fn drag_of(&self, path: &str, picked: &Rc<Vec<String>>) -> DraggedFiles {
        let paths = if picked.iter().any(|p| p == path) { picked.clone() } else { Rc::new(vec![path.to_owned()]) };
        DraggedFiles {
            block: self.block_id.clone(),
            paths,
            local_root: self.local.then(|| PathBuf::from(&self.state.root)).filter(|r| !r.as_os_str().is_empty()),
        }
    }

    /// Items dropped on folder `to_dir` (`copy`: ⌥ held).
    pub(crate) fn drop_files(&mut self, dragged: &DraggedFiles, to_dir: &str, copy: bool, cx: &mut Context<Self>) {
        if dragged.block != self.block_id {
            self.notice = Some("Drag files between Files blocks isn't supported yet: copy them instead".into());
            return cx.notify();
        }
        let paths: Vec<String> = dragged.paths.iter().cloned().collect();
        if !copy && !browse::can_drop(&paths, to_dir) {
            return;
        }
        if copy && paths.iter().any(|p| to_dir == p || to_dir.starts_with(&format!("{p}/"))) {
            return;
        }
        self.transfer(paths, to_dir.to_owned(), copy, cx);
    }

    /// What a drag out of the window would offer for `paths`, for the
    /// promised-file payload to come (see [`DragOutItem`]).
    #[allow(dead_code, reason = "the seam for gpui's promised-file drags (see DragOutItem)")]
    pub(crate) fn drag_out_items(&self, paths: &[String]) -> Vec<DragOutItem> {
        let rows = self.browse_rows();
        paths
            .iter()
            .map(|path| {
                let row = rows.iter().find(|r| &r.path == path);
                let entry = self.listings.get(model::parent(path)).and_then(|l| l.iter().find(|e| e.name == model::name(path)));
                DragOutItem {
                    name: model::name(path).to_owned(),
                    is_dir: row.map_or_else(|| entry.is_some_and(model::is_dir), |r| r.dir),
                    size: row.map_or_else(|| entry.map_or(0, |e| e.size), |r| r.size),
                    rel_path: path.clone(),
                }
            })
            .collect()
    }

    /// Fetches dragged-out items from the host, for the promised-file
    /// payload to come (see [`DragOutItem`]).
    #[allow(dead_code, reason = "the seam for gpui's promised-file drags (see DragOutItem)")]
    pub(crate) fn drag_out_fetch(&self) -> Option<DragOutFetch> {
        let control = self.control.clone().filter(|_| self.ops())?;
        Some(DragOutFetch { control, block_id: self.block_id.clone(), rt: self.rt.clone() })
    }

    // ------------------------------------------------------------ uploads

    /// Files dropped from Finder (or pasted): uploaded into `to_dir` on the
    /// block's host, with progress and Cancel.
    pub(crate) fn upload_into(&mut self, paths: Vec<PathBuf>, to_dir: String, cx: &mut Context<Self>) {
        if !self.ops() {
            self.notice = Some("This host can't take files into a Files block: update Silo there".into());
            return cx.notify();
        }
        if self.browser.transfer.is_some() {
            self.notice = Some("An upload is in progress".into());
            return cx.notify();
        }
        let Some(control) = self.control.clone() else { return };
        let files: Vec<silo_client::Upload> = paths
            .into_iter()
            .map(|path| silo_client::Upload {
                name: path.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned()),
                source: silo_client::UploadSource::Path(path),
            })
            .collect();
        let label = match files.as_slice() {
            [one] => format!("Copying \u{201c}{}\u{201d}", one.name),
            many => format!("Copying {} items", many.len()),
        };
        let progress = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
        let report = progress.clone();
        let (block, dir) = (self.block_id.clone(), to_dir.clone());
        let handle = self.rt.spawn(async move {
            silo_client::upload_to(&control, &block, files, Some(dir), false, move |sent, total| {
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
            let _ = this.update(cx, |this, cx| {
                this.browser.transfer = None;
                match result {
                    Ok(Ok(paths)) => {
                        if this.listings.contains_key(&to_dir) {
                            this.list(to_dir.clone(), cx);
                        }
                        if to_dir == this.state.browse {
                            this.select_paths(&paths);
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::warn!("files upload: {error:#}");
                        this.notice = Some(upload_error(&error));
                    }
                    Err(_) => {}
                }
                this.browser.dirty();
                cx.notify();
            });
        });
        self.browser.transfer = Some(Transfer { label, progress, abort, _task: task });
        cx.notify();
    }

    pub(crate) fn cancel_upload(&mut self, cx: &mut Context<Self>) {
        if let Some(transfer) = self.browser.transfer.take() {
            transfer.abort.abort();
            cx.notify();
        }
    }
}

/// What a failed upload into the block says: the server's reason (a name
/// taken, outside the root…) or the terminal's wording for local problems.
fn upload_error(error: &anyhow::Error) -> String {
    let local = crate::clipboard::upload_error(error);
    if local == "Upload failed" { format!("Couldn't copy the files: {error}") } else { local }
}
