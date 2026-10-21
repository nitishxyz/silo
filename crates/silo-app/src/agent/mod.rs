//! Agent block: one otto session shown as a chat (`docs/AGENT_BLOCK.md`).
//!
//! Contract with the workspace wiring: [`AgentPane::new`], [`AgentPane::show`],
//! [`AgentPane::focus_composer`], [`AgentPaneEvent`], `focus`, `block_id`.
//! `focus` tracks the whole pane; the composer and the pane's fields are
//! children of it, so "the pane is focused" is `focus.contains_focused(..)`.
//! ⌘J (with focus in the pane) opens the session switcher.

mod client;
mod editor;
mod markdown;
mod transcript;
mod view;

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    time::{Duration, Instant},
};

use gpui::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent, ListAlignment, ListState, Render,
    Subscription, Task, Window, px,
};
use otto_client::{ClientError, Connection, FollowerState, SessionCreateOptions, SessionFollower};
use otto_proto::{ApprovalDecision, ApprovalMode, AuthMode, Delivery, InputPart, LoginResult, Method, ProjectInfo, SessionInfo, TurnStatus};

use crate::text_field::{FieldEvent, TextField};
use client::Catalog;
use editor::{Editor, EditorEvent};
use transcript::{Row, Times, Todo};

/// Turn state of the shown session, reported to the server as `block.report`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Working,
    /// A pending approval (`permission`) or question (`question`).
    Blocked {
        question: bool,
    },
    Done,
    Error,
}

pub enum AgentPaneEvent {
    /// Clicked or focused: becomes the active pane.
    Activated,
    /// The user picked or started a session, or its title changed: `agent.set`.
    Session {
        session_id: String,
        title: String,
    },
    Status(AgentStatus),
    /// Jump to a silo block named by a tool call.
    Reveal(String),
}

/// Entry motion of rows and lines (`docs/AGENT_BLOCK.md` §6).
const ENTER: Duration = Duration::from_millis(320);
/// A finished group stays open this long before it folds.
const FOLD_DELAY: Duration = Duration::from_millis(500);
const FOLD: Duration = Duration::from_millis(320);
/// New words fade in over this long.
const FADE: Duration = Duration::from_millis(320);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overlay {
    Sessions,
    Models,
    Agents,
    Menu,
}

/// Choices for sessions this pane creates (otto has no method to change an
/// existing session's agent or model; approval mode applies to both).
#[derive(Clone, Debug, Default)]
struct Choice {
    agent: Option<String>,
    model: Option<(String, String)>,
    mode: ApprovalMode,
}

#[derive(Default)]
struct SignIn {
    /// Provider whose API key field is open.
    key_for: Option<String>,
    /// A subscription login waiting for its code or device approval.
    pending: Option<PendingLogin>,
    busy: bool,
    error: Option<String>,
}

struct PendingLogin {
    provider: String,
    login_id: String,
    user_code: Option<String>,
    url: String,
}

enum Conn {
    Connecting,
    Ready,
    Failed(String),
}

pub struct AgentPane {
    pub block_id: String,
    pub focus: FocusHandle,
    session_id: Option<String>,
    cwd: Option<String>,
    rt: tokio::runtime::Handle,
    epoch: Instant,
    conn: Conn,
    catalog: Option<Catalog>,
    project: Option<ProjectInfo>,
    sessions: Vec<SessionInfo>,
    follower: Option<SessionFollower>,
    pump: Option<Task<()>>,
    generation: u64,
    state: Option<FollowerState>,
    /// Loading the shown session failed.
    error: Option<String>,
    /// A request failed (prompt, approval, …); shown above the composer.
    notice: Option<String>,
    rows: Vec<Row>,
    todos: Vec<Todo>,
    times: Times,
    list: ListState,
    /// Something changed below while scrolled up.
    unseen: bool,
    editor: Entity<Editor>,
    filter: Entity<TextField>,
    key_field: Entity<Editor>,
    code_field: Entity<Editor>,
    choice: Choice,
    overlay: Option<Overlay>,
    selected: usize,
    signin: SignIn,
    status: Option<AgentStatus>,
    title: String,
    /// Set once the shown session's first state arrived.
    primed: bool,
    /// Motion bookkeeping, keyed by row and line keys.
    entered: HashMap<String, Instant>,
    live_groups: HashSet<String>,
    fold_at: HashMap<String, Instant>,
    /// Groups the user opened (folded) or closed (live).
    toggled: HashSet<String>,
    details: HashSet<String>,
    /// Lines shown in the hovered live group (auto-scroll pauses).
    frozen: Option<(String, usize)>,
    /// Arrival marks of the streaming text (byte length, when).
    marks: Option<(String, Vec<(usize, Instant)>)>,
    tray_open: bool,
    tray_shown: Option<Instant>,
    /// Running state changed at (send ↔ stop morph).
    morph: Option<Instant>,
    running: bool,
    /// The shown session was created by this pane (its steps are timed from the start).
    fresh: bool,
    usage_seen: Option<u64>,
    context: Option<u64>,
    /// Parsed assistant text by row key.
    parsed: HashMap<String, (String, std::rc::Rc<Vec<markdown::Block>>)>,
    _subscriptions: Vec<Subscription>,
}

