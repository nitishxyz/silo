//! Editing in the Files viewer (docs/FILES_BLOCK.md §4): keys, the input
//! method (marked text, through gpui's `EntityInputHandler`), saving with
//! `files.write`, what `files.changed` does to a tab with edits, the
//! changed-on-disk bar, the caret and colours kept up while typing. The
//! text logic is `edit.rs`; this applies it to the shown tab.
//!
//! A tab is editable when its whole text is here (not truncated), it is
//! small enough to save, it shows as text (markdown as Source) and the
//! server has `files_write`. Unsaved text is the app's alone.

use std::{collections::HashMap, ops::Range, rc::Rc, time::Duration};

use gpui::{AnyElement, App, Bounds, Context, EntityInputHandler, Pixels, Point, UTF16Selection, Window, div, prelude::*, px, rgb};
use silo_proto::{
    compat::feature,
    control::{ContentKind, FileRead, FileWritten, FilesWriteParams, Method, RpcError},
    files::FileBase,
};

use super::{
    Content, FilesPane, FilesPaneEvent, describe,
    edit::{self, By, Disk, Edit, Kind},
    model::{self, Pos, Selection},
    text,
};
use crate::theme::{self, Continuous as _};

/// Typing pauses this long before a small file is highlighted again.
const HIGHLIGHT_PAUSE: Duration = Duration::from_millis(120);
/// And a big one (over [`BIG`] bytes).
const HIGHLIGHT_PAUSE_BIG: Duration = Duration::from_millis(400);
const BIG: usize = 256 * 1024;

/// The base a whole text read gives an edit, when it can be edited.
pub(super) fn editable_read(read: &FileRead) -> Option<FileBase> {
    (read.kind == ContentKind::Text && !read.truncated && read.size <= FileWritten::MAX_BYTES).then(|| FileBase {
        mtime: read.mtime,
        size: read.size,
        sha256: read.sha256.clone(),
    })
}

/// Whether the file `read` found is the version `base` describes.
fn same_version(base: &FileBase, read: &FileRead) -> bool {
    match (&base.sha256, &read.sha256) {
        (Some(a), Some(b)) => a == b,
        _ => base.mtime == read.mtime && base.size == read.size,
    }
}

/// Unsaved tabs of Files blocks whose pane went (another session shown),
/// by block id; the block's next pane takes them back.
#[derive(Default)]
pub(crate) struct Stash(HashMap<String, Vec<(String, Content)>>);

impl gpui::Global for Stash {}

impl FilesPane {
    // ------------------------------------------------------------ state

    /// The server saves files (`files.write`). Tests run without a server,
    /// and edit.
    pub(crate) fn writes(&self) -> bool {
        self.control.as_ref().map_or(cfg!(test), |c| c.peer_supports(feature::FILES_WRITE))
    }

    /// Tab `path` holds edits not saved.
    pub(crate) fn dirty(&self, path: &str) -> bool {
        matches!(self.contents.get(path), Some(Content::Text(f)) if f.edit.as_ref().is_some_and(edit::EditState::dirty))
    }

    /// Open tabs with unsaved edits, in tab order.
    pub fn unsaved(&self) -> Vec<String> {
        self.state.open.iter().filter(|p| self.dirty(p)).cloned().collect()
    }

    pub(super) fn saving(&self, path: &str) -> bool {
        matches!(self.contents.get(path), Some(Content::Text(f)) if f.edit.as_ref().is_some_and(|e| e.saving))
    }

    /// Look at `path` on disk again once its save is answered.
    pub(super) fn recheck(&mut self, path: &str) {
        if let Some(Content::Text(f)) = self.contents.get_mut(path)
            && let Some(edit) = f.edit.as_mut()
        {
            edit.recheck = true;
        }
    }

    /// The shown tab can be edited now.
    pub(crate) fn editing(&self) -> bool {
        let Some(path) = self.state.active.as_deref() else { return false };
        self.writes() && !self.source_hidden(path) && matches!(self.contents.get(path), Some(Content::Text(f)) if f.edit.is_some())
    }

    /// A clean, editable tab read again as it was (its own save, a touch):
    /// nothing to do but note the time. True when so.
    pub(super) fn same_as_disk(&mut self, path: &str, read: &FileRead) -> bool {
        let Some(Content::Text(file)) = self.contents.get_mut(path) else { return false };
        let Some(edit) = file.edit.as_mut() else { return false };
        if read.kind != ContentKind::Text || read.truncated || !same_version(&edit.base, read) {
            return false;
        }
        edit.base.mtime = read.mtime;
        true
    }

