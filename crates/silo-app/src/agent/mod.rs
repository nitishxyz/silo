//! Agent block: one otto session shown as a chat (`docs/AGENT_BLOCK.md`).
//!
//! Contract with the workspace wiring: [`AgentPane::new`], [`AgentPane::show`],
//! [`AgentPane::focus_composer`], [`AgentPaneEvent`], `focus`, `block_id`.
//! `focus` tracks the whole pane; the composer and the pane's fields are
//! children of it, so "the pane is focused" is `focus.contains_focused(..)`.
//! ⌘J (with focus in the pane) opens the session switcher.

mod attach;
mod client;
mod editor;
mod markdown;
mod pill_input;
mod reveal;
mod rows;
mod scroll;
mod signin;
mod subagents;
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
use otto_proto::{ApprovalDecision, ApprovalMode, Delivery, Effort, InputPart, Method, ProjectInfo, SessionInfo, TurnStatus};

use crate::text_field::{FieldEvent, TextField};
use attach::{Attachment, upload};
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
    /// The user moved the block to another folder (`agent.set` `cwd`): it shows a new session there.
    Project {
        cwd: String,
    },
    Status(AgentStatus),
    /// Jump to a silo block named by a tool call; a parked one is revealed as a float.
    Reveal(String),
    /// Stop a command by closing its block.
    Stop(String),
    /// Show this otto session in its own agent block: the one already showing
    /// it, else a new one split beside this pane (a sub-agent, or its parent).
    OpenSession {
        session_id: String,
        title: String,
    },
    /// [`AgentPane::header_title`] changed: redraw the pane header.
    Header,
}

