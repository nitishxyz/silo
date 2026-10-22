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
mod pill_input;
mod rows;
mod signin;
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
use otto_proto::{ApprovalDecision, ApprovalMode, Delivery, InputPart, Method, ProjectInfo, SessionInfo, TurnStatus};

use crate::text_field::{FieldEvent, TextField};
use client::Catalog;
use editor::{Editor, EditorEvent, Nav};
use signin::SignIn;
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
    /// [`AgentPane::header_title`] changed: redraw the pane header.
    Header,
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
    /// Slash commands, typed into the composer.
    Slash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlashAct {
    New,
    Sessions,
    Model,
    Agent,
    Mode(ApprovalMode),
    Providers,
    Clear,
}

struct SlashCommand {
    name: &'static str,
    about: &'static str,
    icon: &'static str,
    act: SlashAct,
}

const SLASH: [SlashCommand; 9] = [
    SlashCommand { name: "new", about: "Start a new session", icon: "new-session", act: SlashAct::New },
    SlashCommand { name: "sessions", about: "Switch to another session", icon: "history", act: SlashAct::Sessions },
    SlashCommand { name: "model", about: "Choose the model", icon: "bot", act: SlashAct::Model },
    SlashCommand { name: "agent", about: "Choose the agent", icon: "agent", act: SlashAct::Agent },
    SlashCommand { name: "mode auto", about: "Run tools without asking", icon: "lock-off", act: SlashAct::Mode(ApprovalMode::Auto) },
    SlashCommand { name: "mode ask", about: "Ask before changes", icon: "lock", act: SlashAct::Mode(ApprovalMode::AskOnEffects) },
    SlashCommand { name: "mode plan", about: "Read-only", icon: "list-todo", act: SlashAct::Mode(ApprovalMode::Plan) },
    SlashCommand { name: "providers", about: "Connect or disconnect providers", icon: "plug", act: SlashAct::Providers },
    SlashCommand { name: "clear", about: "Clear the input and start over", icon: "rotate-ccw", act: SlashAct::Clear },
];

/// What follows a leading `/` on a single line.
fn slash_query(text: &str) -> Option<&str> {
    text.strip_prefix('/').filter(|q| !q.contains('\n') && !q.starts_with(' '))
}

/// Commands matching `query`, best first (all, in order, for an empty query).
fn slash_matches(query: &str) -> Vec<&'static SlashCommand> {
    let query = query.trim();
    if query.is_empty() {
        return SLASH.iter().collect();
    }
    let mut hits: Vec<(i32, usize, &SlashCommand)> =
        SLASH.iter().enumerate().filter_map(|(i, c)| crate::commands::fuzzy(query, c.name).map(|(score, _)| (score, i, c))).collect();
    hits.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
    hits.into_iter().map(|(_, _, c)| c).collect()
}

/// The ⋮ menu's rows, in order.
const MENU_ROWS: usize = 6;

/// The card above the composer (kept while it animates away).
#[derive(Clone)]
enum Card {
    /// The first pending approval and how many more wait.
    Approval(otto_proto::PendingApproval, usize),
    Question(otto_proto::PendingQuestion),
}

impl Card {
    fn id(&self) -> &str {
        match self {
            Self::Approval(a, _) => &a.approval_id,
            Self::Question(q) => &q.question_id,
        }
    }
}

