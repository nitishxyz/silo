//! The Changes block (docs/CHANGES_BLOCK.md §3, §5): reviewing what an
//! agent's turn, a session, the work tree, the index or a branch changed.
//! The server holds the view state (`ChangesState`: scope, tabs, layout,
//! review comments, the linked agent) and answers `changes.*` with git's
//! view; this pane draws it, changes it with `changes.set` and reads it
//! again on `changes.changed`. The diff's rows, folds, word highlights and
//! the list's groups are [`model`]'s, free of gpui.

pub(crate) mod model;
mod syntax;
mod view;
pub(crate) mod wire;

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use gpui::{AppContext as _, Context, Entity, EventEmitter, FocusHandle, ListAlignment, ListOffset, ListState, Subscription, Window, px};
use silo_proto::{
    changes::{
        ChangesCommitParams, ChangesCommitted, ChangesDiffParams, ChangesHunksParams, ChangesSetParams, ChangesStatus, ChangesStatusParams, FileDiff,
    },
    control::{Method, RpcError},
    model::{AgentLink, BlockInfo, ChangesScope, ChangesState, ReviewComment, Side},
};

use self::model::{Layout, ListMode, Pick, Row, TurnRef};
use crate::{
    agent::editor::{Editor, EditorEvent},
    text_field::{FieldEvent, TextField},
};

pub const LIST_WIDTH: f32 = crate::files::TREE_WIDTH + 40.0;
/// Files whose diffs the summary reads.
pub const SUMMARY_MAX: usize = 60;

pub enum ChangesPaneEvent {
    /// Focus came in: the server's focus follows.
    Activated,
    /// Something its header shows changed.
    Header,
    /// Ask before discarding (the shared confirm card), then [`ChangesPane::revert_now`].
    Revert(RevertAsk),
    /// Queue `text` on the linked agent (else this session's), then [`ChangesPane::review_sent`].
    SendReview { text: String, link: Option<AgentLink> },
    /// Ask the agent for a commit message; its reply goes to [`ChangesPane::commit_message`].
    AskCommit { link: Option<AgentLink> },
    /// Open `path` (under `root`) at `line` in a Files block.
    OpenFile { root: String, path: String, line: u32 },
    /// The linked agent's turns are wanted (the scope picker): [`ChangesPane::set_turns`].
    Turns(String),
}

/// A revert waiting for its confirm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertAsk {
    pub block: String,
    pub path: String,
    pub hunks: Option<Vec<String>>,
    pub question: String,
    pub detail: String,
}

/// A file's diff as read, with what is derived from it.
pub(crate) struct Loaded {
    pub diff: FileDiff,
    /// Changed bytes per hunk and line.
    pub words: Vec<Vec<model::Spans>>,
    /// Syntax spans per hunk and line, once parsed (off the UI thread).
    pub syntax: Option<syntax::DiffSyntax>,
}

/// Lines picked in the gutter (a click, ⇧-click extends): what `c` comments on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineSel {
    pub path: String,
    pub side: Side,
    pub from: u32,
    pub to: u32,
}

impl LineSel {
    pub fn contains(&self, path: &str, side: Side, line: u32) -> bool {
        self.path == path && self.side == side && (self.from.min(self.to)..=self.from.max(self.to)).contains(&line)
    }
}

/// The comment being written: where it goes and its field.
pub(crate) struct Composing {
    pub path: String,
    pub side: Side,
    pub line: u32,
    pub end: Option<u32>,
    pub field: Entity<TextField>,
    _sub: Subscription,
}

/// The scope picker, open.
pub(crate) struct Picker {
    pub rows: Vec<Pick>,
    pub selected: usize,
    /// Branch…: the revisions typed.
    pub branch: Option<(Entity<TextField>, Subscription)>,
}

/// The diff as drawn: its files, in row order, and rows.
#[derive(Default)]
pub(crate) struct Shown {
    pub paths: Vec<String>,
    pub files: Vec<Rc<Loaded>>,
    pub rows: Rc<Vec<Row>>,
}