/// Entry motion of rows and lines (`docs/AGENT_BLOCK.md` §6).
const ENTER: Duration = Duration::from_millis(320);
/// A finished group stays open this long before it folds.
const FOLD_DELAY: Duration = Duration::from_millis(500);
const FOLD: Duration = Duration::from_millis(320);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overlay {
    Sessions,
    /// Recent projects, opened from the switcher's project row.
    Projects,
    Models,
    Agents,
    /// Reasoning effort, from the brain.
    Effort,
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
    Effort,
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

const SLASH: [SlashCommand; 10] = [
    SlashCommand { name: "new", about: "Start a new session", icon: "new-session", act: SlashAct::New },
    SlashCommand { name: "sessions", about: "Switch to another session", icon: "history", act: SlashAct::Sessions },
    SlashCommand { name: "model", about: "Choose the model", icon: "bot", act: SlashAct::Model },
    SlashCommand { name: "effort", about: "Choose the reasoning effort", icon: "brain", act: SlashAct::Effort },
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

/// The effort menu's rows; `None` is the default (the agent's, else the model's).
const EFFORTS: [Option<Effort>; 5] = [None, Some(Effort::Low), Some(Effort::Medium), Some(Effort::High), Some(Effort::Max)];

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

/// Choices for the session this pane creates next (the shown session
/// changes with `session.update` / `session.set_mode` instead).
#[derive(Clone, Debug, Default)]
struct Choice {
    agent: Option<String>,
    model: Option<(String, String)>,
    mode: ApprovalMode,
    effort: Option<Effort>,
}

/// `session.update`s of the shown session not yet reflected by otto (shown meanwhile).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Update {
    agent: Option<String>,
    model: Option<(String, String)>,
    /// `Some(None)`: back to the default.
    effort: Option<Option<Effort>>,
}

impl Update {
    /// Drop what `info` already shows.
    fn reconcile(&mut self, info: &SessionInfo) {
        self.agent.take_if(|a| *a == info.agent);
        self.model.take_if(|(p, m)| *p == info.provider && *m == info.model);
        self.effort.take_if(|e| *e == info.effort);
    }
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
    /// A folder this pane moved to that the server hasn't echoed yet.
    moving: Option<String>,
    rt: tokio::runtime::Handle,
    epoch: Instant,
    conn: Conn,
    catalog: Option<Catalog>,
    project: Option<ProjectInfo>,
    /// Recent otto projects, most recent first (the project list).
    projects: Vec<ProjectInfo>,
    sessions: Vec<SessionInfo>,
    follower: Option<SessionFollower>,
    pump: Option<Task<()>>,
    generation: u64,
    state: Option<FollowerState>,
    /// Loading the shown session failed.
    error: Option<String>,
    /// A request failed (prompt, approval, …); shown above the composer.
    notice: Option<String>,
    /// The composer's text was just put back after a failed send (its change keeps the notice).
    restored: bool,
    rows: Vec<Row>,
    /// The transcript as otto has it (`rows` adds the optimistic bubble).
    built: Vec<Row>,
    /// A prompt sent but not yet recorded by otto.
    optimistic: Option<Pending>,
    /// Steers sent but not yet in otto's queue (or landed): the queue bar shows them meanwhile.
    steers: Vec<Pending>,
    /// Queue edits sent but not yet reflected by otto: removed (`None`) or a new delivery.
    queue_edits: HashMap<String, Option<Delivery>>,
    /// Sent at, until the turn is seen running.
    sent_at: Option<Instant>,
    todos: Vec<Todo>,
    times: Times,
    list: ListState,
    /// The pane's width as last laid out: narrow panes drop secondary items.
    width: std::rc::Rc<std::cell::Cell<f32>>,
    /// The composer column's height as last laid out (0 when hidden), and the one the list was last padded for:
    /// the column floats over the transcript, whose end is padded clear of it.
    composer_h: std::rc::Rc<std::cell::Cell<f32>>,
    composer_pad: f32,
    /// Something changed below while scrolled up.
    unseen: bool,
    editor: Entity<Editor>,
    filter: Entity<TextField>,
    key_field: Entity<pill_input::PillInput>,
    code_field: Entity<pill_input::PillInput>,
    /// A local or custom provider's address (the connect dialog).
    url_field: Entity<pill_input::PillInput>,
    choice: Choice,
    update: Update,
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
    /// Line reveal of streaming text rows, by row key (kept until the last line faded in).
    reveals: HashMap<String, reveal::Reveal>,
    /// Scroll chaining of the boxes inside the transcript (details, code blocks).
    scroll: std::rc::Rc<scroll::Inner>,
    /// Files attached to the next prompt.
    attachments: Vec<Attachment>,
    /// Thumbnails of the transcript's images by `item#part` (`None` while made).
    thumbs: HashMap<String, Option<std::sync::Arc<gpui::Image>>>,
    /// Bars above the composer by key: expanded motion, and when each appeared.
    bars: HashMap<&'static str, crate::motion::Toggle>,
    bar_shown: HashMap<&'static str, Instant>,
    /// Subagent calls seen (the session list is refreshed when they change).
    subagent_calls: usize,
    subs: subagents::Subs,
    /// The send circle lights up when there is something to send.
    send_ready: crate::motion::Toggle,
    /// The activity row at the transcript's end (`Thinking…` + Stop).
    activity: crate::motion::Toggle,
    /// The composer area is shown (rises in after sign-in).
    bottom_shown: crate::motion::Toggle,
    card: Option<Card>,
    card_anim: crate::motion::Toggle,
    /// Expand/collapse motion of folded groups (`grp:`) and details (`det:`), with their row's key.
    anims: HashMap<String, (String, crate::motion::Toggle)>,
    /// The `↓ New activity` pill.
    unseen_anim: crate::motion::Toggle,
    /// Last status of each line, when it changed and the one before it (the icons crossfade).
    line_status: HashMap<String, (transcript::Status, Instant, Option<transcript::Status>)>,
    /// Streaming reasoning under a `Thinking` line: eases its height as it grows and when it ends.
    streams: HashMap<String, view::Stream>,
    /// What the body shows (connecting, sign-in, empty, transcript…) and since when.
    body_mode: (u8, Option<Instant>),
    /// When the running turn was first seen (pane ms).
    running_since: Option<u64>,
    running: bool,
    /// The shown session was created by this pane (its steps are timed from the start).
    fresh: bool,
    usage_seen: Option<u64>,
    context: Option<u64>,
    /// Parsed assistant text by row key (only a streaming text's open block parses again).
    parsed: HashMap<String, markdown::Doc>,
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
            e.files = true;
            e
        });
        let filter = cx.new(|cx| TextField::new("Filter", cx));
        let key_field = cx.new(|cx| pill_input::PillInput::new("Paste an API key", "key", cx).masked(cx));
        let code_field = cx.new(|cx| pill_input::PillInput::new("Paste the code", "key", cx));
        let url_field = cx.new(|cx| pill_input::PillInput::new("Server address", "globe", cx));
        let subscriptions = vec![
            cx.observe(&editor, |_, _, cx| cx.notify()),
            cx.subscribe_in(&editor, window, |this, _, event: &EditorEvent, window, cx| this.editor_event(event, window, cx)),
            cx.subscribe(&key_field, |this, _, event: &pill_input::PillInputEvent, cx| this.dialog_field_event(event, cx)),
            cx.subscribe(&code_field, |this, _, event: &pill_input::PillInputEvent, cx| this.dialog_field_event(event, cx)),
            cx.subscribe(&url_field, |this, _, event: &pill_input::PillInputEvent, cx| this.dialog_field_event(event, cx)),
            cx.subscribe_in(&filter, window, |this, _, event: &FieldEvent, window, cx| this.filter_event(event, window, cx)),
            cx.observe_global::<client::AuthChanged>(Self::reload_catalog),
            cx.on_focus_in(&focus, window, |_, _, cx| cx.emit(AgentPaneEvent::Activated)),
            // The pane itself has nothing to type into: focus lands on the composer.
            cx.on_focus(&focus, window, |this, window, cx| this.focus_composer(window, cx)),
        ];
        // The rows, then the activity row.
        let list = ListState::new(1, ListAlignment::Top, px(600.0));
        list.set_follow_mode(gpui::FollowMode::Tail);
        let scroll = scroll::Inner::new(list.clone());
        let mut pane = Self {
            block_id,
            focus,
            session_id,
            cwd,
            moving: None,
            rt,
            width: std::rc::Rc::default(),
            composer_h: std::rc::Rc::default(),
            composer_pad: 0.0,
            // A day back, so what started before the pane opened (a turn, a call) still has a time on its clock.
            epoch: Instant::now().checked_sub(Duration::from_secs(86_400)).unwrap_or_else(Instant::now),
            conn: Conn::Connecting,
            catalog: None,
            project: None,
            projects: Vec::new(),
            sessions: Vec::new(),
            follower: None,
            pump: None,
            generation: 0,
            state: None,
            error: None,
            notice: None,
            restored: false,
            rows: Vec::new(),
            built: Vec::new(),
            optimistic: None,
            steers: Vec::new(),
            queue_edits: HashMap::new(),
            sent_at: None,
            todos: Vec::new(),
            times: Times::default(),
            list,
            unseen: false,
            editor,
            filter,
            key_field,
            code_field,
            url_field,
            choice: Choice::default(),
            update: Update::default(),
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
            reveals: HashMap::new(),
            scroll,
            attachments: Vec::new(),
            thumbs: HashMap::new(),
            bars: HashMap::new(),
            bar_shown: HashMap::new(),
            subagent_calls: 0,
            subs: subagents::Subs::default(),
            send_ready: crate::motion::Toggle::new(false, crate::motion::UI_FAST),
            activity: crate::motion::Toggle::new(false, crate::motion::UI),
            bottom_shown: crate::motion::Toggle::new(false, crate::motion::UI),
            card: None,
            card_anim: crate::motion::Toggle::new(false, crate::motion::UI),
            anims: HashMap::new(),
            unseen_anim: crate::motion::Toggle::new(false, crate::motion::UI_FAST),
            line_status: HashMap::new(),
            streams: HashMap::new(),
            body_mode: (0, None),
            running_since: None,
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
        // State from before this pane's own move.
        if self.moving.is_some() && self.moving != cwd {
            return;
        }
        self.moving = None;
        if self.cwd != cwd {
            // Moved to another folder: its project, and the session the server names (none: a new one).
            self.cwd = cwd;
            self.project = None;
            self.sessions.clear();
            if self.session_id != session_id {
                self.forget_sends();
                self.session_id = session_id;
                self.follow(cx);
            }
            self.load(cx);
            return;
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

    /// Move the block to `cwd` (a project root or a picked folder): a new session there.
    fn switch_project(&mut self, cwd: String, window: &mut Window, cx: &mut Context<Self>) {
        self.set_overlay(None, cx);
        // The current project (picked from the list) stays as it is.
        if self.cwd.as_deref() != Some(cwd.as_str()) && self.project.as_ref().is_none_or(|p| p.root != cwd) {
            self.forget_sends();
            self.cwd = Some(cwd.clone());
            self.moving = Some(cwd.clone());
            self.project = None;
            self.sessions.clear();
            self.session_id = None;
            self.follow(cx);
            self.load(cx);
            cx.emit(AgentPaneEvent::Project { cwd });
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// "Open folder…": the native folder picker, then [`Self::switch_project`].
    fn open_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_overlay(window, cx);
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Open".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(dir) = paths.into_iter().next().and_then(|p| p.to_str().map(str::to_owned)) else { return };
            let _ = this.update_in(cx, |this, window, cx| this.switch_project(dir, window, cx));
        })
        .detach();
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

    /// The pane's clock: wall time (ms since the Unix epoch), the clock otto
    /// records in, so live and recorded times are one timeline.
    fn now(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    /// A time otto reports (ms since the Unix epoch) on the pane's clock.
    fn pane_ms(at: i64) -> u64 {
        u64::try_from(at).unwrap_or(0)
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
            work(connection).await
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
        let asked = cwd.clone();
        self.run(
            cx,
            move |c| async move {
                let catalog = client::catalog(&c).await?;
                let project = client::project(&c, cwd.as_deref()).await?;
                let sessions = c.session_list(Some(project.id.clone()), false).await?;
                Ok((catalog, project, sessions))
            },
            move |this, result, cx| match result {
                // Moved again meanwhile: the newer load wins.
                _ if this.cwd != asked => {}
                Ok((catalog, project, sessions)) => {
                    let first = matches!(this.conn, Conn::Connecting | Conn::Failed(_));
                    this.conn = Conn::Ready;
                    this.catalog = Some(catalog);
                    this.project = Some(project);
                    this.sessions = sessions;
                    this.watch_sessions(cx);
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
        let asked = project.clone();
        self.run(
            cx,
            move |c| async move { c.session_list(Some(project), false).await },
            move |this, result, cx| {
                if let Ok(mut sessions) = result
                    && this.project.as_ref().is_some_and(|p| p.id == asked)
                {
                    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
                    this.sessions = sessions;
                    this.track_subagents();
                    cx.notify();
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
        self.built.clear();
        self.todos.clear();
        self.list.reset(1);
        self.activity = crate::motion::Toggle::new(false, crate::motion::UI);
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
        self.streams.clear();
        self.card = None;
        self.card_anim = crate::motion::Toggle::new(false, crate::motion::UI);
        self.reveals.clear();
        self.scroll.clear();
        self.parsed.clear();
        self.usage_seen = None;
        self.context = None;
        self.title.clear();
        self.reset_subagents();
        self.update = Update::default();
        self.set_running(false, None);
        // A prompt that is creating this session keeps its bubble.
        self.show_rows();
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

    /// `started`: when otto says the turn started (a turn already running when the session loaded).
    fn set_running(&mut self, running: bool, started: Option<u64>) {
        // A turn this pane started is timed from the send.
        let sent = running && self.sent_at.take().is_some();
        if self.running != running {
            self.running = running;
            self.subs.turn(running, Instant::now());
            if running && !sent {
                self.running_since = Some(started.unwrap_or_else(|| self.now()));
            }
        }
    }

    /// A new projection of the shown session.
    fn apply(&mut self, state: FollowerState, cx: &mut Context<Self>) {
        let now = self.now();
        let m = &state.mirror;
        let turn = transcript::effective_turn(m.turn.map(|t| t.1), m.info.status);
        let running = turn == Some(TurnStatus::Running);
        let waiting =
            m.pending_approvals.iter().map(|a| (a.call_id.clone(), false)).chain(m.pending_questions.iter().map(|q| (q.call_id.clone(), true)));
        // Calls already running when the session loaded keep otto's start, so their clocks match the parent's.
        let starts: Vec<(String, u64)> = m.tools.iter().filter_map(|(id, t)| t.started_at.map(|at| (id.clone(), Self::pane_ms(at)))).collect();
        self.times.seed(starts);
        // Steps that ran before the session loaded: otto's recorded times.
        self.times.record(&m.items, &m.part_times);
        self.times.observe(&m.items, &m.tools, running, waiting.collect(), now);
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
        let mut todos = transcript::todos(&m.items, &m.tools, &self.times);
        for todo in &mut todos {
            todo.step = transcript::display(&todo.step, cwd.as_deref());
        }
        // Subagents are child sessions: their status comes from the session list
        // (refreshed on otto's `sessions.changed` too).
        let calls = m.tools.values().filter(|t| t.name == "subagent").count();
        if calls != self.subagent_calls || self.running != running {
            self.subagent_calls = calls;
            self.refresh_sessions(cx);
        }
        self.todos = todos;
        self.built = rows;
        self.show_rows();
        // A turn that ended before it was seen running: its reply is there.
        if self.optimistic.is_none() && !running && !matches!(self.built.last(), Some(Row::User { .. }) | None) {
            self.sent_at = None;
        }
        let status = transcript::agent_status(m.turn.map(|t| t.1), m.info.status, !m.pending_approvals.is_empty(), !m.pending_questions.is_empty());
        // A command waiting for input in its block needs the user once the turn is over.
        let input = || self.built.iter().any(|r| matches!(r, Row::Group(g) if g.lines.iter().any(|l| l.status == transcript::Status::Input)));
        let status =
            if matches!(status, AgentStatus::Done | AgentStatus::Idle) && input() { AgentStatus::Blocked { question: false } } else { status };
        let status = if !self.primed && status == AgentStatus::Done { AgentStatus::Idle } else { status };
        self.set_status(status, cx);
        self.set_running(running, m.turn_started_at.map(Self::pane_ms));
        self.update.reconcile(&m.info);
        if !m.info.title.is_empty() && m.info.title != self.title {
            self.title.clone_from(&m.info.title);
            // otto names a session after its first turn: the switcher's row follows at once.
            if let Some(s) = self.sessions.iter_mut().find(|s| s.id == m.info.id) {
                s.title.clone_from(&m.info.title);
            }
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
        // A row that changed in place (a live group's new line) is remeasured: a splice would drop the scroll anchor.
        let change = transcript::list_change(&old, &self.rows);
        let changed = !change.remeasure.is_empty() || change.splice.is_some();
        if !change.remeasure.is_empty() {
            self.list.remeasure_items(change.remeasure);
        }
        if let Some((range, count)) = change.splice {
            self.list.splice(range, count);
        }
        // New below only when the user is away from the bottom (not merely between the list's follow updates).
        if changed && !self.tail().at_end && self.primed {
            self.unseen = true;
        }
        let primed = self.primed;
        // A live group taking the activity row's place keeps its header where the row was: it doesn't
        // enter, the row hides at once and only the group's lines ease in below it.
        let handoff = (primed && self.activity.on())
            .then(|| match self.rows.last() {
                Some(Row::Group(g)) if g.live && !old.iter().any(|o| o.key() == g.key) => Some(g.key.clone()),
                _ => None,
            })
            .flatten();
        if primed {
            for key in transcript::arrivals(&old, &self.rows, |k| self.entered.contains_key(k)) {
                if handoff.as_ref() != Some(&key) {
                    self.entered.insert(key, now);
                }
            }
        }
        if handoff.is_some() {
            self.activity = crate::motion::Toggle::new(false, crate::motion::UI);
        }
        for row in &self.rows {
            match row {
                Row::Group(g) if g.live => {
                    self.live_groups.insert(g.key.clone());
                }
                Row::Group(g) => {
                    if self.live_groups.remove(&g.key) {
                        self.fold_at.insert(g.key.clone(), now + FOLD_DELAY);
                        // Folded lines never stream again.
                        self.streams.retain(|k, _| !g.lines.iter().any(|l| l.key == *k));
                    }
                }
                // A message streaming before it was seen shows what's there at once.
                Row::Text { key, streaming: true, .. } => {
                    self.reveals.entry(key.clone()).or_insert_with(|| reveal::Reveal::new(!primed, now));
                }
                Row::Text { .. } | Row::User { .. } => {}
            }
        }
        let rows = &self.rows;
        self.reveals.retain(|k, _| rows.iter().any(|r| r.key() == k));
    }

    // ---- actions ----

    fn mirror(&self) -> Option<&otto_client::SessionMirror> {
        self.state.as_ref().map(|s| &s.mirror)
    }

    /// Text as shown (`transcript::display`) for the shown session's folder.
    fn shown(&self, text: &str) -> String {
        let cwd = self.mirror().map(|m| m.info.cwd.as_str()).filter(|c| !c.is_empty()).or(self.cwd.as_deref());
        transcript::display(text, cwd)
    }

    fn editor_event(&mut self, event: &EditorEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            EditorEvent::Submit => {
                if matches!(self.overlay, Some(Overlay::Slash | Overlay::Agents | Overlay::Effort | Overlay::Menu)) && self.overlay_count(cx) > 0 {
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
                } else if self.running && self.editor.read(cx).text().trim().is_empty() && self.attachments.is_empty() {
                    self.stop(cx);
                }
            }
            EditorEvent::Changed => {
                // A draft put back after a failed send keeps the notice saying why.
                if !std::mem::take(&mut self.restored) {
                    self.notice = None;
                }
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
            EditorEvent::Files(paths) => self.attach_paths(paths.clone(), cx),
            EditorEvent::Image(png) => {
                let png = png.clone();
                let read = cx.background_executor().spawn(async move { Attachment::new("Pasted image.png".into(), png) });
                cx.spawn(async move |this, cx| {
                    let attachment = read.await;
                    let _ = this.update(cx, |this, cx| this.add_attachments(vec![attachment], cx));
                })
                .detach();
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
            SlashAct::Effort => self.open_overlay(Overlay::Effort, window, cx),
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
            Some(Overlay::Sessions) => self.session_choices(cx).len() + 2,
            Some(Overlay::Projects) => self.project_choices(cx).len() + 1,
            Some(Overlay::Models) => self.model_choices(cx).len(),
            Some(Overlay::Agents) => self.agent_choices().len(),
            Some(Overlay::Effort) => EFFORTS.len(),
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
        let nav = matches!(overlay, Some(Overlay::Slash | Overlay::Agents | Overlay::Effort | Overlay::Menu));
        self.editor.update(cx, |e, _| e.nav = nav);
        cx.notify();
    }

    /// Change a pending input's delivery (`Some`) or withdraw it (`None`); the
    /// queue bar shows the edit at once and drops it if otto refuses.
    pub(super) fn edit_queued(&mut self, input_id: String, delivery: Option<Delivery>, cx: &mut Context<Self>) {
        let Some(id) = self.session_id.clone() else { return };
        self.queue_edits.insert(input_id.clone(), delivery);
        let key = input_id.clone();
        self.run(
            cx,
            move |c| async move {
                match delivery {
                    Some(delivery) => c.queue_update(&id, &input_id, delivery).await,
                    None => c.queue_remove(&id, &input_id).await,
                }
            },
            move |this, result, _| {
                if let Err(error) = result {
                    this.notice = Some(error);
                    this.queue_edits.remove(&key);
                }
            },
        );
    }

    /// The queue bar's rows: pending steers first (`Next step`), then inputs
    /// waiting for the turn to end (`After turn`), with edits in flight applied.
    pub(super) fn queue_rows(&self) -> Vec<QueueRow> {
        let queue = self.mirror().map(|m| m.queue.as_slice()).unwrap_or_default();
        // A steer otto already lists (by id, or by text before its id is known) shows once, as otto's.
        let mut listed: Vec<String> = queue.iter().map(|q| input_text(&q.input)).collect();
        let mut rows: Vec<QueueRow> = self
            .steers
            .iter()
            .filter(|s| {
                if s.id.as_ref().is_some_and(|id| queue.iter().any(|q| &q.id == id)) {
                    return false;
                }
                let Some(i) = listed.iter().position(|t| t.trim() == s.text) else { return true };
                listed.remove(i);
                false
            })
            .map(|s| QueueRow { id: None, text: s.text.clone(), steer: true })
            .collect();
        if let Some(m) = self.mirror() {
            for q in m.queue.iter().filter(|q| !q.input.is_empty()) {
                let delivery = match self.queue_edits.get(&q.id) {
                    Some(None) => continue,
                    Some(Some(d)) => *d,
                    None => q.delivery,
                };
                rows.push(QueueRow { id: Some(q.id.clone()), text: input_text(&q.input), steer: delivery == Delivery::Steer });
            }
        }
        rows.sort_by_key(|r| !r.steer);
        rows
    }

    /// An input on its way: shown until the session has one more user item with its text.
    fn pending(&self, text: &str) -> Pending {
        let text = text.trim().to_owned();
        let users = user_count(&self.built, &text);
        Pending { text, users, at: Instant::now(), id: None }
    }

    /// The transcript plus the optimistic bubble of a prompt otto hasn't recorded yet.
    fn show_rows(&mut self) {
        let users = |p: &Pending| user_count(&self.built, &p.text) > p.users;
        if let Some(p) = self.optimistic.take_if(|p| users(p)) {
            // The real item takes the bubble's place without fading in again.
            if let Some(Row::User { key, .. }) = self.built.iter().rev().find(|r| matches!(r, Row::User { text, .. } if *text == p.text)) {
                self.entered.remove(OPTIMISTIC);
                self.entered.insert(key.clone(), p.at.checked_sub(ENTER).unwrap_or(p.at));
            }
        }
        let built = &self.built;
        // A steer leaves the pane's list once it landed or otto's queue lists it.
        let queue = self.state.as_ref().map(|s| s.mirror.queue.as_slice()).unwrap_or_default();
        self.steers.retain(|p| user_count(built, &p.text) <= p.users && !p.id.as_ref().is_some_and(|id| queue.iter().any(|q| &q.id == id)));
        self.queue_edits.retain(|id, edit| queue.iter().any(|q| &q.id == id && Some(q.delivery) != *edit));
        let mut rows = self.built.clone();
        if let Some(p) = self.optimistic.as_ref().filter(|p| !p.text.is_empty()) {
            rows.push(Row::User { key: OPTIMISTIC.to_owned(), text: p.text.clone(), files: Vec::new() });
        }
        self.sync_rows(rows);
    }

    /// Sent, and the turn hasn't been seen running yet: the activity row shows at once.
    fn awaiting(&self) -> bool {
        self.sent_at.is_some_and(|at| at.elapsed() < AWAIT)
    }

    /// ⏎: sends; while a turn runs it steers (the agent reads it at its next step).
    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let delivery = if self.running { Delivery::Steer } else { Delivery::Queue };
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
        // A new session needs its project (still loading after a move): the text stays.
        if (self.session_id.is_none() || self.error.is_some()) && self.project.is_none() {
            return;
        }
        self.editor.update(cx, |e, cx| e.set_text("", cx));
        self.list.set_follow_mode(gpui::FollowMode::Tail);
        self.unseen = false;
        // Uploaded to the session first; the prompt names them (kept if the send fails).
        let files = std::mem::take(&mut self.attachments);
        let uploads: Vec<Attachment> = files.clone();
        let words = (!text.is_empty()).then(|| InputPart::Text { text: text.clone() });
        if self.error.is_some() {
            // The shown session can't be opened: this prompt starts a new one.
            self.session_id = None;
        }
        // Feedback at once: a steer waits in the queue bar until it lands; a
        // prompt shows its bubble and the activity row before otto confirms.
        if delivery == Delivery::Steer {
            self.steers.push(self.pending(&text));
        } else {
            self.optimistic = Some(self.pending(&text));
            self.entered.insert(OPTIMISTIC.to_owned(), Instant::now());
            self.sent_at = Some(Instant::now());
            self.running_since = Some(self.now());
            self.show_rows();
        }
        if let Some(id) = self.session_id.clone() {
            let steer = delivery == Delivery::Steer;
            self.run(
                cx,
                move |c| async move {
                    let input = upload(&c, &id, uploads, words).await?;
                    c.prompt(&id, input, delivery).await
                },
                move |this, result, cx| match result {
                    // Known by id from now on: it leaves the pane's list once otto's queue has it.
                    Ok(accepted) if steer => {
                        if let Some(p) = this.steers.iter_mut().find(|p| p.text == text && p.id.is_none()) {
                            p.id = Some(accepted.input_id);
                        }
                        this.show_rows();
                    }
                    Ok(_) => {}
                    Err(error) => this.unsend(text, files, error, cx),
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
            effort: self.choice.effort,
            ..SessionCreateOptions::default()
        };
        self.run(
            cx,
            move |c| async move {
                let info = c.session_create(options).await?;
                let input = upload(&c, &info.id, uploads, words).await?;
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
                Err(error) => this.unsend(text, files, error, cx),
            },
        );
    }

    /// A send failed: its optimistic feedback goes, the draft returns to the
    /// composer with its attachments and the notice says why.
    fn unsend(&mut self, text: String, files: Vec<Attachment>, error: String, cx: &mut Context<Self>) {
        self.steers.retain(|s| s.text != text);
        if self.optimistic.as_ref().is_some_and(|p| p.text == text) {
            self.optimistic = None;
            self.sent_at = None;
            self.show_rows();
        }
        self.notice = Some(error);
        self.restored = true;
        if self.attachments.is_empty() {
            self.attachments = files;
        }
        self.editor.update(cx, |e, cx| e.set_text(text, cx));
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.sent_at = None;
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

    /// The shown session, when the pane can change it (else picks go to the next new session).
    fn updatable(&self) -> Option<String> {
        self.session_id.clone().filter(|_| self.error.is_none() && self.state.is_some())
    }

    fn set_model(&mut self, model: (String, String), cx: &mut Context<Self>) {
        if self.updatable().is_none() {
            self.choice.model = Some(model);
            return;
        }
        let (provider, id) = model.clone();
        self.update.model = Some(model);
        self.update_session(otto_client::SessionUpdate { provider: Some(provider), model: Some(id), ..Default::default() }, cx);
    }

    fn set_agent(&mut self, agent: String, cx: &mut Context<Self>) {
        if self.updatable().is_none() {
            self.choice.agent = Some(agent);
            return;
        }
        self.update.agent = Some(agent.clone());
        self.update_session(otto_client::SessionUpdate { agent: Some(agent), ..Default::default() }, cx);
    }

    /// The effort menu: a level, or `None` for the default (the agent's, else the model's).
    pub(super) fn set_effort(&mut self, effort: Option<Effort>, cx: &mut Context<Self>) {
        if self.updatable().is_none() {
            self.choice.effort = effort;
            cx.notify();
            return;
        }
        self.update.effort = Some(effort);
        self.update_session(otto_client::SessionUpdate { effort, reset_effort: effort.is_none(), ..Default::default() }, cx);
    }

    /// `session.update` of the shown session; it shows at once, and otto's
    /// `session.changed` confirms it. A refusal drops it and says why.
    fn update_session(&mut self, update: otto_client::SessionUpdate, cx: &mut Context<Self>) {
        let Some(id) = self.updatable() else { return };
        let generation = self.generation;
        let sent = update.clone();
        self.run(
            cx,
            move |c| async move { c.session_update(&id, update).await },
            move |this, result, _| {
                if this.generation != generation {
                    return;
                }
                match result {
                    // Shown as otto has it until the session's own state catches up.
                    Ok(info) => {
                        if sent.agent.is_some() {
                            this.update.agent = Some(info.agent.clone());
                        }
                        if sent.model.is_some() {
                            this.update.model = Some((info.provider.clone(), info.model.clone()));
                        }
                        if sent.effort.is_some() || sent.reset_effort {
                            this.update.effort = Some(info.effort);
                        }
                        if let Some(state) = &this.state {
                            this.update.reconcile(&state.mirror.info);
                        }
                    }
                    Err(error) => {
                        this.notice = Some(error);
                        if sent.agent.is_some() {
                            this.update.agent = None;
                        }
                        if sent.model.is_some() {
                            this.update.model = None;
                        }
                        if sent.effort.is_some() || sent.reset_effort {
                            this.update.effort = None;
                        }
                    }
                }
            },
        );
        cx.notify();
    }

    /// The shown session's effort (with an update in flight), else the next session's.
    pub(super) fn effort(&self) -> Option<Effort> {
        match self.mirror() {
            Some(m) => self.update.effort.unwrap_or(m.info.effort),
            None => self.choice.effort,
        }
    }

    /// The agent the next prompt runs with.
    pub(super) fn agent_name(&self) -> String {
        self.update
            .agent
            .clone()
            .or_else(|| self.mirror().map(|m| m.info.agent.clone()))
            .or_else(|| self.choice.agent.clone())
            .unwrap_or_else(|| "build".into())
    }

    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_overlay(None, cx);
        self.forget_sends();
        if self.session_id.is_some() {
            self.session_id = None;
            self.follow(cx);
        }
        self.focus_composer(window, cx);
    }

    fn pick_session(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.set_overlay(None, cx);
        if self.session_id.as_deref() != Some(id.as_str()) {
            self.forget_sends();
            let title = self.sessions.iter().find(|s| s.id == id).map(|s| s.title.clone()).unwrap_or_default();
            self.session_id = Some(id.clone());
            self.follow(cx);
            cx.emit(AgentPaneEvent::Session { session_id: id, title });
        }
        self.focus_composer(window, cx);
    }

    /// Leaving the shown session: its in-flight feedback stays with it.
    fn forget_sends(&mut self) {
        self.optimistic = None;
        self.steers.clear();
        self.queue_edits.clear();
        self.sent_at = None;
    }

    fn open_overlay(&mut self, overlay: Overlay, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay == Some(overlay) {
            self.close_overlay(window, cx);
            return;
        }
        self.set_overlay(Some(overlay), cx);
        // The effort menu opens on the current level.
        self.selected =
            if overlay == Overlay::Effort { EFFORTS.iter().position(|e| *e == self.effort()).unwrap_or(0) } else { first_choice(overlay) };
        self.filter.update(cx, |f, cx| f.replace_text("", cx));
        match overlay {
            Overlay::Sessions => {
                self.refresh_sessions(cx);
                self.filter.update(cx, |f, cx| f.set_placeholder("Filter sessions", cx));
                let focus = self.filter.read(cx).focus.clone();
                window.focus(&focus, cx);
            }
            Overlay::Projects => {
                self.run(
                    cx,
                    |c| async move { c.project_list().await },
                    |this, result, _| match result {
                        Ok(projects) => this.projects = projects,
                        Err(error) => this.notice = Some(error),
                    },
                );
                self.filter.update(cx, |f, cx| f.set_placeholder("Filter projects", cx));
                let focus = self.filter.read(cx).focus.clone();
                window.focus(&focus, cx);
            }
            Overlay::Models => {
                self.reload_catalog(cx);
                self.filter.update(cx, |f, cx| f.set_placeholder("Filter models", cx));
                let focus = self.filter.read(cx).focus.clone();
                window.focus(&focus, cx);
            }
            Overlay::Agents | Overlay::Effort | Overlay::Menu | Overlay::Slash => self.focus_composer(window, cx),
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
        let Some(overlay) = self.overlay.filter(|o| matches!(o, Overlay::Sessions | Overlay::Projects | Overlay::Models)) else {
            return;
        };
        let count = self.overlay_count(cx);
        match event {
            FieldEvent::Changed(_) => self.selected = first_choice(overlay),
            FieldEvent::Up => self.selected = self.selected.checked_sub(1).unwrap_or(count.saturating_sub(1)),
            FieldEvent::Down => self.selected = if count == 0 { 0 } else { (self.selected + 1) % count },
            FieldEvent::Cancel => return self.close_overlay(window, cx),
            FieldEvent::Submit => return self.activate_choice(self.selected, window, cx),
        }
        cx.notify();
    }

    fn activate_choice(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.overlay {
            // The project row, the sessions, then New session.
            Some(Overlay::Sessions) => match index.checked_sub(1) {
                None => self.open_overlay(Overlay::Projects, window, cx),
                Some(i) => match self.session_choices(cx).get(i) {
                    Some(s) => {
                        let id = s.id.clone();
                        self.pick_session(id, window, cx);
                    }
                    None => self.new_session(window, cx),
                },
            },
            // Recent projects, then Open folder….
            Some(Overlay::Projects) => match self.project_choices(cx).get(index) {
                Some(p) => {
                    let root = p.root.clone();
                    self.switch_project(root, window, cx);
                }
                None => self.open_folder(window, cx),
            },
            Some(Overlay::Models) => {
                if let Some(m) = self.model_choices(cx).get(index) {
                    let model = (m.provider.as_str().to_owned(), m.id.clone());
                    self.set_model(model, cx);
                }
                self.close_overlay(window, cx);
            }
            Some(Overlay::Agents) => {
                if let Some((name, _)) = self.agent_choices().into_iter().nth(index) {
                    self.set_agent(name, cx);
                }
                self.close_overlay(window, cx);
            }
            Some(Overlay::Effort) => {
                if let Some(effort) = EFFORTS.get(index) {
                    self.set_effort(*effort, cx);
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

    /// Recent projects matching the filter (most recent first), the current one included.
    fn project_choices(&self, cx: &gpui::App) -> Vec<&ProjectInfo> {
        let query = self.filter.read(cx).text.clone();
        let names = project_names(self.projects.iter().map(|p| p.root.as_str()));
        let mut list: Vec<(i32, usize, &ProjectInfo)> = self
            .projects
            .iter()
            .zip(&names)
            .enumerate()
            .filter_map(|(i, (p, (name, parent)))| {
                crate::commands::fuzzy(&query, &format!("{name} {}", parent.as_deref().unwrap_or(""))).map(|(score, _)| (score, i, p))
            })
            .collect();
        if !query.trim().is_empty() {
            list.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
        }
        list.into_iter().map(|(_, _, p)| p).collect()
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

    /// Pick files for the next prompt.
    fn attach_files(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Attach".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let _ = this.update(cx, |this, cx| this.attach_paths(paths, cx));
        })
        .detach();
    }

    /// Dropped, pasted or picked files for the next prompt (folders are skipped).
    fn attach_paths(&mut self, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let read =
            cx.background_executor().spawn(async move { paths.iter().filter(|p| !p.is_dir()).map(|p| Attachment::read(p)).collect::<Vec<_>>() });
        cx.spawn(async move |this, cx| {
            let read = read.await;
            let _ = this.update(cx, |this, cx| this.add_attachments(read, cx));
        })
        .detach();
    }

    /// Read attachments join the composer; a failed one says why in the notice.
    fn add_attachments(&mut self, read: Vec<Result<Attachment, String>>, cx: &mut Context<Self>) {
        let mut failed = Vec::new();
        for r in read {
            match r {
                Ok(a) => self.attachments.push(a),
                Err(e) => failed.push(e),
            }
        }
        if !failed.is_empty() {
            self.notice = Some(failed.join("; "));
        }
        cx.notify();
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

/// Key of the optimistic user bubble.
const OPTIMISTIC: &str = "optimistic-user";
/// How long a send shows activity without otto confirming the turn.
const AWAIT: Duration = Duration::from_secs(15);
/// An input sent but not yet in the transcript.
struct Pending {
    text: String,
    /// User items with this text when it was sent.
    users: usize,
    at: Instant,
    /// otto's input id, once the send is accepted.
    id: Option<String>,
}

/// A row of the queue bar; `id` is `None` for a steer otto hasn't confirmed yet.
pub(super) struct QueueRow {
    pub id: Option<String>,
    pub text: String,
    pub steer: bool,
}

fn user_count(rows: &[Row], text: &str) -> usize {
    rows.iter().filter(|r| matches!(r, Row::User { text: t, .. } if t == text)).count()
}

/// The text parts of an input, joined.
pub(super) fn input_text(input: &[InputPart]) -> String {
    input.iter().filter_map(|p| if let InputPart::Text { text } = p { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join(" ")
}

fn session_label(s: &SessionInfo) -> String {
    if s.title.trim().is_empty() { "Untitled".to_owned() } else { s.title.clone() }
}

/// The row an overlay opens on: the switcher skips its project row.
fn first_choice(overlay: Overlay) -> usize {
    usize::from(overlay == Overlay::Sessions)
}

/// A project's folder name (the UI never shows paths).
fn project_name(root: &str) -> String {
    let root = root.trim_end_matches('/');
    root.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("/").to_owned()
}

/// Folder names of `roots`, each with its parent folder's name when another root has the same name.
fn project_names<'a>(roots: impl Iterator<Item = &'a str> + Clone) -> Vec<(String, Option<String>)> {
    let names: Vec<String> = roots.clone().map(project_name).collect();
    roots
        .zip(&names)
        .map(|(root, name)| {
            let shared = names.iter().filter(|n| *n == name).count() > 1;
            let parent = root.trim_end_matches('/').rsplit_once('/').map(|(parent, _)| project_name(parent));
            (name.clone(), parent.filter(|_| shared))
        })
        .collect()
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
        use gpui::{ParentElement as _, Styled as _, prelude::*};
        // Files dropped anywhere on the pane attach to the next prompt.
        let drop = gpui::div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .drag_over::<gpui::ExternalPaths>(|s, _, _, _| s.bg(gpui::rgb(crate::theme::accent()).opacity(0.06)))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                let focus = this.editor.read(cx).focus.clone();
                focus.focus(window, cx);
                this.attach_paths(paths.paths().to_vec(), cx);
            }));
        gpui::div().relative().size_full().child(self.render_pane(window, cx)).child(drop)
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

    #[test]
    fn projects_show_folder_names_and_parents_only_for_duplicates() {
        assert_eq!(project_name("/Users/me/dev/term"), "term");
        assert_eq!(project_name("/Users/me/dev/term/"), "term");
        assert_eq!(project_name("/"), "/");
        let roots = ["/Users/me/dev/term", "/Users/me/work/app", "/Users/me/old/app", "/srv"];
        assert_eq!(
            project_names(roots.iter().copied()),
            [
                ("term".to_owned(), None),
                ("app".to_owned(), Some("work".to_owned())),
                ("app".to_owned(), Some("old".to_owned())),
                ("srv".to_owned(), None)
            ]
        );
        // The switcher opens on its first session (the project row is above it); other lists on their first row.
        assert_eq!((first_choice(Overlay::Sessions), first_choice(Overlay::Projects), first_choice(Overlay::Models)), (1, 0, 0));
    }
}