/// Choices for sessions this pane creates (otto has no method to change an
/// existing session's agent or model; approval mode applies to both).
#[derive(Clone, Debug, Default)]
struct Choice {
    agent: Option<String>,
    model: Option<(String, String)>,
    mode: ApprovalMode,
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
    key_field: Entity<pill_input::PillInput>,
    code_field: Entity<pill_input::PillInput>,
    choice: Choice,
    overlay: Option<Overlay>,
    /// The overlay drawn while it fades out, and its open/close motion.
    overlay_shown: Option<Overlay>,
    overlay_anim: crate::motion::Toggle,
    /// Esc closed the slash menu; it stays closed until the `/` goes.
    slash_dismissed: bool,
    selected: usize,
    signin: SignIn,
    status: Option<AgentStatus>,
    title: String,
    /// The header title last announced with [`AgentPaneEvent::Header`].
    header_seen: Option<Option<String>>,
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
    /// Images attached to the next prompt.
    attachments: Vec<Attachment>,
    tray_anim: crate::motion::Toggle,
    tray_shown: Option<Instant>,
    /// The send circle lights up when there is something to send (or stop).
    send_ready: crate::motion::Toggle,
    /// The composer area is shown (rises in after sign-in).
    bottom_shown: crate::motion::Toggle,
    card: Option<Card>,
    card_anim: crate::motion::Toggle,
    /// Expand/collapse motion of folded groups (`grp:`) and details (`det:`), with their row's key.
    anims: HashMap<String, (String, crate::motion::Toggle)>,
    /// The `↓ New activity` pill.
    unseen_anim: crate::motion::Toggle,
    /// Last status of each line and when it changed (the icon swaps in).
    line_status: HashMap<String, (transcript::Status, Instant)>,
    /// What the body shows (connecting, sign-in, empty, transcript…) and since when.
    body_mode: (u8, Option<Instant>),
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
        let editor = cx.new(|cx| {
            let mut e = Editor::new("Ask anything", cx);
            e.animate_growth = true;
            e
        });
        let filter = cx.new(|cx| TextField::new("Filter", cx));
        let key_field = cx.new(|cx| pill_input::PillInput::new("Paste an API key", "key", cx).masked(cx));
        let code_field = cx.new(|cx| pill_input::PillInput::new("Paste the code", "key", cx));
        let subscriptions = vec![
            cx.observe(&editor, |_, _, cx| cx.notify()),
            cx.subscribe_in(&editor, window, |this, _, event: &EditorEvent, window, cx| this.editor_event(event, window, cx)),
            cx.subscribe(&key_field, |this, _, event: &pill_input::PillInputEvent, cx| this.dialog_field_event(event, cx)),
            cx.subscribe(&code_field, |this, _, event: &pill_input::PillInputEvent, cx| this.dialog_field_event(event, cx)),
            cx.subscribe_in(&filter, window, |this, _, event: &FieldEvent, window, cx| this.filter_event(event, window, cx)),
            cx.observe_global::<client::AuthChanged>(Self::reload_catalog),
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
            overlay_shown: None,
            overlay_anim: crate::motion::Toggle::new(false, crate::motion::UI_FAST),
            slash_dismissed: false,
            selected: 0,
            signin: SignIn::default(),
            status: None,
            title: String::new(),
            header_seen: None,
            primed: false,
            entered: HashMap::new(),
            live_groups: HashSet::new(),
            fold_at: HashMap::new(),
            toggled: HashSet::new(),
            details: HashSet::new(),
            frozen: None,
            marks: None,
            attachments: Vec::new(),
            tray_anim: crate::motion::Toggle::new(false, crate::motion::UI),
            tray_shown: None,
            send_ready: crate::motion::Toggle::new(false, crate::motion::UI_FAST),
            bottom_shown: crate::motion::Toggle::new(false, crate::motion::UI),
            card: None,
            card_anim: crate::motion::Toggle::new(false, crate::motion::UI),
            anims: HashMap::new(),
            unseen_anim: crate::motion::Toggle::new(false, crate::motion::UI_FAST),
            line_status: HashMap::new(),
            body_mode: (0, None),
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

    /// The pane header's title on the chat screen (`New session` before the
    /// first prompt); `None` on sign-in and Providers (no session controls).
    pub fn header_title(&self) -> Option<String> {
        self.composer_visible().then(|| {
            self.mirror()
                .map(|m| m.info.title.clone())
                .filter(|t| !t.trim().is_empty())
                .or_else(|| (!self.title.is_empty()).then(|| self.title.clone()))
                // Until otto names it: the first prompt.
                .or_else(|| {
                    self.rows.iter().find_map(|r| {
                        if let Row::User { text, .. } = r { Some(transcript::clip(text.lines().next().unwrap_or(""), 60)) } else { None }
                    })
                })
                .unwrap_or_else(|| "New session".into())
        })
    }

    /// The header's title / History button.
    pub fn open_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overlay(Overlay::Sessions, window, cx);
    }