    /// A read of `path` while it holds edits: the edits stay. When the file
    /// is no longer the one they started from, the bar asks.
    pub(super) fn disk_read(&mut self, path: &str, read: FileRead, cx: &mut Context<Self>) {
        let Some(Content::Text(file)) = self.contents.get_mut(path) else { return };
        let Some(edit) = file.edit.as_mut() else { return };
        if read.kind == ContentKind::Text && !read.truncated && same_version(&edit.base, &read) {
            edit.base.mtime = read.mtime;
            return;
        }
        if read.kind == ContentKind::Text && !read.truncated && read.text.as_deref() == Some(file.doc.text.as_str()) {
            // Someone wrote exactly these edits: clean now.
            let id = edit.history.current();
            edit.saved_as(FileBase { mtime: read.mtime, size: read.size, sha256: read.sha256 }, id);
        } else {
            edit.disk = Disk::Changed;
        }
        cx.emit(FilesPaneEvent::Header);
        cx.notify();
    }

    // ------------------------------------------------------------ save

    /// ⌘S: save the shown tab when it has edits (or kept them over a
    /// change on disk).
    pub(crate) fn save_active(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.state.active.clone() else { return };
        let wanted = matches!(self.contents.get(&path), Some(Content::Text(f)) if f.edit.as_ref().is_some_and(|e| e.dirty() || e.disk == Disk::Kept));
        if wanted {
            let _ = self.save(&path, cx);
        }
    }