impl AgentPane {
    pub fn new(
        block_id: String,
        session_id: Option<String>,
        cwd: Option<String>,
        rt: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let editor = cx.new(|cx| Editor::new("Ask anything", cx));
        let filter = cx.new(|cx| TextField::new("Filter", cx));
        let key_field = cx.new(|cx| {
            let mut e = Editor::new("Paste an API key", cx);
            e.mask = true;
            e
        });
        let code_field = cx.new(|cx| Editor::new("Paste the code", cx));
        let subscriptions = vec![
            cx.observe(&editor, |_, _, cx| cx.notify()),
            cx.observe(&key_field, |_, _, cx| cx.notify()),
            cx.observe(&code_field, |_, _, cx| cx.notify()),
            cx.subscribe_in(&editor, window, |this, _, event: &EditorEvent, window, cx| this.editor_event(event, window, cx)),
            cx.subscribe_in(&key_field, window, |this, _, event: &EditorEvent, _, cx| {
                if matches!(event, EditorEvent::Submit) {
                    this.submit_key(cx);
                }
            }),
            cx.subscribe_in(&code_field, window, |this, _, event: &EditorEvent, _, cx| {
                if matches!(event, EditorEvent::Submit) {
                    this.submit_code(cx);
                }
            }),
            cx.subscribe_in(&filter, window, |this, _, event: &FieldEvent, window, cx| this.filter_event(event, window, cx)),
            cx.on_focus_in(&focus, window, |_, _, cx| cx.emit(AgentPaneEvent::Activated)),
            // The pane itself has nothing to type into: focus lands on the composer.
            cx.on_focus(&focus, window, |this, window, cx| this.focus_composer(window, cx)),
        ];
        let list = ListState::new(0, ListAlignment::Top, px(600.0));
        list.set_follow_mode(gpui::FollowMode::Tail);
        let mut pane = Self {
            block_id,
            focus,
            session_id,
            cwd,
            rt,
            epoch: Instant::now(),
            conn: Conn::Connecting,
            catalog: None,
            project: None,
            sessions: Vec::new(),
            follower: None,
            pump: None,
            generation: 0,
            state: None,
            error: None,
            notice: None,
            rows: Vec::new(),
            todos: Vec::new(),
            times: Times::default(),
            list,
            unseen: false,
            editor,
            filter,
            key_field,
            code_field,
            choice: Choice::default(),
            overlay: None,
            selected: 0,
            signin: SignIn::default(),
            status: None,
            title: String::new(),
            primed: false,
            entered: HashMap::new(),
            live_groups: HashSet::new(),
            fold_at: HashMap::new(),
            toggled: HashSet::new(),
            details: HashSet::new(),
            frozen: None,
            marks: None,
            tray_open: false,
            tray_shown: None,
            morph: None,
            running: false,
            fresh: false,
            usage_seen: None,
            context: None,
            parsed: HashMap::new(),
            _subscriptions: subscriptions,
        };
        pane.load(cx);
        pane
    }

    /// The server's state changed (another window or device switched sessions).
    pub fn show(&mut self, session_id: Option<String>, cwd: Option<String>, cx: &mut Context<Self>) {
        let cwd_changed = self.cwd != cwd;
        self.cwd = cwd;
        if cwd_changed {
            self.load(cx);
            if self.session_id == session_id {
                return;
            }
        }
        // A session this pane is about to create is not on the server yet.
        if self.session_id != session_id && session_id.is_some() {
            self.session_id = session_id;
            self.follow(cx);
        }
    }