pub struct ChangesPane {
    pub block_id: String,
    rt: tokio::runtime::Handle,
    control: Option<silo_client::Control>,
    pub focus: FocusHandle,
    pub info: Option<BlockInfo>,
    /// The view state: the server's, with local changes not confirmed yet.
    pub(crate) state: ChangesState,
    in_flight: u32,
    pub(crate) status: Option<ChangesStatus>,
    pub(crate) status_error: Option<String>,
    pub(crate) diffs: HashMap<String, Rc<Loaded>>,
    pub(crate) diff_errors: HashMap<String, String>,
    pub(crate) shown: Shown,
    pub(crate) list: ListState,
    /// Folds opened: path, hunk id and the fold's first line.
    pub(crate) expanded: HashSet<(String, String, usize)>,
    /// The hunk j/k or a click went to: what s / u / r / c act on.
    pub(crate) current: Option<(String, String)>,
    /// The hunk (file, hunk) under the mouse: its bar shows its actions.
    pub(crate) hovered: Option<(usize, usize)>,
    /// How far the code is scrolled sideways (px); the line numbers stay.
    pub(crate) scroll_x: f32,
    /// The diff's width as last laid out, to stop the sideways scroll at the
    /// longest line's end.
    pub(crate) body_width: std::rc::Rc<std::cell::Cell<f32>>,
    pub(crate) selection: Option<LineSel>,
    pub(crate) composing: Option<Composing>,
    pub(crate) picker: Option<Picker>,
    pub(crate) commit: Entity<Editor>,
    pub(crate) amend: bool,
    pub(crate) busy: Option<&'static str>,
    pub(crate) asking: bool,
    /// The last error (or a commit's summary), shown over the diff.
    pub(crate) notice: Option<(String, bool)>,
    /// The linked agent's turns that changed files.
    pub(crate) turns: Vec<TurnRef>,
    turns_of: Option<String>,
    /// Status reads given out; a reply to an older one is dropped.
    generation: u64,
    pub(crate) body_bounds: Rc<Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ChangesPaneEvent> for ChangesPane {}

impl ChangesPane {
    pub fn new(
        block_id: String,
        info: Option<BlockInfo>,
        rt: tokio::runtime::Handle,
        control: Option<silo_client::Control>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let state = info.as_ref().and_then(|i| i.changes.as_deref().cloned()).unwrap_or_else(|| ChangesState::new(String::new()));
        let commit = cx.new(|cx| {
            let mut e = Editor::new("Commit message…", cx);
            e.multiline = true;
            e
        });
        let subs = vec![
            cx.on_focus_in(&focus, window, |_, _, cx| cx.emit(ChangesPaneEvent::Activated)),
            cx.subscribe(&commit, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::Submit) {
                    this.commit_now(cx);
                }
            }),
        ];
        let mut pane = Self {
            block_id,
            rt,
            control,
            focus,
            info,
            state,
            in_flight: 0,
            status: None,
            status_error: None,
            diffs: HashMap::new(),
            diff_errors: HashMap::new(),
            shown: Shown::default(),
            list: ListState::new(0, ListAlignment::Top, px(600.0)),
            expanded: HashSet::new(),
            current: None,
            hovered: None,
            scroll_x: 0.0,
            body_width: std::rc::Rc::default(),
            selection: None,
            composing: None,
            picker: None,
            commit,
            amend: false,
            busy: None,
            asking: false,
            notice: None,
            turns: Vec::new(),
            turns_of: None,
            generation: 0,
            body_bounds: Rc::default(),
            _subs: subs,
        };
        pane.refresh(cx);
        pane.want_turns(cx);
        pane
    }

    /// A new control connection: read everything again.
    pub fn set_control(&mut self, control: silo_client::Control, cx: &mut Context<Self>) {
        self.control = Some(control);
        self.in_flight = 0;
        self.refresh(cx);
    }

    pub fn bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.body_bounds.get()
    }

    /// The server's view of the block (a session view, `block.changed`).
    pub fn set_info(&mut self, info: BlockInfo, cx: &mut Context<Self>) {
        let state = info.changes.as_deref().cloned();
        self.info = Some(info);
        if let Some(state) = state
            && (self.in_flight == 0 || state.root != self.state.root || state.scope != self.state.scope)
        {
            self.adopt(state, cx);
        }
    }

    /// Take `state` as the view state, reading what it newly shows.
    fn adopt(&mut self, state: ChangesState, cx: &mut Context<Self>) {
        if state == self.state {
            return;
        }
        let old = std::mem::replace(&mut self.state, state);
        if old.root != self.state.root || old.scope != self.state.scope || old.context != self.state.context {
            self.status = None;
            self.diffs.clear();
            self.diff_errors.clear();
            self.expanded.clear();
            self.current = None;
            self.selection = None;
            self.list.reset(0);
            self.refresh(cx);
        } else if old.active != self.state.active {
            self.list.scroll_to(ListOffset::default());
            self.load_diffs(cx);
        }
        if old.agent != self.state.agent {
            self.want_turns(cx);
        }
        self.rebuild(cx);
        cx.emit(ChangesPaneEvent::Header);
        cx.notify();
    }

    fn want_turns(&mut self, cx: &mut Context<Self>) {
        let session = self.state.agent.as_ref().map(|a| a.otto_session.clone());
        if session != self.turns_of {
            self.turns_of.clone_from(&session);
            self.turns.clear();
            if let Some(session) = session {
                cx.emit(ChangesPaneEvent::Turns(session));
            }
        }
    }

    /// The otto session reviews go back to.
    pub fn linked_session(&self) -> Option<&str> {
        self.state.agent.as_ref().map(|a| a.otto_session.as_str())
    }

    /// The linked agent's turns, for the scope picker.
    pub fn set_turns(&mut self, session: &str, turns: Vec<TurnRef>, cx: &mut Context<Self>) {
        if self.turns_of.as_deref() == Some(session) {
            self.turns = turns;
            cx.notify();
        }
    }

    // ------------------------------------------------------------ io

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
            let _ = this.update(cx, |this, cx| {
                done(this, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Read the status, then the diffs shown.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.state.root.is_empty() {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        let method = Method::ChangesStatus(ChangesStatusParams { block_id: self.block_id.clone() });
        self.request(method, cx, move |this, result: anyhow::Result<ChangesStatus>, cx| {
            if this.generation != generation {
                return;
            }
            this.took_status(result, cx);
        });
    }

    fn took_status(&mut self, result: anyhow::Result<ChangesStatus>, cx: &mut Context<Self>) {
        match result {
            Ok(status) => {
                self.status_error = None;
                self.status = Some(status);
                self.load_diffs(cx);
            }
            Err(error) => self.status_error = Some(crate::files::describe(&error)),
        }
        self.rebuild(cx);
        cx.emit(ChangesPaneEvent::Header);
    }

    /// The files the diff shows: the open tab, else the summary's.
    pub(crate) fn shown_paths(&self) -> Vec<String> {
        match &self.state.active {
            Some(active) => vec![active.clone()],
            None => self.status.as_ref().map(|s| s.files.iter().take(SUMMARY_MAX).map(|f| f.path.clone()).collect()).unwrap_or_default(),
        }
    }

    fn load_diffs(&mut self, cx: &mut Context<Self>) {
        let paths = self.shown_paths();
        self.diffs.retain(|p, _| paths.contains(p));
        self.diff_errors.retain(|p, _| paths.contains(p));
        for path in paths {
            self.read_diff(path, cx);
        }
    }

    fn read_diff(&mut self, path: String, cx: &mut Context<Self>) {
        let method =
            Method::ChangesDiff(ChangesDiffParams { block_id: self.block_id.clone(), path: path.clone(), context: Some(self.state.context) });
        let scope = self.state.scope.clone();
        self.request(method, cx, move |this, result: anyhow::Result<FileDiff>, cx| {
            if this.state.scope != scope || !this.shown_paths().contains(&path) {
                return;
            }
            match result {
                Ok(diff) => {
                    this.diff_errors.remove(&path);
                    if this.diffs.get(&path).is_some_and(|l| l.diff == diff) {
                        return;
                    }
                    let words = model::words(&diff);
                    this.diffs.insert(path.clone(), Rc::new(Loaded { diff, words, syntax: None }));
                    this.highlight(path, cx);
                }
                Err(error) => {
                    this.diffs.remove(&path);
                    this.diff_errors.insert(path, crate::files::describe(&error));
                }
            }
            this.rebuild(cx);
        });
    }

    /// Parse `path`'s two sides off the UI thread, then colour its lines.
    fn highlight(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(loaded) = self.diffs.get(&path).cloned() else { return };
        let Some(job) = syntax::job(&loaded.diff) else { return };
        cx.spawn(async move |this, cx| {
            let lines = cx.background_spawn(async move { job.run() }).await;
            let _ = this.update(cx, |this, cx| {
                let Some(current) = this.diffs.get(&path) else { return };
                if current.diff != loaded.diff {
                    return;
                }
                let words = current.words.clone();
                this.diffs.insert(path, Rc::new(Loaded { diff: loaded.diff.clone(), words, syntax: Some(lines.into_shared()) }));
                this.rebuild(cx);
            });
        })
        .detach();
    }

    /// Change the view state here at once and on the server.
    pub(crate) fn set(&mut self, params: ChangesSetParams, cx: &mut Context<Self>) {
        let mut state = self.state.clone();
        if let Some(scope) = params.scope.clone() {
            state.scope = scope;
        }
        if let Some(open) = params.open.clone() {
            state.open = open;
        }
        if let Some(active) = params.active.clone() {
            state.active = Some(active).filter(|a| !a.is_empty());
            if let Some(a) = &state.active
                && !state.open.contains(a)
            {
                state.open.push(a.clone());
            }
        }
        if let Some(list) = params.list {
            state.list = list;
        }
        if let Some(split) = params.split {
            state.split = split;
        }
        if let Some(comments) = params.comments.clone() {
            state.comments = comments;
        }
        if let Some(agent) = params.agent.clone() {
            state.agent = agent;
        }
        self.adopt(state, cx);
        self.in_flight += 1;
        let method = Method::ChangesSet(ChangesSetParams { block_id: self.block_id.clone(), ..params });
        self.request(method, cx, |this, result: anyhow::Result<ChangesState>, cx| {
            this.in_flight = this.in_flight.saturating_sub(1);
            match result {
                Ok(state) if this.in_flight == 0 => this.adopt(state, cx),
                Ok(_) => {}
                Err(error) => this.notice = Some((crate::files::describe(&error), true)),
            }
        });
    }

    /// `changes.changed`: the repo moved; read it again, keeping the place.
    pub fn changes_changed(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
    }

    // ------------------------------------------------------------ rows

    /// The rows for what is loaded, keeping the hunk shown at the top in place.
    pub(crate) fn rebuild(&mut self, cx: &mut Context<Self>) {
        let anchor = self.anchor();
        let paths: Vec<String> = self.shown_paths().into_iter().filter(|p| self.diffs.contains_key(p)).collect();
        let files: Vec<Rc<Loaded>> = paths.iter().filter_map(|p| self.diffs.get(p).cloned()).collect();
        let diffs: Vec<&FileDiff> = files.iter().map(|l| &l.diff).collect();
        let mut expanded: HashSet<model::FoldId> = HashSet::new();
        for (path, id, at) in &self.expanded {
            let Some(f) = paths.iter().position(|p| p == path) else { continue };
            if let Some(h) = files[f].diff.hunks.iter().position(|h| &h.id == id) {
                expanded.insert((f, h, *at));
            }
        }
        let layout = Layout { split: self.state.split, headers: self.state.active.is_none() };
        let rows = model::rows(&diffs, layout, &expanded, &self.state.comments);
        let count = rows.len();
        self.shown = Shown { paths, files, rows: Rc::new(rows) };
        self.list.reset(count);
        if let Some((path, hunk, offset)) = anchor
            && let Some(ix) = self.hunk_row_of(&path, &hunk)
        {
            self.list.scroll_to(ListOffset { item_ix: ix, offset_in_item: offset });
        }
        cx.notify();
    }

    /// The hunk at the list's top (path, id) and how far into its row.
    fn anchor(&self) -> Option<(String, String, gpui::Pixels)> {
        let top = self.list.logical_scroll_top();
        if top.item_ix == 0 && top.offset_in_item == px(0.0) {
            return None;
        }
        let row = self.shown.rows.get(top.item_ix)?;
        let (file, hunk) = (row.file(), row.hunk()?);
        let loaded = self.shown.files.get(file)?;
        let header = model::hunk_row(&self.shown.rows, file, hunk)?;
        // Rows below the header: keep the header in view, offset as far as the row was.
        let offset = if header == top.item_ix { top.offset_in_item } else { px(0.0) };
        Some((loaded.diff.path.clone(), loaded.diff.hunks.get(hunk)?.id.clone(), offset))
    }

    /// The row of hunk `id` of `path`, or the file's first row.
    pub(crate) fn hunk_row_of(&self, path: &str, id: &str) -> Option<usize> {
        let file = self.shown.paths.iter().position(|p| p == path)?;
        let hunk = self.shown.files[file].diff.hunks.iter().position(|h| h.id == id);
        match hunk {
            Some(h) => model::hunk_row(&self.shown.rows, file, h),
            None => self.shown.rows.iter().position(|r| r.file() == file),
        }
    }

    /// The hunk s / u / r / c act on: the one gone to, else the one at the top.
    pub(crate) fn target_hunk(&self) -> Option<(String, String)> {
        if let Some((path, id)) = &self.current
            && self.hunk_row_of(path, id).is_some()
            && self.diffs.get(path).is_some_and(|l| l.diff.hunks.iter().any(|h| &h.id == id))
        {
            return Some((path.clone(), id.clone()));
        }
        let (file, hunk) = model::hunk_at(&self.shown.rows, self.list.logical_scroll_top().item_ix)?;
        let loaded = self.shown.files.get(file)?;
        Some((loaded.diff.path.clone(), loaded.diff.hunks.get(hunk)?.id.clone()))
    }

    /// j / k: the next or previous hunk.
    pub(crate) fn step_hunk(&mut self, delta: isize, cx: &mut Context<Self>) {
        let from = match self.current.clone().and_then(|(p, id)| self.hunk_row_of(&p, &id)) {
            Some(row) => row,
            None => self.list.logical_scroll_top().item_ix.saturating_sub(usize::from(delta > 0)),
        };
        let Some(row) = model::step_hunk(&self.shown.rows, from, delta) else { return };
        self.go_to_row(row, cx);
    }

    fn go_to_row(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(r) = self.shown.rows.get(row)
            && let Some(h) = r.hunk()
        {
            let loaded = &self.shown.files[r.file()];
            self.current = Some((loaded.diff.path.clone(), loaded.diff.hunks[h].id.clone()));
        }
        self.list.scroll_to(ListOffset { item_ix: row.saturating_sub(1), offset_in_item: px(0.0) });
        cx.notify();
    }

    /// n / p: the next or previous file (a tab: the list's next file).
    pub(crate) fn step_file(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(active) = self.state.active.clone() {
            let Some(status) = &self.status else { return };
            let at = status.files.iter().position(|f| f.path == active).unwrap_or(0);
            let next = at.cast_signed() + delta;
            if let Some(f) = usize::try_from(next).ok().and_then(|n| status.files.get(n)) {
                let path = f.path.clone();
                self.open(&path, cx);
            }
            return;
        }
        let top = self.list.logical_scroll_top().item_ix;
        let headers = self.shown.rows.iter().enumerate().filter(|(_, r)| matches!(r, Row::File { .. })).map(|(i, _)| i);
        let row = if delta > 0 { headers.into_iter().find(|&i| i > top) } else { headers.into_iter().rev().find(|&i| i < top) };
        if let Some(row) = row {
            self.current = None;
            self.list.scroll_to(ListOffset { item_ix: row, offset_in_item: px(0.0) });
            cx.notify();
        }
    }

    pub(crate) fn toggle_fold(&mut self, file: usize, hunk: usize, at: usize, cx: &mut Context<Self>) {
        let Some(loaded) = self.shown.files.get(file) else { return };
        let Some(h) = loaded.diff.hunks.get(hunk) else { return };
        let key = (loaded.diff.path.clone(), h.id.clone(), at);
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        self.rebuild(cx);
    }

    // ------------------------------------------------------------ tabs, scope, layout

    pub(crate) fn open(&mut self, path: &str, cx: &mut Context<Self>) {
        self.current = None;
        self.set(ChangesSetParams { active: Some(path.to_owned()), ..ChangesSetParams::default() }, cx);
    }

    /// The summary of every file.
    pub(crate) fn show_summary(&mut self, cx: &mut Context<Self>) {
        self.set(ChangesSetParams { active: Some(String::new()), ..ChangesSetParams::default() }, cx);
    }

    pub(crate) fn close(&mut self, path: &str, cx: &mut Context<Self>) {
        let open: Vec<String> = self.state.open.iter().filter(|p| *p != path).cloned().collect();
        let active = if self.state.active.as_deref() == Some(path) {
            let at = self.state.open.iter().position(|p| p == path).unwrap_or(0);
            Some(open.get(at).or_else(|| open.last()).cloned().unwrap_or_default())
        } else {
            None
        };
        self.set(ChangesSetParams { open: Some(open), active, ..ChangesSetParams::default() }, cx);
    }

    pub(crate) fn step_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        // The summary is the first place: no tab shown.
        let places: Vec<Option<&String>> = std::iter::once(None).chain(self.state.open.iter().map(Some)).collect();
        let at = places.iter().position(|p| p.map(String::as_str) == self.state.active.as_deref()).unwrap_or(0);
        let next = crate::launcher::step(at, places.len(), delta);
        match places[next] {
            Some(path) => {
                let path = path.clone();
                self.open(&path, cx);
            }
            None => self.show_summary(cx),
        }
    }

    pub(crate) fn set_split(&mut self, split: bool, cx: &mut Context<Self>) {
        if split != self.state.split {
            self.set(ChangesSetParams { split: Some(split), ..ChangesSetParams::default() }, cx);
        }
    }

    pub(crate) fn toggle_list(&mut self, cx: &mut Context<Self>) {
        let list = !self.state.list;
        self.set(ChangesSetParams { list: Some(list), ..ChangesSetParams::default() }, cx);
    }

    pub(crate) fn set_scope(&mut self, scope: ChangesScope, cx: &mut Context<Self>) {
        self.picker = None;
        if scope != self.state.scope {
            self.set(ChangesSetParams { scope: Some(scope), ..ChangesSetParams::default() }, cx);
        }
        cx.notify();
    }

    pub(crate) fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.take().is_none() {
            let rows = model::picker_rows(&self.turns, self.repo());
            let selected = rows.iter().position(|p| scope_of(p).as_ref() == Some(&self.state.scope)).unwrap_or(0);
            self.picker = Some(Picker { rows, selected, branch: None });
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(crate) fn pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.as_mut() else { return };
        let Some(pick) = picker.rows.get(index).cloned() else { return };
        picker.selected = index;
        match scope_of(&pick) {
            Some(scope) => self.set_scope(scope, cx),
            None => {
                let field = cx.new(|cx| TextField::new("main, main..otto", cx));
                let sub = cx.subscribe_in(&field, window, |this, field, event: &FieldEvent, window, cx| match event {
                    FieldEvent::Submit => {
                        if let Some((base, head)) = model::parse_range(&field.read(cx).text) {
                            this.set_scope(ChangesScope::Range { base, head }, cx);
                            window.focus(&this.focus, cx);
                        }
                    }
                    FieldEvent::Cancel => {
                        this.picker = None;
                        window.focus(&this.focus, cx);
                        cx.notify();
                    }
                    _ => {}
                });
                let focus = field.read(cx).focus.clone();
                picker.branch = Some((field, sub));
                window.focus(&focus, cx);
                cx.notify();
            }
        }
    }

    pub(crate) fn repo(&self) -> bool {
        self.status.as_ref().is_none_or(|s| s.repo)
    }

    /// What the list's checkboxes and the hunks' actions can do here.
    pub(crate) fn mode(&self) -> ListMode {
        match self.state.scope {
            ChangesScope::Working => ListMode::Working,
            ChangesScope::Staged => ListMode::Staged,
            ChangesScope::Unstaged => ListMode::Unstaged,
            ChangesScope::Tree { .. } | ChangesScope::Range { .. } | ChangesScope::Unknown => ListMode::ReadOnly,
        }
    }

    /// Commits from here: the work tree and the index.
    pub(crate) fn commits(&self) -> bool {
        self.repo() && matches!(self.state.scope, ChangesScope::Working | ChangesScope::Staged)
    }

    // ------------------------------------------------------------ writes

    fn write(&mut self, method: Method, what: &'static str, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(what);
        self.notice = None;
        self.request(method, cx, |this, result: anyhow::Result<ChangesStatus>, cx| {
            this.busy = None;
            match result {
                Ok(status) => this.took_status(Ok(status), cx),
                Err(error) => {
                    this.notice = Some((write_error(&error), true));
                    this.refresh(cx);
                }
            }
        });
    }

    fn hunks_params(&self, path: &str, hunks: Option<Vec<String>>) -> ChangesHunksParams {
        ChangesHunksParams { block_id: self.block_id.clone(), path: path.to_owned(), hunks }
    }

    pub(crate) fn stage(&mut self, path: &str, hunks: Option<Vec<String>>, cx: &mut Context<Self>) {
        let method = Method::ChangesStage(self.hunks_params(path, hunks));
        self.write(method, "Staging…", cx);
    }

    pub(crate) fn unstage(&mut self, path: &str, hunks: Option<Vec<String>>, cx: &mut Context<Self>) {
        let method = Method::ChangesUnstage(self.hunks_params(path, hunks));
        self.write(method, "Unstaging…", cx);
    }

    /// Ask before discarding `path`'s changes (or those hunks).
    pub(crate) fn ask_revert(&mut self, path: &str, hunks: Option<Vec<String>>, cx: &mut Context<Self>) {
        let lines = hunks.as_ref().and_then(|ids| {
            let loaded = self.diffs.get(path)?;
            let picked: Vec<&wire::Hunk> = loaded.diff.hunks.iter().filter(|h| ids.contains(&h.id)).collect();
            Some(model::changed_lines(&picked))
        });
        let untracked =
            self.status.as_ref().and_then(|s| s.files.iter().find(|f| f.path == path)).is_some_and(|f| f.status == wire::FileStatus::Untracked);
        let detail =
            if untracked && hunks.is_none() { "It goes to the Trash.".to_owned() } else { "Staged changes stay. This can't be undone.".to_owned() };
        let question =
            if untracked && hunks.is_none() { format!("Discard {}?", crate::files::model::name(path)) } else { model::revert_question(path, lines) };
        cx.emit(ChangesPaneEvent::Revert(RevertAsk { block: self.block_id.clone(), path: path.to_owned(), hunks, question, detail }));
    }

    /// The revert, confirmed.
    pub fn revert_now(&mut self, path: &str, hunks: Option<Vec<String>>, cx: &mut Context<Self>) {
        let method = Method::ChangesRevert(self.hunks_params(path, hunks));
        self.write(method, "Discarding…", cx);
    }

    /// The hunk action for a key: s stages, u unstages, r reverts.
    pub(crate) fn hunk_action(&mut self, action: HunkAction, cx: &mut Context<Self>) {
        let Some((path, id)) = self.target_hunk() else { return };
        let mode = self.mode();
        match (action, mode) {
            (_, ListMode::ReadOnly) => self.notice = Some(("This scope is read-only: pick Working, Staged or Unstaged to change files".into(), true)),
            (HunkAction::Stage, ListMode::Working | ListMode::Unstaged) => self.stage(&path, Some(vec![id]), cx),
            (HunkAction::Unstage, ListMode::Staged) => self.unstage(&path, Some(vec![id]), cx),
            // Working shows staged and unstaged as one diff: unstage the file.
            (HunkAction::Unstage, ListMode::Working) => self.unstage(&path, None, cx),
            (HunkAction::Revert, ListMode::Working | ListMode::Unstaged) => self.ask_revert(&path, Some(vec![id]), cx),
            (HunkAction::Stage, ListMode::Staged) => self.notice = Some(("Already staged".into(), false)),
            (HunkAction::Unstage, ListMode::Unstaged) => self.notice = Some(("Not staged".into(), false)),
            (HunkAction::Revert, ListMode::Staged) => self.notice = Some(("Unstage it first: a revert only discards work tree changes".into(), true)),
        }
        cx.notify();
    }

    pub(crate) fn commit_now(&mut self, cx: &mut Context<Self>) {
        if !self.commits() || self.busy.is_some() {
            return;
        }
        let message = self.commit.read(cx).text().trim().to_owned();
        if message.is_empty() {
            self.notice = Some(("Write a commit message first".into(), true));
            return cx.notify();
        }
        self.busy = Some(if self.amend { "Amending…" } else { "Committing…" });
        self.notice = None;
        let method = Method::ChangesCommit(ChangesCommitParams { block_id: self.block_id.clone(), message, amend: self.amend });
        self.request(method, cx, |this, result: anyhow::Result<ChangesCommitted>, cx| {
            this.busy = None;
            match result {
                Ok(done) => {
                    this.commit.update(cx, |e, cx| e.set_text("", cx));
                    this.amend = false;
                    this.notice = Some((format!("Committed {}: {}", done.sha, done.summary), false));
                }
                Err(error) => this.notice = Some((crate::files::describe(&error), true)),
            }
            this.refresh(cx);
        });
        cx.notify();
    }

    /// Ask the agent for a commit message (its reply fills the field).
    pub(crate) fn ask_commit(&mut self, cx: &mut Context<Self>) {
        if self.asking {
            return;
        }
        self.asking = true;
        cx.emit(ChangesPaneEvent::AskCommit { link: self.state.agent.clone() });
        cx.notify();
    }

    /// The agent's reply to Ask agent (`Err`: why there is none).
    pub fn commit_message(&mut self, reply: Result<String, String>, cx: &mut Context<Self>) {
        self.asking = false;
        match reply {
            Ok(text) if !text.trim().is_empty() => self.commit.update(cx, |e, cx| e.set_text(model::commit_message_from(&text), cx)),
            Ok(_) => self.notice = Some(("The agent's reply had no message".into(), true)),
            Err(error) => self.notice = Some((error, true)),
        }
        cx.notify();
    }

    // ------------------------------------------------------------ review

    /// Comment on the picked lines, else the hunk gone to.
    pub(crate) fn start_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let place = match &self.selection {
            Some(s) => Some((s.path.clone(), s.side, s.from.min(s.to), Some(s.from.max(s.to)).filter(|e| *e != s.from.min(s.to)))),
            None => self.target_hunk().and_then(|(path, id)| {
                let hunk = self.diffs.get(&path)?.diff.hunks.iter().find(|h| h.id == id)?.clone();
                let (side, line, end) = model::hunk_place(&hunk);
                Some((path, side, line, end))
            }),
        };
        let Some((path, side, line, end)) = place else { return };
        self.comment_on(path, side, line, end, window, cx);
    }

    pub(crate) fn comment_on(&mut self, path: String, side: Side, line: u32, end: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        let field = cx.new(|cx| TextField::new("Comment…  ↩ to add", cx));
        let sub = cx.subscribe_in(&field, window, |this, field, event: &FieldEvent, window, cx| match event {
            FieldEvent::Submit => {
                let text = field.read(cx).text.trim().to_owned();
                this.add_comment(text, cx);
                window.focus(&this.focus, cx);
            }
            FieldEvent::Cancel => {
                this.composing = None;
                window.focus(&this.focus, cx);
                cx.notify();
            }
            _ => {}
        });
        let focus = field.read(cx).focus.clone();
        self.composing = Some(Composing { path, side, line, end, field, _sub: sub });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn add_comment(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(c) = self.composing.take() else { return };
        if text.is_empty() {
            return cx.notify();
        }
        let now = now_ms();
        let id = format!("c{now:x}{}", self.state.comments.len());
        let comment = ReviewComment { id, path: c.path, side: c.side, line: c.line, end_line: c.end, text, created_at: now };
        let mut comments = self.state.comments.clone();
        comments.push(comment);
        self.selection = None;
        self.set(ChangesSetParams { comments: Some(comments), ..ChangesSetParams::default() }, cx);
    }

    pub(crate) fn remove_comment(&mut self, id: &str, cx: &mut Context<Self>) {
        let comments: Vec<ReviewComment> = self.state.comments.iter().filter(|c| c.id != id).cloned().collect();
        self.set(ChangesSetParams { comments: Some(comments), ..ChangesSetParams::default() }, cx);
    }

    /// ⌥⌘↩: the comments as one message to the agent.
    pub(crate) fn send_review(&mut self, cx: &mut Context<Self>) {
        if self.state.comments.is_empty() || self.busy.is_some() {
            return;
        }
        let label = match &self.state.scope {
            ChangesScope::Working => String::new(),
            other => scope_label(other),
        };
        let diffs = &self.diffs;
        let text = model::review_message(&label, &self.state.comments, |path, side, line| {
            diffs.get(path).and_then(|l| model::line_text(&l.diff, side, line)).map(str::to_owned)
        });
        self.busy = Some("Sending…");
        cx.emit(ChangesPaneEvent::SendReview { text, link: self.state.agent.clone() });
        cx.notify();
    }

    /// Send review went (`Ok`: the comments go) or failed.
    pub fn review_sent(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        self.busy = None;
        match result {
            Ok(()) => {
                self.notice = Some(("Review sent".into(), false));
                self.set(ChangesSetParams { comments: Some(Vec::new()), ..ChangesSetParams::default() }, cx);
            }
            Err(error) => self.notice = Some((error, true)),
        }
        cx.notify();
    }

    /// ⌘-click: the file in a Files block, at `line`.
    pub(crate) fn open_in_files(&mut self, path: &str, line: u32, cx: &mut Context<Self>) {
        cx.emit(ChangesPaneEvent::OpenFile { root: self.state.root.clone(), path: path.to_owned(), line });
    }

    /// A line of the gutter picked (`extend`: ⇧, from the last pick).
    pub(crate) fn pick_line(&mut self, path: &str, side: Side, line: u32, extend: bool, cx: &mut Context<Self>) {
        self.selection = match self.selection.take() {
            Some(s) if extend && s.path == path && s.side == side => Some(LineSel { to: line, ..s }),
            Some(s) if !extend && s.from == line && s.to == line && s.path == path && s.side == side => None,
            _ => Some(LineSel { path: path.to_owned(), side, from: line, to: line }),
        };
        cx.notify();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HunkAction {
    Stage,
    Unstage,
    Revert,
}

/// The scope a picker row stands for; `None`: Branch…, which asks.
pub(crate) fn scope_of(pick: &Pick) -> Option<ChangesScope> {
    match pick {
        Pick::Working => Some(ChangesScope::Working),
        Pick::Staged => Some(ChangesScope::Staged),
        Pick::Unstaged => Some(ChangesScope::Unstaged),
        Pick::Turn(t) => Some(ChangesScope::Tree { base: t.base_tree.clone(), head: Some(t.end_tree.clone()), label: model::turn_label(t.turn) }),
        Pick::Session { base, head } => {
            Some(ChangesScope::Tree { base: base.clone(), head: Some(head.clone()), label: model::SESSION_LABEL.to_owned() })
        }
        Pick::Branch => None,
    }
}

/// The picker button's label.
pub(crate) fn scope_label(scope: &ChangesScope) -> String {
    match scope {
        ChangesScope::Working => "Working".into(),
        ChangesScope::Staged => "Staged".into(),
        ChangesScope::Unstaged => "Unstaged".into(),
        ChangesScope::Tree { label, .. } if !label.is_empty() => label.clone(),
        ChangesScope::Tree { .. } => "Snapshot".into(),
        ChangesScope::Range { base, head: Some(head) } => format!("{base}..{head}"),
        ChangesScope::Range { base, head: None } => base.clone(),
        ChangesScope::Unknown => "Unknown scope".into(),
    }
}

/// A write's refusal, for people: a hunk gone stale says what to do.
fn write_error(error: &anyhow::Error) -> String {
    match error.downcast_ref::<RpcError>() {
        Some(rpc) if rpc.code == RpcError::CONFLICT => "The file changed: review it again".into(),
        _ => crate::files::describe(error),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests;