    /// Write tab `path` with `files.write`, from the version its edits
    /// started from (`force` after Keep Mine). The receiver says whether it
    /// was saved; `None` when it can't be (nothing to save, a save on its
    /// way, an older server).
    pub fn save(&mut self, path: &str, cx: &mut Context<Self>) -> Option<tokio::sync::oneshot::Receiver<bool>> {
        if !self.writes() {
            self.notice = Some("This server can't save files: update it to edit them".into());
            cx.notify();
            return None;
        }
        let Some(Content::Text(file)) = self.contents.get_mut(path) else { return None };
        let edit = file.edit.as_mut()?;
        if edit.saving {
            return None;
        }
        edit.saving = true;
        let (id, force) = (edit.history.current(), edit.disk == Disk::Kept);
        let params = FilesWriteParams {
            block_id: self.block_id.clone(),
            path: path.to_owned(),
            text: file.doc.text.clone(),
            base: Some(edit.base.clone()),
            force,
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let key = path.to_owned();
        self.request(Method::FilesWrite(params), cx, move |this, result: anyhow::Result<FileWritten>, cx| {
            let saved = this.saved(&key, id, result, cx);
            let _ = tx.send(saved);
        });
        cx.notify();
        Some(rx)
    }

    pub(super) fn saved(&mut self, path: &str, id: u64, result: anyhow::Result<FileWritten>, cx: &mut Context<Self>) -> bool {
        let Some(Content::Text(file)) = self.contents.get_mut(path) else { return false };
        let Some(edit) = file.edit.as_mut() else { return false };
        edit.saving = false;
        let recheck = std::mem::take(&mut edit.recheck);
        let ok = match result {
            Ok(written) => {
                file.size = written.size;
                edit.saved_as(FileBase { mtime: written.mtime, size: written.size, sha256: Some(written.sha256) }, id);
                true
            }
            Err(error) if error.downcast_ref::<RpcError>().is_some_and(|e| e.code == RpcError::CONFLICT) => {
                edit.disk = Disk::Changed;
                false
            }
            Err(error) => {
                self.notice = Some(format!("Couldn't save {}: {}", model::name(path), describe(&error)));
                false
            }
        };
        if recheck && ok {
            self.read(path.to_owned(), None, cx);
        }
        cx.emit(FilesPaneEvent::Header);
        cx.notify();
        ok
    }

    /// The bar's Reload: drop the edits and read the file again.
    pub(crate) fn reload_from_disk(&mut self, path: &str, cx: &mut Context<Self>) {
        if let Some(Content::Text(file)) = self.contents.get_mut(path)
            && let Some(edit) = file.edit.as_mut()
        {
            // Clean, from no known version: the read replaces the text.
            edit.saved = edit.history.current();
            edit.base = FileBase::default();
            edit.disk = Disk::Same;
        }
        self.read(path.to_owned(), Some(FileRead::MAX_BYTES), cx);
        cx.emit(FilesPaneEvent::Header);
        cx.notify();
    }

    /// The bar's Keep Mine: the next save overwrites the file.
    pub(crate) fn keep_mine(&mut self, path: &str, cx: &mut Context<Self>) {
        if let Some(Content::Text(file)) = self.contents.get_mut(path)
            && let Some(edit) = file.edit.as_mut()
        {
            edit.disk = Disk::Kept;
        }
        cx.notify();
    }

    /// The bar over a tab whose file changed on disk under its edits (or
    /// whose save was refused for it).
    pub(super) fn disk_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.state.active.clone()?;
        let Some(Content::Text(file)) = self.contents.get(&path) else { return None };
        if file.edit.as_ref()?.disk != Disk::Changed {
            return None;
        }
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px(px(8.0))
                .py(px(2.0))
                .continuous(5.0)
                .bg(theme::control_fill())
                .hover(|s| s.bg(theme::hover_fill()))
                .cursor_pointer()
                .child(label)
        };
        let (reload, keep) = (path.clone(), path);
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.0))
                .px(px(12.0))
                .py(px(5.0))
                .bg(rgb(theme::mix(theme::background(), theme::warning(), 0.16)))
                .text_size(px(12.0))
                .child(div().flex_1().min_w_0().text_color(rgb(theme::foreground())).child("Changed on disk."))
                .child(
                    button("files-disk-reload", "Reload (lose your edits)")
                        .on_click(cx.listener(move |this, _, _, cx| this.reload_from_disk(&reload, cx))),
                )
                .child(button("files-disk-keep", "Keep mine").on_click(cx.listener(move |this, _, _, cx| this.keep_mine(&keep, cx))))
                .into_any_element(),
        )
    }

    // ------------------------------------------------------------ stash

    /// The pane is going (another session shown): keep its unsaved tabs for
    /// the block's next pane.
    pub fn stash(&mut self, cx: &mut Context<Self>) {
        self.park_selection();
        let dirty = self.unsaved();
        let tabs: Vec<(String, Content)> = dirty.into_iter().filter_map(|p| self.contents.remove(&p).map(|c| (p, c))).collect();
        if !tabs.is_empty() {
            cx.default_global::<Stash>().0.insert(self.block_id.clone(), tabs);
        }
    }

    /// Take back what [`Self::stash`] kept, and look at those files on disk.
    pub(super) fn unstash(&mut self, cx: &mut Context<Self>) {
        if !cx.has_global::<Stash>() {
            return;
        }
        let tabs = cx.global_mut::<Stash>().0.remove(&self.block_id).unwrap_or_default();
        for (path, content) in tabs {
            if !self.state.open.contains(&path) {
                continue;
            }
            self.contents.insert(path.clone(), content);
            self.rehighlight(&path, Duration::ZERO, cx);
            if self.state.active.as_deref() != Some(path.as_str()) {
                self.read(path, None, cx);
            }
        }
        self.unpark_selection();
    }

    // ------------------------------------------------------------ edits

    /// The selection edits apply to: the shown one, else a caret at the
    /// top line shown.
    pub(crate) fn edit_selection(&self, doc: &model::Doc) -> Selection {
        match self.selection {
            Some(sel) => edit::plain(doc, sel),
            None => {
                let top = self.state.active.as_deref().and_then(|a| self.top_line(a)).unwrap_or(0);
                #[allow(clippy::cast_possible_truncation)]
                edit::caret(doc.clamp(Pos { line: top as usize, col: 0 }))
            }
        }
    }

    /// Make the edit `make` gives for the shown tab; false when there is
    /// none (or the tab can't be edited).
    fn edit_with(&mut self, cx: &mut Context<Self>, make: impl FnOnce(&model::Doc, Selection, &edit::EditState) -> Option<Edit>) -> bool {
        if !self.editing() {
            return false;
        }
        let Some(path) = self.state.active.clone() else { return false };
        let sel = match self.contents.get(&path) {
            Some(Content::Text(file)) => self.edit_selection(&file.doc),
            _ => return false,
        };
        let Some(Content::Text(file)) = self.contents.get_mut(&path) else { return false };
        let Some(state) = file.edit.as_mut() else { return false };
        let Some(edit) = make(&file.doc, sel, state) else { return false };
        if edit.range.is_empty() && edit.text.is_empty() {
            return false;
        }
        let was = state.dirty();
        let col = file.doc.pos(edit.range.start).col;
        let (old, new) = (edit.range.len(), edit.text.len());
        let doc = Rc::make_mut(&mut file.doc);
        let (splice, after) = state.apply(doc, sel, edit, std::time::Instant::now());
        if let Some(syntax) = file.syntax.as_mut() {
            edit::splice_spans(Rc::make_mut(syntax), splice, col, old, new);
        }
        let now_dirty = state.dirty();
        self.after_edit(&path, after, was != now_dirty, cx);
        true
    }

    /// After the shown tab's text changed: the selection, colours, find
    /// matches, the caret in view, the header's dot.
    fn after_edit(&mut self, path: &str, after: Selection, header: bool, cx: &mut Context<Self>) {
        self.versions += 1;
        if let Some(Content::Text(file)) = self.contents.get_mut(path) {
            file.version = self.versions;
            file.markdown_stale = true;
        }
        self.selection = Some(after);
        self.rehighlight(path, Duration::MAX, cx);
        self.refind(cx);
        self.reveal_caret(cx);
        self.blink.reset();
        if header {
            cx.emit(FilesPaneEvent::Header);
        }
        cx.notify();
    }

    /// ⌘Z, ⇧⌘Z.
    fn undo(&mut self, again: bool, cx: &mut Context<Self>) {
        let Some(path) = self.state.active.clone().filter(|_| self.editing()) else { return };
        let Some(Content::Text(file)) = self.contents.get_mut(&path) else { return };
        let Some(state) = file.edit.as_mut() else { return };
        let was = state.dirty();
        let Some((splice, after)) = state.step(Rc::make_mut(&mut file.doc), again) else { return };
        // Colours can't follow an undo cheaply: the changed lines lose theirs until highlighted again.
        if let Some(syntax) = file.syntax.as_mut() {
            edit::splice_spans(Rc::make_mut(syntax), splice, 0, usize::MAX / 2, 0);
        }
        let header = was != state.dirty();
        self.after_edit(&path, after, header, cx);
    }

    /// Colour `path` again, off the UI thread, once typing pauses (`MAX`:
    /// the usual pause for its size; zero: now).
    pub(super) fn rehighlight(&mut self, path: &str, pause: Duration, cx: &mut Context<Self>) {
        let Some(Content::Text(file)) = self.contents.get_mut(path) else { return };
        let Some(lang) = file.lang else { return };
        let pause = if pause == Duration::MAX { if file.doc.text.len() > BIG { HIGHLIGHT_PAUSE_BIG } else { HIGHLIGHT_PAUSE } } else { pause };
        let (version, key) = (file.version, path.to_owned());
        file.highlighting = Some(cx.spawn(async move |this, cx| {
            if !pause.is_zero() {
                cx.background_executor().timer(pause).await;
            }
            let text = this.update(cx, |this, _| match this.contents.get(&key) {
                Some(Content::Text(f)) if f.version == version && crate::syntax::highlightable(&f.doc.text) => Some(f.doc.text.clone()),
                _ => None,
            });
            let Ok(Some(text)) = text else { return };
            let lines = cx.background_spawn(async move { crate::syntax::by_line(&text, &crate::syntax::highlight(lang, &text)) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(Content::Text(f)) = this.contents.get_mut(&key)
                    && f.version == version
                {
                    f.syntax = Some(Rc::new(lines));
                    cx.notify();
                }
            });
        }));
    }

    // ------------------------------------------------------------ caret

    /// Move the caret to `to` (`extend`: the selection's head only).
    fn move_to(&mut self, to: Pos, extend: bool, cx: &mut Context<Self>) {
        let Some(doc) = self.shown_doc() else { return };
        let sel = self.edit_selection(&doc);
        self.selection = Some(if extend { Selection { head: to, ..sel } } else { edit::caret(to) });
        if let Some(state) = self.edit_state_mut() {
            state.history.seal();
            state.marked = None;
        }
        self.reveal_caret(cx);
        self.blink.reset();
        cx.notify();
    }

    fn edit_state_mut(&mut self) -> Option<&mut edit::EditState> {
        let path = self.state.active.as_ref()?;
        match self.contents.get_mut(path)? {
            Content::Text(file) => file.edit.as_mut(),
            _ => None,
        }
    }

    /// Lines a page moves.
    fn page_lines(&self) -> usize {
        let m = self.metrics.get();
        let height = self.text_bounds.get().map_or(400.0, |b| f32::from(b.size.height));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lines = (height / m.line).floor() as usize;
        lines.saturating_sub(1).max(1)
    }

    /// Scroll the least that shows the caret.
    pub(crate) fn reveal_caret(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.shown_doc() else { return };
        let Some(path) = self.state.active.clone() else { return };
        let head = self.edit_selection(&doc).head;
        let handle = self.scroll_handle(&path);
        let base = handle.0.borrow().base_handle.clone();
        let m = self.metrics.get();
        let Some(bounds) = self.text_bounds.get() else { return };
        let (height, width) = (f32::from(bounds.size.height), f32::from(bounds.size.width));
        let offset = base.offset();
        let (mut x, mut y) = (f32::from(offset.x), f32::from(offset.y));
        #[allow(clippy::cast_precision_loss)]
        let top = head.line as f32 * m.line;
        if top < -y {
            y = -top;
        } else if top + m.line > -y + height {
            y = -(top + m.line - height);
        }
        let gutter = text::gutter_width(doc.line_count(), m.cell) + text::PAD;
        #[allow(clippy::cast_precision_loss)]
        let left = model::columns(&doc.line(head.line)[..head.col.min(doc.line(head.line).len())]) as f32 * m.cell;
        let room = (width - gutter - m.cell * 2.0).max(m.cell);
        if left < -x {
            x = -(left - m.cell * 4.0).max(0.0);
        } else if left > -x + room {
            x = -(left - room);
        }
        if (x, y) != (f32::from(offset.x), f32::from(offset.y)) {
            base.set_offset(gpui::point(px(x.min(0.0)), px(y.min(0.0))));
        }
        cx.notify();
    }

    /// Whether the caret shows this frame, running the blink timer while
    /// it does.
    pub(super) fn caret_phase(&mut self, shown: bool, cx: &mut Context<Self>) -> bool {
        if shown && !self.caret_on {
            self.blink.reset();
        }
        self.caret_on = shown;
        if !shown {
            return false;
        }
        if self.blink_task.is_none() {
            self.blink_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    let wait = this.update(cx, |pane, _| {
                        if pane.caret_on {
                            Some(pane.blink.until_next(std::time::Instant::now()))
                        } else {
                            pane.blink_task = None;
                            None
                        }
                    });
                    let Ok(Some(wait)) = wait else { return };
                    cx.background_executor().timer(wait).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            }));
        }
        self.blink.visible_at(std::time::Instant::now())
    }

    // ------------------------------------------------------------ keys

    /// A key in an editable viewer; false leaves it to the input method
    /// (characters) or others.
    pub(crate) fn edit_key(&mut self, e: &gpui::KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let marked = self.edit_state_mut().is_some_and(|s| s.marked.is_some());
        if marked {
            return false;
        }
        let Some(doc) = self.shown_doc() else { return false };
        let k = &e.keystroke;
        let m = k.modifiers;
        if m.control && !m.platform {
            return false;
        }
        let sel = self.edit_selection(&doc);
        let head = sel.head;
        let (a, b) = sel.bounds(&doc);
        let shift = m.shift;
        let by = if m.platform {
            By::Line
        } else if m.alt {
            By::Word
        } else {
            By::Char
        };
        let goal = self.edit_state_mut().and_then(|s| s.goal);
        let vertical = |pane: &mut Self, delta: isize, cx: &mut Context<Self>| {
            let from = if !shift && a != b { if delta < 0 { a } else { b } } else { head };
            let (to, goal) = edit::vertical(&doc, from, delta, goal);
            pane.move_to(to, shift, cx);
            if let Some(state) = pane.edit_state_mut() {
                state.goal = Some(goal);
            }
        };
        match k.key.as_str() {
            "left" => {
                let to = if m.platform {
                    edit::home(&doc, head)
                } else if m.alt {
                    edit::word_left(&doc, head)
                } else if a != b && !shift {
                    a
                } else {
                    edit::prev(&doc, head)
                };
                self.move_to(to, shift, cx);
            }
            "right" => {
                let to = if m.platform {
                    edit::end(&doc, head)
                } else if m.alt {
                    edit::word_right(&doc, head)
                } else if a != b && !shift {
                    b
                } else {
                    edit::next(&doc, head)
                };
                self.move_to(to, shift, cx);
            }
            "up" if m.platform => self.move_to(Pos::default(), shift, cx),
            "down" if m.platform => self.move_to(doc.end(), shift, cx),
            "up" => vertical(self, -1, cx),
            "down" => vertical(self, 1, cx),
            "home" if m.platform => self.move_to(Pos::default(), shift, cx),
            "end" if m.platform => self.move_to(doc.end(), shift, cx),
            "home" => self.move_to(edit::home(&doc, head), shift, cx),
            "end" => self.move_to(edit::end(&doc, head), shift, cx),
            "pageup" | "pagedown" => {
                let lines = self.page_lines().cast_signed();
                let delta = if k.key == "pageup" { -lines } else { lines };
                // The view moves a page too, so the caret keeps its place on screen.
                if let Some(path) = self.state.active.clone() {
                    let base = self.scroll_handle(&path).0.borrow().base_handle.clone();
                    let (offset, max) = (base.offset(), base.max_offset());
                    #[allow(clippy::cast_precision_loss)]
                    let y = (f32::from(offset.y) - delta as f32 * self.metrics.get().line).clamp(-f32::from(max.y), 0.0);
                    base.set_offset(gpui::point(offset.x, px(y)));
                }
                vertical(self, delta, cx);
            }
            "backspace" => {
                self.edit_with(cx, |doc, sel, s| edit::backspace(doc, sel, by, s.indent));
            }
            "delete" => {
                self.edit_with(cx, |doc, sel, _| edit::delete(doc, sel, by));
            }
            "enter" if !m.platform => {
                let colon = matches!(self.shown_lang(), Some(crate::syntax::Language::Python));
                self.edit_with(cx, |doc, sel, s| Some(edit::newline(doc, sel, s.indent, s.newline, colon)));
            }
            "tab" if shift => {
                self.edit_with(cx, |doc, sel, s| edit::shift_lines(doc, sel, s.indent, true));
            }
            "tab" if !m.platform && !m.alt => {
                self.edit_with(cx, |doc, sel, s| Some(edit::tab(doc, sel, s.indent)));
            }
            "escape" if a != b => self.move_to(head, false, cx),
            "z" if m.platform && !m.alt => self.undo(shift, cx),
            "x" if m.platform && !shift && !m.alt => self.cut(cx),
            _ => return false,
        }
        true
    }

    fn shown_lang(&self) -> Option<crate::syntax::Language> {
        match self.contents.get(self.state.active.as_ref()?)? {
            Content::Text(file) => file.lang,
            _ => None,
        }
    }

    /// ⌘X.
    fn cut(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.selected_text() else { return };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.edit_with(cx, |doc, sel, _| Some(Edit { range: edit::span(doc, sel), text: String::new(), after: None, kind: Kind::Other }));
    }

    /// ⌘V; false when the tab can't take it.
    pub(crate) fn paste(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.editing() {
            return false;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) else { return true };
        self.edit_with(cx, |doc, sel, s| {
            let mut e = edit::insert(doc, sel, &text, s.newline);
            e.kind = Kind::Other;
            Some(e)
        });
        true
    }

    /// ⌘↑ / ⌘↓ (bound to the terminal's prompt jumps): the text's start or
    /// end; false when the tab can't be edited (the viewer scrolls).
    pub(crate) fn caret_to_end(&mut self, end: bool, cx: &mut Context<Self>) -> bool {
        if !self.editing() {
            return false;
        }
        let Some(doc) = self.shown_doc() else { return false };
        self.move_to(if end { doc.end() } else { Pos::default() }, false, cx);
        true
    }

    /// The caret's rectangle (window coordinates) at `pos`, from the last
    /// paint.
    fn caret_bounds(&self, pos: Pos) -> Option<Bounds<Pixels>> {
        let doc = self.shown_doc()?;
        let bounds = self.text_bounds.get()?;
        let path = self.state.active.as_ref()?;
        let offset = self.scrolls.get(path)?.0.borrow().base_handle.offset();
        let m = self.metrics.get();
        let line = doc.line(pos.line);
        #[allow(clippy::cast_precision_loss)]
        let x = model::columns(&line[..pos.col.min(line.len())]) as f32 * m.cell;
        let gutter = text::gutter_width(doc.line_count(), m.cell) + text::PAD;
        #[allow(clippy::cast_precision_loss)]
        let y = pos.line as f32 * m.line;
        let origin = gpui::point(bounds.left() + offset.x + px(gutter + x), bounds.top() + offset.y + px(y));
        Some(Bounds::new(origin, gpui::size(px(m.cell), px(m.line))))
    }
}