    /// The header's New Session button.
    pub fn start_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_session(window, cx);
        cx.notify();
    }

    pub fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.composer_visible() {
            // Still connecting: focus it once it appears (unless sign-in shows instead).
            self.signin.focus_composer = self.catalog.is_none();
            // Meanwhile the pane itself holds focus (its block is the active one).
            if !self.focus.contains_focused(window, cx) {
                window.focus(&self.focus, cx);
            }
            return;
        }
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
        self.anims.clear();
        self.line_status.clear();
        self.card = None;
        self.card_anim = crate::motion::Toggle::new(false, crate::motion::UI);
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
            EditorEvent::Submit => {
                if matches!(self.overlay, Some(Overlay::Slash | Overlay::Agents | Overlay::Menu)) && self.overlay_count(cx) > 0 {
                    self.activate_choice(self.selected, window, cx);
                } else {
                    self.set_overlay(None, cx);
                    self.submit(window, cx);
                }
            }
            EditorEvent::Escape => {
                if self.overlay == Some(Overlay::Slash) {
                    self.slash_dismissed = true;
                    self.set_overlay(None, cx);
                } else if self.overlay.is_some() {
                    self.close_overlay(window, cx);
                } else if self.mirror().is_some_and(|m| !m.pending_approvals.is_empty()) {
                    self.answer_approval(ApprovalDecision::Deny, cx);
                }
            }
            EditorEvent::Changed => {
                self.notice = None;
                self.sync_slash(cx);
            }
            EditorEvent::Nav(nav) => {
                let count = self.overlay_count(cx);
                match nav {
                    Nav::Up => self.selected = self.selected.checked_sub(1).unwrap_or(count.saturating_sub(1)),
                    Nav::Down => self.selected = if count == 0 { 0 } else { (self.selected + 1) % count },
                    Nav::Tab if count > 0 => return self.activate_choice(self.selected, window, cx),
                    Nav::Tab => {}
                }
                cx.notify();
            }
        }
    }

    /// Open, filter or close the slash menu as the composer's text changes.
    fn sync_slash(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_owned();
        let query = slash_query(&text);
        if query.is_none() {
            self.slash_dismissed = false;
        }
        let open = query.is_some() && !self.slash_dismissed && self.composer_visible();
        if open {
            if self.overlay != Some(Overlay::Slash) {
                self.set_overlay(Some(Overlay::Slash), cx);
            }
            self.selected = 0;
        } else if self.overlay == Some(Overlay::Slash) {
            self.set_overlay(None, cx);
        }
        cx.notify();
    }

    fn slash_choices(&self, cx: &gpui::App) -> Vec<&'static SlashCommand> {
        slash_query(self.editor.read(cx).text()).map(slash_matches).unwrap_or_default()
    }

    fn run_slash(&mut self, act: SlashAct, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| e.set_text("", cx));
        self.set_overlay(None, cx);
        match act {
            SlashAct::New => self.new_session(window, cx),
            SlashAct::Clear => {
                self.attachments.clear();
                self.new_session(window, cx);
            }
            SlashAct::Sessions => self.open_overlay(Overlay::Sessions, window, cx),
            SlashAct::Model => self.open_overlay(Overlay::Models, window, cx),
            SlashAct::Agent => self.open_overlay(Overlay::Agents, window, cx),
            SlashAct::Mode(mode) => self.set_mode(mode, cx),
            SlashAct::Providers => self.open_signin(window, cx),
        }
        cx.notify();
    }

    /// Agents offered by the picker (name, description).
    fn agent_choices(&self) -> Vec<(String, String)> {
        let agents: Vec<(String, String)> =
            self.catalog.as_ref().map(|c| c.agents.iter().map(|a| (a.name.clone(), a.description.clone())).collect()).unwrap_or_default();
        if agents.is_empty() { vec![("build".into(), String::new()), ("plan".into(), String::new())] } else { agents }
    }

    /// Rows of the open overlay, for keyboard selection.
    fn overlay_count(&self, cx: &gpui::App) -> usize {
        match self.overlay {
            Some(Overlay::Sessions) => self.session_choices(cx).len() + 1,
            Some(Overlay::Models) => self.model_choices(cx).len(),
            Some(Overlay::Agents) => self.agent_choices().len(),
            Some(Overlay::Menu) => MENU_ROWS,
            Some(Overlay::Slash) => self.slash_choices(cx).len(),
            None => 0,
        }
    }

    /// The one way overlays change: the shown one, its motion and the composer's key routing.
    fn set_overlay(&mut self, overlay: Option<Overlay>, cx: &mut Context<Self>) {
        let now = Instant::now();
        let reduce = cx.reduce_motion();
        if let Some(o) = overlay {
            if self.overlay_shown != Some(o) || !self.overlay_anim.on() {
                self.overlay_anim = crate::motion::Toggle::new(false, crate::motion::UI_FAST);
            }
            self.overlay_shown = Some(o);
            self.overlay_anim.set(true, now, reduce);
        } else {
            self.overlay_anim.set(false, now, reduce);
        }
        self.overlay = overlay;
        let nav = matches!(overlay, Some(Overlay::Slash | Overlay::Agents | Overlay::Menu));
        self.editor.update(cx, |e, _| e.nav = nav);
        cx.notify();
    }

    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().trim().to_owned();
        let question = self.mirror().and_then(|m| m.pending_questions.first()).map(|q| q.question_id.clone());
        if let (Some(question), false) = (question, text.is_empty()) {
            self.editor.update(cx, |e, cx| e.set_text("", cx));
            self.answer_question(question, text, cx);
            return;
        }
        if text.is_empty() && self.attachments.is_empty() {
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
        let mut input: Vec<InputPart> =
            std::mem::take(&mut self.attachments).into_iter().map(|a| InputPart::Image { mime: a.mime, data_base64: a.data }).collect();
        if !text.is_empty() {
            input.push(InputPart::Text { text: text.clone() });
        }
        if self.error.is_some() {
            // The shown session can't be opened: this prompt starts a new one.
            self.session_id = None;
        }
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
        self.set_overlay(None, cx);
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
        self.set_overlay(None, cx);
        if self.session_id.is_some() {
            self.session_id = None;
            self.follow(cx);
        }
        self.focus_composer(window, cx);
    }

    fn pick_session(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.set_overlay(None, cx);
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
        self.set_overlay(Some(overlay), cx);
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
            Overlay::Agents | Overlay::Menu | Overlay::Slash => self.focus_composer(window, cx),
        }
        cx.notify();
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay == Some(Overlay::Slash) {
            self.slash_dismissed = true;
        }
        self.set_overlay(None, cx);
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn filter_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.overlay, Some(Overlay::Sessions | Overlay::Models)) {
            return;
        }
        let count = self.overlay_count(cx);
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
            Some(Overlay::Agents) => {
                if let Some((name, _)) = self.agent_choices().into_iter().nth(index) {
                    self.choice.agent = Some(name);
                }
                self.close_overlay(window, cx);
            }
            Some(Overlay::Menu) => match index {
                0 => self.set_mode(ApprovalMode::Auto, cx),
                1 => self.set_mode(ApprovalMode::AskOnEffects, cx),
                2 => self.set_mode(ApprovalMode::Plan, cx),
                3 => self.new_session(window, cx),
                4 => self.open_overlay(Overlay::Sessions, window, cx),
                _ => self.open_signin(window, cx),
            },
            Some(Overlay::Slash) => {
                if let Some(c) = self.slash_choices(cx).get(index) {
                    self.run_slash(c.act, window, cx);
                }
            }
            None => {}
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

    /// Models matching the filter, grouped by provider (catalog order), best match first within a group.
    fn model_choices(&self, cx: &gpui::App) -> Vec<&otto_proto::ModelInfo> {
        let query = self.filter.read(cx).text.clone();
        let Some(catalog) = &self.catalog else { return Vec::new() };
        let mut providers: Vec<&str> = Vec::new();
        for m in catalog.usable() {
            if !providers.contains(&m.provider.as_str()) {
                providers.push(m.provider.as_str());
            }
        }
        let mut list: Vec<(usize, i32, &otto_proto::ModelInfo)> = catalog
            .usable()
            .filter_map(|m| {
                let group = providers.iter().position(|p| *p == m.provider.as_str()).unwrap_or(0);
                crate::commands::fuzzy(&query, &format!("{} {} {}", m.provider, m.id, m.name)).map(|(score, _)| (group, score, m))
            })
            .collect();
        if query.trim().is_empty() {
            list.sort_by_key(|(group, _, _)| *group);
        } else {
            list.sort_by_key(|(group, score, _)| (*group, std::cmp::Reverse(*score)));
        }
        list.into_iter().map(|(_, _, m)| m).collect()
    }

    /// Pick images for the next prompt.
    fn attach_image(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Attach".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let read = cx.background_executor().spawn(async move { paths.iter().filter_map(|p| Attachment::read(p)).collect::<Vec<_>>() });
            let images = read.await;
            let _ = this.update(cx, |this, cx| {
                this.attachments.extend(images);
                cx.notify();
            });
        })
        .detach();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        if k.key == "j" && k.modifiers.platform && !k.modifiers.shift && !k.modifiers.alt && !k.modifiers.control {
            self.open_overlay(Overlay::Sessions, window, cx);
            cx.stop_propagation();
        } else if k.key == "escape" && self.overlay.is_some() {
            self.close_overlay(window, cx);
            cx.stop_propagation();
        } else if k.key == "escape" && self.signin_escape(window, cx) {
            cx.stop_propagation();
        }
    }
}