    pub fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.editor.read(cx).focus.clone();
        window.focus(&focus, cx);
    }

    fn now(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Run `work` on the shared connection (tokio), then `done` on the pane.
    fn run<T, F, Fut>(&self, cx: &mut Context<Self>, work: F, done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static)
    where
        T: Send + 'static,
        F: FnOnce(Connection) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    {
        let task = self.rt.spawn(async move {
            let connection = client::connection().await?;
            let result = work(connection).await;
            if let Err(error) = &result {
                client::forget(error).await;
            }
            result
        });
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(result) => result.map_err(|e| describe(&e)),
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                done(this, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Connect, then load providers, the project and its sessions.
    fn load(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        self.run(
            cx,
            move |c| async move {
                let catalog = client::catalog(&c).await?;
                let project = client::project(&c, cwd.as_deref()).await?;
                let sessions = c.session_list(Some(project.id.clone()), false).await?;
                Ok((catalog, project, sessions))
            },
            |this, result, cx| match result {
                Ok((catalog, project, sessions)) => {
                    let first = matches!(this.conn, Conn::Connecting | Conn::Failed(_));
                    this.conn = Conn::Ready;
                    this.catalog = Some(catalog);
                    this.project = Some(project);
                    this.sessions = sessions;
                    if first || this.follower.is_none() {
                        this.follow(cx);
                    }
                }
                Err(error) => this.conn = Conn::Failed(error),
            },
        );
    }

    fn reload_catalog(&mut self, cx: &mut Context<Self>) {
        self.run(
            cx,
            |c| async move { client::catalog(&c).await },
            |this, result, _| {
                if let Ok(catalog) = result {
                    this.catalog = Some(catalog);
                }
            },
        );
    }

    fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.as_ref().map(|p| p.id.clone()) else { return };
        self.run(
            cx,
            move |c| async move { c.session_list(Some(project), false).await },
            |this, result, _| {
                if let Ok(mut sessions) = result {
                    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
                    this.sessions = sessions;
                }
            },
        );
    }

    /// Follow the shown session, dropping what was shown before.
    fn follow(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.follower = None;
        self.pump = None;
        self.state = None;
        self.error = None;
        self.notice = None;
        self.times = if std::mem::take(&mut self.fresh) { Times::live() } else { Times::default() };
        self.rows.clear();
        self.todos.clear();
        self.list.reset(0);
        self.list.set_follow_mode(gpui::FollowMode::Tail);
        self.unseen = false;
        self.primed = false;
        self.entered.clear();
        self.live_groups.clear();
        self.fold_at.clear();
        self.toggled.clear();
        self.details.clear();
        self.marks = None;
        self.parsed.clear();
        self.usage_seen = None;
        self.context = None;
        self.title.clear();
        self.set_running(false);
        cx.notify();
        let Some(id) = self.session_id.clone() else {
            self.set_status(AgentStatus::Idle, cx);
            return;
        };
        if !matches!(self.conn, Conn::Ready) {
            return;
        }
        let task = self.rt.spawn(async move {
            let connection = client::connection().await?;
            SessionFollower::new(connection, id).await
        });
        self.pump = Some(cx.spawn(async move |this, cx| {
            let follower = match task.await {
                Ok(Ok(follower)) => follower,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        if this.generation == generation {
                            this.error = Some(describe(&error));
                            cx.notify();
                        }
                    });
                    return;
                }
                Err(_) => return,
            };
            let mut changes = follower.changes();
            let first = changes.borrow_and_update().clone();
            let current = this.update(cx, move |this, cx| {
                if this.generation != generation {
                    return false;
                }
                this.follower = Some(follower);
                this.apply(first, cx);
                true
            });
            if !matches!(current, Ok(true)) {
                return;
            }
            while changes.changed().await.is_ok() {
                let state = changes.borrow_and_update().clone();
                let current = this.update(cx, |this, cx| {
                    let current = this.generation == generation;
                    if current {
                        this.apply(state, cx);
                    }
                    current
                });
                if !matches!(current, Ok(true)) {
                    return;
                }
                cx.background_executor().timer(Duration::from_millis(16)).await;
            }
        }));
    }

    fn set_status(&mut self, status: AgentStatus, cx: &mut Context<Self>) {
        if self.status != Some(status) {
            self.status = Some(status);
            cx.emit(AgentPaneEvent::Status(status));
        }
    }

    fn set_running(&mut self, running: bool) {
        if self.running != running {
            self.running = running;
            self.morph = Some(Instant::now());
        }
    }

    /// A new projection of the shown session.
    fn apply(&mut self, state: FollowerState, cx: &mut Context<Self>) {
        let now = self.now();
        let m = &state.mirror;
        let turn = transcript::effective_turn(m.turn.map(|t| t.1), m.info.status);
        let running = turn == Some(TurnStatus::Running);
        self.times.observe(&m.items, &m.tools, running, now);
        let cwd = if m.info.cwd.is_empty() { self.cwd.clone() } else { Some(m.info.cwd.clone()) };
        let rows = transcript::build(&transcript::Input {
            items: &m.items,
            tools: &m.tools,
            turn,
            error: m.last_error.as_ref().map(|e| e.1.as_str()),
            cwd: cwd.as_deref(),
            times: &self.times,
            now,
        });
        let todos = transcript::todos(&m.items, &m.tools, &self.times);
        if (!todos.is_empty() || !m.queue.is_empty()) && self.todos.is_empty() && self.state.as_ref().is_none_or(|s| s.mirror.queue.is_empty()) {
            self.tray_shown = self.primed.then(Instant::now);
        }
        self.todos = todos;
        self.sync_rows(rows);
        let status = transcript::agent_status(m.turn.map(|t| t.1), m.info.status, !m.pending_approvals.is_empty(), !m.pending_questions.is_empty());
        let status = if !self.primed && status == AgentStatus::Done { AgentStatus::Idle } else { status };
        self.set_status(status, cx);
        self.set_running(running);
        if !m.info.title.is_empty() && m.info.title != self.title {
            self.title.clone_from(&m.info.title);
            cx.emit(AgentPaneEvent::Session { session_id: m.info.id.clone(), title: self.title.clone() });
        }
        let used = m.info.usage.usage.input + m.info.usage.usage.cache_read + m.info.usage.usage.cache_write;
        if let Some(seen) = self.usage_seen
            && used > seen
        {
            self.context = Some(used - seen);
        }
        self.usage_seen = Some(used);
        self.state = Some(state);
        self.primed = true;
        cx.notify();
    }

    fn sync_rows(&mut self, rows: Vec<Row>) {
        let now = Instant::now();
        let old = std::mem::replace(&mut self.rows, rows);
        let same = old.iter().zip(&self.rows).take_while(|(a, b)| a == b).count();
        if same < old.len() || same < self.rows.len() {
            self.list.splice(same..old.len(), self.rows.len() - same);
            if !self.list.is_following_tail() && self.primed {
                self.unseen = true;
            }
        }
        let primed = self.primed;
        let mut stream = None;
        for row in &self.rows {
            if primed && !self.entered.contains_key(row.key()) && !old.iter().any(|o| o.key() == row.key()) {
                self.entered.insert(row.key().to_owned(), now);
            }
            match row {
                Row::Group(g) if g.live => {
                    self.live_groups.insert(g.key.clone());
                    for line in &g.lines {
                        if primed && !self.entered.contains_key(&line.key) {
                            self.entered.insert(line.key.clone(), now);
                        }
                    }
                }
                Row::Group(g) => {
                    if self.live_groups.remove(&g.key) {
                        self.fold_at.insert(g.key.clone(), now + FOLD_DELAY);
                    }
                }
                Row::Text { key, text, streaming: true } => stream = Some((key.clone(), text.len())),
                Row::Text { .. } | Row::User { .. } => {}
            }
        }
        self.marks = stream.map(|(key, len)| {
            let mut marks = match self.marks.take() {
                Some((k, marks)) if k == key => marks,
                _ => Vec::new(),
            };
            marks.retain(|(_, at)| now.duration_since(*at) < FADE * 2);
            if primed && len > 0 {
                marks.push((len, now));
            }
            (key, marks)
        });
    }

    // ---- actions ----

    fn mirror(&self) -> Option<&otto_client::SessionMirror> {
        self.state.as_ref().map(|s| &s.mirror)
    }

    fn editor_event(&mut self, event: &EditorEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            EditorEvent::Submit => self.submit(window, cx),
            EditorEvent::Escape => {
                if self.overlay.is_some() {
                    self.close_overlay(window, cx);
                } else if self.mirror().is_some_and(|m| !m.pending_approvals.is_empty()) {
                    self.answer_approval(ApprovalDecision::Deny, cx);
                }
            }
            EditorEvent::Changed => self.notice = None,
        }
    }

    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().trim().to_owned();
        let question = self.mirror().and_then(|m| m.pending_questions.first()).map(|q| q.question_id.clone());
        if let (Some(question), false) = (question, text.is_empty()) {
            self.editor.update(cx, |e, cx| e.set_text("", cx));
            self.answer_question(question, text, cx);
            return;
        }
        if text.is_empty() {
            if self.mirror().is_some_and(|m| !m.pending_approvals.is_empty()) {
                self.answer_approval(ApprovalDecision::Allow, cx);
            }
            return;
        }
        if !self.catalog.as_ref().is_some_and(Catalog::any_signed_in) {
            return;
        }
        self.editor.update(cx, |e, cx| e.set_text("", cx));
        self.list.set_follow_mode(gpui::FollowMode::Tail);
        self.unseen = false;
        let input = vec![InputPart::Text { text: text.clone() }];
        if let Some(id) = self.session_id.clone() {
            self.run(
                cx,
                move |c| async move { c.prompt(&id, input, Delivery::Queue).await },
                move |this, result, cx| {
                    if let Err(error) = result {
                        this.notice = Some(error);
                        this.editor.update(cx, |e, cx| e.set_text(text, cx));
                    }
                },
            );
            return;
        }
        let Some(project) = self.project.as_ref().map(|p| p.id.clone()) else { return };
        let options = SessionCreateOptions {
            project_id: Some(project),
            agent: self.choice.agent.clone(),
            approval_mode: Some(self.choice.mode),
            provider: self.choice.model.as_ref().map(|m| m.0.clone()),
            model: self.choice.model.as_ref().map(|m| m.1.clone()),
            ..SessionCreateOptions::default()
        };
        self.run(
            cx,
            move |c| async move {
                let info = c.session_create(options).await?;
                c.prompt(&info.id, input, Delivery::Queue).await?;
                Ok(info)
            },
            move |this, result, cx| match result {
                Ok(info) => {
                    this.session_id = Some(info.id.clone());
                    this.fresh = true;
                    cx.emit(AgentPaneEvent::Session { session_id: info.id.clone(), title: info.title.clone() });
                    this.follow(cx);
                    this.refresh_sessions(cx);
                }
                Err(error) => {
                    this.notice = Some(error);
                    this.editor.update(cx, |e, cx| e.set_text(text, cx));
                }
            },
        );
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.session_id.clone() else { return };
        self.run(
            cx,
            move |c| async move { c.cancel(&id).await },
            |this, result, _| {
                if let Err(error) = result {
                    this.notice = Some(error);
                }
            },
        );
    }

    /// Ask the same question again.
    fn retry(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(id) = self.session_id.clone() else { return };
        let input = vec![InputPart::Text { text }];
        self.list.set_follow_mode(gpui::FollowMode::Tail);
        self.run(
            cx,
            move |c| async move { c.prompt(&id, input, Delivery::Queue).await },
            |this, result, _| {
                if let Err(error) = result {
                    this.notice = Some(error);
                }
            },
        );
    }

    fn answer_approval(&mut self, decision: ApprovalDecision, cx: &mut Context<Self>) {
        let (Some(id), Some(approval)) =
            (self.session_id.clone(), self.mirror().and_then(|m| m.pending_approvals.first()).map(|a| a.approval_id.clone()))
        else {
            return;
        };
        self.run(
            cx,
            move |c| async move { c.approval_answer(&id, &approval, decision, None).await },
            |this, result, _| {
                if let Err(error) = result {
                    this.notice = Some(error);
                }
            },
        );
    }

    fn answer_question(&mut self, question: String, answer: String, cx: &mut Context<Self>) {
        let Some(id) = self.session_id.clone() else { return };
        self.run(
            cx,
            move |c| async move { c.question_answer(&id, &question, answer).await },
            |this, result, _| {
                if let Err(error) = result {
                    this.notice = Some(error);
                }
            },
        );
    }

    fn set_mode(&mut self, mode: ApprovalMode, cx: &mut Context<Self>) {
        self.choice.mode = mode;
        self.overlay = None;
        let Some(id) = self.session_id.clone() else { return };
        self.run(
            cx,
            move |c| async move { c.call(Method::SessionSetMode { session_id: id, approval_mode: mode }).await },
            |this, result, _| {
                if let Err(error) = result {
                    this.notice = Some(error);
                }
            },
        );
    }

    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = None;
        if self.session_id.is_some() {
            self.session_id = None;
            self.follow(cx);
        }
        self.focus_composer(window, cx);
    }

    fn pick_session(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = None;
        if self.session_id.as_deref() != Some(id.as_str()) {
            let title = self.sessions.iter().find(|s| s.id == id).map(|s| s.title.clone()).unwrap_or_default();
            self.session_id = Some(id.clone());
            self.follow(cx);
            cx.emit(AgentPaneEvent::Session { session_id: id, title });
        }
        self.focus_composer(window, cx);
    }

    fn open_overlay(&mut self, overlay: Overlay, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay == Some(overlay) {
            self.close_overlay(window, cx);
            return;
        }
        self.overlay = Some(overlay);
        self.selected = 0;
        self.filter.update(cx, |f, cx| f.replace_text("", cx));
        match overlay {
            Overlay::Sessions => {
                self.refresh_sessions(cx);
                self.filter.update(cx, |f, cx| f.set_placeholder("Filter sessions", cx));
                let focus = self.filter.read(cx).focus.clone();
                window.focus(&focus, cx);
            }
            Overlay::Models => {
                self.reload_catalog(cx);
                self.filter.update(cx, |f, cx| f.set_placeholder("Filter models", cx));
                let focus = self.filter.read(cx).focus.clone();
                window.focus(&focus, cx);
            }
            Overlay::Agents | Overlay::Menu => {}
        }
        cx.notify();
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = None;
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn filter_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let count = match self.overlay {
            Some(Overlay::Sessions) => self.session_choices(cx).len() + 1,
            Some(Overlay::Models) => self.model_choices(cx).len(),
            _ => return,
        };
        match event {
            FieldEvent::Changed(_) => self.selected = 0,
            FieldEvent::Up => self.selected = self.selected.checked_sub(1).unwrap_or(count.saturating_sub(1)),
            FieldEvent::Down => self.selected = if count == 0 { 0 } else { (self.selected + 1) % count },
            FieldEvent::Cancel => return self.close_overlay(window, cx),
            FieldEvent::Submit => return self.activate_choice(self.selected, window, cx),
        }
        cx.notify();
    }

    fn activate_choice(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.overlay {
            Some(Overlay::Sessions) => match self.session_choices(cx).get(index) {
                Some(s) => {
                    let id = s.id.clone();
                    self.pick_session(id, window, cx);
                }
                None => self.new_session(window, cx),
            },
            Some(Overlay::Models) => {
                if let Some(m) = self.model_choices(cx).get(index) {
                    self.choice.model = Some((m.provider.as_str().to_owned(), m.id.clone()));
                }
                self.close_overlay(window, cx);
            }
            _ => {}
        }
    }

    /// Sessions matching the filter, newest first.
    fn session_choices(&self, cx: &gpui::App) -> Vec<&SessionInfo> {
        let query = self.filter.read(cx).text.clone();
        let mut list: Vec<(i32, &SessionInfo)> = self
            .sessions
            .iter()
            .filter(|s| s.parent_id.is_none())
            .filter_map(|s| crate::commands::fuzzy(&query, &session_label(s)).map(|(score, _)| (score, s)))
            .collect();
        if query.trim().is_empty() {
            list.sort_by_key(|(_, s)| std::cmp::Reverse(s.updated_at));
        } else {
            list.sort_by_key(|(score, s)| (std::cmp::Reverse(*score), std::cmp::Reverse(s.updated_at)));
        }
        list.into_iter().map(|(_, s)| s).collect()
    }

    fn model_choices(&self, cx: &gpui::App) -> Vec<&otto_proto::ModelInfo> {
        let query = self.filter.read(cx).text.clone();
        let Some(catalog) = &self.catalog else { return Vec::new() };
        let mut list: Vec<(i32, &otto_proto::ModelInfo)> = catalog
            .usable()
            .filter_map(|m| crate::commands::fuzzy(&query, &format!("{} {} {}", m.provider, m.id, m.name)).map(|(score, _)| (score, m)))
            .collect();
        if !query.trim().is_empty() {
            list.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        }
        list.into_iter().map(|(_, m)| m).collect()
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        if k.key == "j" && k.modifiers.platform && !k.modifiers.shift && !k.modifiers.alt && !k.modifiers.control {
            self.open_overlay(Overlay::Sessions, window, cx);
            cx.stop_propagation();
        } else if k.key == "escape" && self.overlay.is_some() {
            self.close_overlay(window, cx);
            cx.stop_propagation();
        }
    }

    // ---- sign in ----

    fn login(&mut self, provider: String, mode: AuthMode, api_key: Option<String>, cx: &mut Context<Self>) {
        self.signin.busy = true;
        self.signin.error = None;
        let p = provider.clone();
        self.run(
            cx,
            move |c| async move { c.call_typed::<LoginResult>(Method::AuthLogin { provider: p, mode, api_key, device: false }).await },
            move |this, result, cx| {
                this.signin.busy = false;
                match result {
                    Ok(LoginResult::SignedIn { .. }) => {
                        this.signin = SignIn::default();
                        this.key_field.update(cx, |e, cx| e.set_text("", cx));
                        this.reload_catalog(cx);
                    }
                    Ok(LoginResult::Pending { url, login_id, user_code, verification_uri, .. }) => {
                        let open = verification_uri.clone().unwrap_or_else(|| url.clone());
                        cx.open_url(&open);
                        let device = user_code.is_some();
                        this.signin.pending = Some(PendingLogin { provider, login_id: login_id.clone(), user_code, url: open });
                        if device {
                            this.complete_login(login_id, None, cx);
                        }
                    }
                    Err(error) => this.signin.error = Some(error),
                }
            },
        );
    }

    fn complete_login(&mut self, login_id: String, code: Option<String>, cx: &mut Context<Self>) {
        self.signin.busy = true;
        self.run(
            cx,
            move |c| async move { c.call_typed::<otto_proto::AuthStatus>(Method::AuthLoginComplete { login_id, code }).await },
            |this, result, cx| {
                this.signin.busy = false;
                match result {
                    Ok(_) => {
                        this.signin = SignIn::default();
                        this.code_field.update(cx, |e, cx| e.set_text("", cx));
                        this.reload_catalog(cx);
                    }
                    Err(error) => this.signin.error = Some(error),
                }
            },
        );
    }

    fn submit_key(&mut self, cx: &mut Context<Self>) {
        let key = self.key_field.read(cx).text().trim().to_owned();
        if let (Some(provider), false) = (self.signin.key_for.clone(), key.is_empty()) {
            self.login(provider, AuthMode::ApiKey, Some(key), cx);
        }
    }

    fn submit_code(&mut self, cx: &mut Context<Self>) {
        let code = self.code_field.read(cx).text().trim().to_owned();
        if let (Some(pending), false) = (self.signin.pending.as_ref(), code.is_empty()) {
            let login_id = pending.login_id.clone();
            self.complete_login(login_id, Some(code), cx);
        }
    }
}

fn session_label(s: &SessionInfo) -> String {
    if s.title.trim().is_empty() { "Untitled".to_owned() } else { s.title.clone() }
}

/// A short, user-facing reason.
fn describe(error: &ClientError) -> String {
    match error {
        ClientError::Io { kind: std::io::ErrorKind::NotFound, .. } => "otto is not available".to_owned(),
        ClientError::Io { .. } | ClientError::Closed | ClientError::Timeout => "Can't reach otto".to_owned(),
        ClientError::Incompatible { .. } => "This otto is incompatible".to_owned(),
        ClientError::Rpc(e) => e.message.clone(),
        other => other.to_string(),
    }
}

impl EventEmitter<AgentPaneEvent> for AgentPane {}

impl Focusable for AgentPane {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AgentPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_pane(window, cx)
    }
}