impl EntityInputHandler for FilesPane {
    fn text_for_range(&mut self, range: Range<usize>, adjusted: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        let doc = self.shown_doc().filter(|_| self.editing())?;
        let range = edit::from_utf16(&doc, range.start)..edit::from_utf16(&doc, range.end);
        adjusted.replace(edit::to_utf16(&doc, range.start)..edit::to_utf16(&doc, range.end));
        doc.text.get(range).map(str::to_owned)
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        let doc = self.shown_doc().filter(|_| self.editing())?;
        let sel = self.edit_selection(&doc);
        let range = edit::span(&doc, sel);
        Some(UTF16Selection { range: edit::to_utf16(&doc, range.start)..edit::to_utf16(&doc, range.end), reversed: sel.head < sel.anchor })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let doc = self.shown_doc()?;
        let path = self.state.active.as_ref()?;
        let Some(Content::Text(file)) = self.contents.get(path) else { return None };
        let marked = file.edit.as_ref()?.marked.clone()?;
        Some(edit::to_utf16(&doc, marked.start)..edit::to_utf16(&doc, marked.end))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.edit_state_mut() {
            state.marked = None;
        }
        cx.notify();
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let doc = self.shown_doc()?;
        Some(edit::to_utf16(&doc, doc.text.len()))
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let marked = self.edit_state_mut().and_then(|s| s.marked.take());
        self.edit_with(cx, |doc, sel, s| {
            let range =
                range.map(|r| edit::from_utf16(doc, r.start)..edit::from_utf16(doc, r.end)).or(marked).unwrap_or_else(|| edit::span(doc, sel));
            let text = edit::with_newlines(text, s.newline);
            let kind = if text.contains('\n') || text.chars().count() > 1 { Kind::Other } else { Kind::Type };
            Some(Edit { range, text, after: None, kind })
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let marked = self.edit_state_mut().and_then(|s| s.marked.take());
        let mut start = None;
        let edited = self.edit_with(cx, |doc, sel, _| {
            let range =
                range.map(|r| edit::from_utf16(doc, r.start)..edit::from_utf16(doc, r.end)).or(marked).unwrap_or_else(|| edit::span(doc, sel));
            start = Some(range.start);
            let text = text.replace(['\r', '\n'], " ");
            // Composing is not a step of its own until committed.
            Some(Edit { range, text, after: None, kind: Kind::Other })
        });
        let (Some(start), true) = (start, edited || text.is_empty()) else { return };
        let len = text.len();
        let Some(doc) = self.shown_doc() else { return };
        if let Some(state) = self.edit_state_mut() {
            state.marked = (len > 0).then(|| start..start + len);
        }
        if let Some(sel) = selected {
            let units: Vec<u16> = text.encode_utf16().collect();
            let byte = |u: usize| String::from_utf16_lossy(&units[..u.min(units.len())]).len();
            let (a, b) = (doc.pos(start + byte(sel.start)), doc.pos(start + byte(sel.end)));
            self.selection = Some(Selection { anchor: a, head: b, unit: model::Unit::Char });
        }
        cx.notify();
    }

    fn bounds_for_range(&mut self, range: Range<usize>, _: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let doc = self.shown_doc()?;
        let pos = doc.pos(edit::from_utf16(&doc, range.start));
        self.caret_bounds(pos)
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let doc = self.shown_doc()?;
        let pos = self.pos_at(point)?;
        Some(edit::to_utf16(&doc, doc.offset(pos)))
    }
}

/// The unsaved-tab helpers the workspace's dialog uses.
pub(crate) fn save_all(pane: &gpui::Entity<FilesPane>, paths: &[String], cx: &mut App) -> Vec<tokio::sync::oneshot::Receiver<bool>> {
    pane.update(cx, |pane, cx| {
        let mut out = Vec::new();
        for path in paths {
            match pane.save(path, cx) {
                Some(rx) => out.push(rx),
                None => {
                    // Couldn't start: a refused answer.
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = tx.send(false);
                    out.push(rx);
                }
            }
        }
        out
    })
}