/// An image for the next prompt.
struct Attachment {
    name: String,
    mime: String,
    data: String,
    /// Added at (the chip fades in).
    at: Instant,
}

impl Attachment {
    fn read(path: &std::path::Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        let mime = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => return None,
        };
        let bytes = std::fs::read(path).ok()?;
        let name = path.file_name()?.to_string_lossy().into_owned();
        Some(Self { name, mime: mime.to_owned(), data: data_encoding::BASE64.encode(&bytes), at: Instant::now() })
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
        // otto's hint names its CLI; in the app the way is the Providers view.
        ClientError::Rpc(e) if e.message.starts_with("Auth:") => "Its provider isn't connected. Connect it with /providers.".to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_menu_filters_commands() {
        assert_eq!(slash_query("/mo"), Some("mo"));
        assert_eq!(slash_query("/"), Some(""));
        assert_eq!(slash_query("hi /new"), None);
        assert_eq!(slash_query("/new\nmore"), None);
        assert_eq!(slash_query("/ x"), None);
        assert_eq!(slash_matches("").len(), SLASH.len());
        let names = |q: &str| slash_matches(q).into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names("new")[0], "new");
        assert!(names("mode").iter().all(|n| n.starts_with("mode") || n.starts_with("model")));
        assert_eq!(names("mode plan")[0], "mode plan");
        assert!(names("zzz").is_empty());
        for c in &SLASH {
            assert!(crate::assets::has_icon(c.icon), "{}", c.icon);
        }
    }
}
