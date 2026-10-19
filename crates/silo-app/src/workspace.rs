//! The app window: one session of one host, its windows as tabs, and the
//! active window's split layout of terminal panes. The titlebar session
//! pill switches sessions and hosts (see `switcher.rs`).

use std::{collections::HashMap, path::PathBuf, time::Instant};

use gpui::{
    AnyElement, Bounds, Context, Entity, FocusHandle, IntoElement, Render, Subscription, Task, Window, actions, div, point, prelude::*, px, rgb, size,
};
use silo_client::Control;
use silo_proto::{
    control::{Created, Event, Method, SpawnSpec},
    model::{Axis, BlockKind, SessionSummary, SessionView, WindowInfo},
};

use crate::{
    add_host::AddHostSheet,
    cache::{self, CacheWriter, CachedTree},
    conn::ConnEvent,
    hosts::{self, AppState, HostInfo, HostState},
    motion::{self, Edge, Motion, PaneGhost, TabGhost},
    pane::{Pane, PaneEvent},
    switcher::Switcher,
    theme::{self, Continuous as _},
};

actions!(
    silo,
    [SelectWindow1, SelectWindow2, SelectWindow3, SelectWindow4, SelectWindow5, SelectWindow6, SelectWindow7, SelectWindow8, SelectWindow9]
);

pub const TITLEBAR_HEIGHT: f32 = 46.0;

/// Status bar while the shown content is the last known (cached) state.
const OFFLINE: &str = "Offline";

pub struct HostEntry {
    pub info: HostInfo,
    pub state: HostState,
    pub control: Option<Control>,
    /// Sessions, oldest first (the order of their ⌃⌘ shortcuts).
    pub sessions: Vec<SessionSummary>,
    /// Stop Server: the connection loop waits instead of respawning.
    pub paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// A server of a different build holds this host.
    pub incompatible: Option<silo_client::DifferentBuild>,
    /// Removed via Remove Host…; kept so host indices stay stable.
    pub removed: bool,
    pub(crate) conn: Option<Task<()>>,
    /// This host's client cache (`None` if it could not be opened).
    cache: Option<CacheWriter>,
    /// Last known tree, shown while switching to it offline.
    tree: Option<CachedTree>,
    /// Mirrors waiting for their pane: from the cache, or kept in memory
    /// when switching away.
    restored: HashMap<String, silo_client::Mirror>,
}

pub struct Workspace {
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) home: PathBuf,
    pub(crate) hosts: Vec<HostEntry>,
    pub(crate) active_host: usize,
    pub(crate) status: Option<String>,
    pub(crate) view: Option<SessionView>,
    /// Revision an optimistic local layout change was applied on top of;
    /// snapshots up to it are stale and ignored. See `call_optimistic`.
    pub(crate) ahead_of: Option<u64>,
    /// Session to show once the host connects (restored or requested).
    pub(crate) wanted_session: Option<String>,
    pub(crate) panes: HashMap<String, (Entity<Pane>, Subscription)>,
    /// Browser blocks' panes (terminal blocks are in `panes`).
    pub(crate) browsers: HashMap<String, (Entity<crate::browser_pane::BrowserPane>, Subscription)>,
    /// The server's focused block of the active window, as last applied.
    server_focus: Option<String>,
    pub(crate) focus: FocusHandle,
    pub(crate) switcher: Option<Switcher>,
    pub(crate) add_host: Option<AddHostSheet>,
    pub(crate) palette: Option<crate::palette::Palette>,
    pub(crate) motion: Motion,
    pub(crate) appearance_menu: crate::appearance::AppearanceMenu,
    pub(crate) settings: Option<crate::settings::SettingsModal>,
    pub(crate) pairing: crate::settings::Pairing,
    /// Last fullscreen state the titlebar was laid out for.
    pub(crate) last_fullscreen: std::cell::Cell<Option<bool>>,
    /// The titlebar's horizontally scrolling tab strip.
    pub(crate) tab_scroll: gpui::ScrollHandle,
    /// Active tab the strip was last rendered for, and a pending
    /// scroll-into-view of the active tab (window id, frames left).
    pub(crate) tab_last_active: std::cell::RefCell<Option<String>>,
    pub(crate) tab_reveal: std::cell::RefCell<Option<(String, u16)>>,
    /// Tab under a dragged tab and whether the drop lands after it.
    pub(crate) tab_drop: Option<(usize, bool)>,
    /// The tab being dragged (window id), while its drag lasts.
    pub(crate) tab_dragging: Option<String>,
    pub(crate) float_drag: Option<crate::floats::FloatDrag>,
    /// A press on empty titlebar space that moves the window once dragged.
    pub(crate) titlebar_move: bool,
    state: AppState,
    _checkpoints: Task<()>,
    /// When each block was last checkpointed.
    checkpointed: HashMap<String, Instant>,
    _quit: Subscription,
    _bounds: Subscription,
}

impl Workspace {
    pub fn new(rt: tokio::runtime::Handle, home: PathBuf, exe: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = hosts::load_state(&home);
        theme::apply_ui_font(&state.ui_font_family, cx);
        let wanted_session = std::env::var("SILO_SESSION").ok().filter(|s| !s.is_empty()).or_else(|| state.last_session.clone());
        let checkpoints = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(cache::CHECKPOINT_TICK).await;
                if this.update(cx, |this, cx| this.checkpoint_next(cx)).is_err() {
                    return;
                }
            }
        });
        let quit = cx.on_app_quit(|this, cx| {
            this.checkpoint_all(cx);
            let writers: Vec<CacheWriter> = this.hosts.iter().filter_map(|h| h.cache.clone()).collect();
            let rt = this.rt.clone();
            async move {
                let flush = move || writers.iter().for_each(|w| w.flush(std::time::Duration::from_secs(2)));
                let _ = rt.spawn_blocking(flush).await;
            }
        });
        let mut this = Self {
            rt,
            home: home.clone(),
            hosts: Vec::new(),
            active_host: 0,
            status: Some("connecting to server…".into()),
            view: None,
            ahead_of: None,
            wanted_session,
            panes: HashMap::new(),
            browsers: HashMap::new(),
            server_focus: None,
            focus: cx.focus_handle(),
            switcher: None,
            add_host: None,
            palette: None,
            motion: Motion::default(),
            appearance_menu: crate::appearance::AppearanceMenu::new(window, cx),
            settings: None,
            pairing: crate::settings::Pairing::default(),
            last_fullscreen: std::cell::Cell::new(None),
            tab_scroll: gpui::ScrollHandle::new(),
            tab_last_active: std::cell::RefCell::new(None),
            tab_reveal: std::cell::RefCell::new(None),
            tab_drop: None,
            tab_dragging: None,
            float_drag: None,
            titlebar_move: false,
            state,
            _checkpoints: checkpoints,
            checkpointed: HashMap::new(),
            _quit: quit,
            // Fullscreen changes the titlebar inset; re-render at once.
            _bounds: cx.observe_window_bounds(window, |_, _, cx| cx.notify()),
        };
        this.add_host_entry(HostInfo::local(home.clone(), exe), window, cx);
        for remote in hosts::load(&home) {
            this.add_host_entry(HostInfo::remote(home.clone(), &remote), window, cx);
        }
        // Paint the cached tree until the server answers.
        this.show_cached(window, cx);
        this
    }

    /// Cache of the shown (active) host.
    fn active_cache(&self) -> Option<&CacheWriter> {
        self.hosts.get(self.active_host)?.cache.as_ref()
    }

    /// Checkpoint one changed pane of the shown host, the one saved
    /// longest ago, at most every `CHECKPOINT_INTERVAL` per block. Capture
    /// encodes the snapshot here on the UI thread; the write is queued to
    /// the host's cache worker.
    fn checkpoint_next(&mut self, cx: &mut Context<Self>) {
        if self.active_cache().is_none() {
            return;
        }
        let panes = &self.panes;
        self.checkpointed.retain(|id, _| panes.contains_key(id));
        let now = Instant::now();
        let due = self
            .panes
            .iter()
            .filter(|(id, (pane, _))| {
                self.checkpointed.get(*id).is_none_or(|t| now - *t >= cache::CHECKPOINT_INTERVAL) && pane.read(cx).needs_checkpoint()
            })
            .min_by_key(|(id, _)| self.checkpointed.get(*id).copied())
            .map(|(id, _)| id.clone());
        if let Some(id) = due {
            self.checkpoint_block(&id, cx);
        }
    }

    /// Checkpoint every changed pane (app quit, leaving the host).
    fn checkpoint_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        for id in ids {
            self.checkpoint_block(&id, cx);
        }
    }

    fn checkpoint_block(&mut self, id: &str, cx: &mut Context<Self>) {
        let (Some(writer), Some((pane, _))) = (self.active_cache(), self.panes.get(id)) else {
            return;
        };
        if let Some(checkpoint) = pane.update(cx, |pane, _| pane.checkpoint()) {
            writer.save_block(id.to_owned(), checkpoint);
            self.checkpointed.insert(id.to_owned(), Instant::now());
        }
    }

    /// Remember the shown host's live tree (in memory and its cache).
    fn save_tree(&mut self) {
        let Some(view) = &self.view else { return };
        let entry = &mut self.hosts[self.active_host];
        if entry.control.is_none() {
            return;
        }
        let tree = CachedTree { sessions: entry.sessions.clone(), view: view.clone() };
        if let Some(writer) = &entry.cache {
            writer.save_tree(tree.clone());
        }
        entry.tree = Some(tree);
    }

    /// Drop checkpoints of blocks `host`'s server no longer has, checked
    /// against every block of every session.
    fn gc_cache(&self, host: usize) {
        let entry = &self.hosts[host];
        let (Some(writer), Some(control)) = (entry.cache.clone(), entry.control.clone()) else {
            return;
        };
        self.rt.spawn(async move {
            let all = async {
                let sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await?;
                let mut live = Vec::new();
                for s in sessions {
                    let view: SessionView = control.call(Method::SessionView { session_id: s.id }).await?;
                    live.extend(cache::live_blocks(&view));
                }
                anyhow::Ok(live)
            };
            match all.await {
                Ok(live) => writer.gc(live),
                Err(error) => tracing::debug!("cache gc skipped: {error:#}"),
            }
        });
    }

    pub(crate) fn add_host_entry(&mut self, info: HostInfo, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.hosts.len();
        let opened = cache::open_host(&cache::root(), &info.transport.cache_identity());
        let (cache, tree, restored) = opened.map_or((None, None, HashMap::new()), |c| (Some(c.writer), c.tree, c.restored));
        let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut events = crate::conn::start(&self.rt, info.transport.clone(), paused.clone());
        let conn = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this.update_in(cx, |this, window, cx| this.conn_event(index, event, window, cx)).is_err() {
                    return;
                }
            }
        });
        self.hosts.push(HostEntry {
            info,
            state: HostState::Connecting,
            control: None,
            sessions: tree.as_ref().map(|t| t.sessions.clone()).unwrap_or_default(),
            paused,
            incompatible: None,
            removed: false,
            conn: Some(conn),
            cache,
            tree,
            restored,
        });
    }

    /// Paint the active host's last known tree until its server answers.
    fn show_cached(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = self.hosts[self.active_host].tree.clone() else {
            return;
        };
        self.status = Some(OFFLINE.into());
        self.set_view(tree.view, window, cx);
    }

    pub(crate) fn control(&self) -> Option<Control> {
        self.hosts.get(self.active_host).and_then(|h| h.control.clone())
    }

    fn conn_event(&mut self, host: usize, event: ConnEvent, window: &mut Window, cx: &mut Context<Self>) {
        let active = host == self.active_host;
        match event {
            ConnEvent::Connected(control) => {
                let entry = &mut self.hosts[host];
                entry.state = HostState::Connected;
                entry.control = Some(control.clone());
                entry.incompatible = None;
                self.refresh_sessions(host, cx);
                self.refresh_settings(None, cx);
                self.gc_cache(host);
                if let Some(reason) = control.hello().server.remote_error.as_deref().filter(|_| host == 0) {
                    tracing::warn!("server remote access unavailable: {reason}");
                }
                if host == 0 && self.settings.is_some() {
                    self.refresh_devices(cx);
                }
                if active {
                    // The view after a reconnect only records ids.
                    self.motion.reset();
                    self.status = None;
                    for (pane, _) in self.panes.values() {
                        pane.update(cx, |pane, cx| pane.set_control(control.clone(), cx));
                    }
                    for (pane, _) in self.browsers.values() {
                        pane.update(cx, |pane, cx| pane.set_control(control.clone(), cx));
                    }
                    let preferred = self.view.as_ref().map(|v| v.session.id.clone()).or_else(|| self.wanted_session.take());
                    self.choose_session(preferred, window, cx);
                }
            }
            ConnEvent::Disconnected(reason) => {
                let entry = &mut self.hosts[host];
                entry.control = None;
                entry.incompatible = None;
                // Retries repeat the same failure; only a change refreshes
                // the settings panel (which would reset its confirmations).
                let changed = !matches!(&entry.state, HostState::Unreachable(old) if *old == reason);
                entry.state = HostState::Unreachable(reason.clone());
                if changed {
                    self.refresh_settings(None, cx);
                }
                if active {
                    self.motion.reset();
                    self.status = Some(if self.hosts[host].paused.load(std::sync::atomic::Ordering::Acquire) {
                        "Server stopped — run “Start Server” from the command palette (⌘P)".to_owned()
                    } else if self.view.is_some() {
                        OFFLINE.to_owned()
                    } else {
                        format!("server unavailable: {reason}")
                    });
                }
            }
            ConnEvent::Quit => cx.quit(),
            ConnEvent::Addr(addr) => {
                if let hosts::Transport::Iroh { addr: current, .. } = &mut self.hosts[host].info.transport {
                    current.clone_from(&addr);
                }
                let mut saved = hosts::load(&self.home);
                if hosts::merge_addr(&mut saved, &addr)
                    && let Err(error) = hosts::save(&self.home, &saved)
                {
                    tracing::warn!("save host address: {error:#}");
                }
            }
            ConnEvent::NotPaired | ConnEvent::RemoteBuild => {
                let entry = &mut self.hosts[host];
                entry.control = None;
                entry.state = if matches!(event, ConnEvent::NotPaired) { HostState::NotPaired } else { HostState::DifferentBuild };
                self.refresh_settings(None, cx);
                if active {
                    self.motion.reset();
                    self.status = None;
                }
            }
            ConnEvent::DifferentBuild(server) => {
                let entry = &mut self.hosts[host];
                entry.control = None;
                entry.sessions.clear();
                entry.state = HostState::Unreachable(server.headline());
                entry.incompatible = Some(server);
                self.refresh_settings(None, cx);
                if active {
                    // Keep the last content painted, read-only, under the card.
                    self.motion.reset();
                    self.status = None;
                }
            }
            ConnEvent::Event(Event::SessionsChanged { mut sessions }) => {
                sessions.sort_by_key(|s| s.created_at);
                self.hosts[host].sessions = sessions;
                self.save_tree();
            }
            ConnEvent::Event(_) if !active => {}
            ConnEvent::Event(Event::SessionChanged { view }) => {
                let stale = self.ahead_of.is_some_and(|base| view.session.revision <= base);
                if !stale && self.view.as_ref().is_some_and(|v| v.session.id == view.session.id) {
                    self.set_view(view, window, cx);
                }
            }
            ConnEvent::Event(Event::SessionClosed { session_id }) => {
                self.gc_cache(host);
                if self.view.as_ref().is_some_and(|v| v.session.id == session_id) {
                    self.view = None;
                    self.choose_session(None, window, cx);
                }
            }
            ConnEvent::Event(Event::BlockChanged { block }) => {
                if let Some((pane, _)) = self.browsers.get(&block.id) {
                    pane.update(cx, |pane, cx| pane.set_info(block.clone(), cx));
                } else if let Some((pane, _)) = self.panes.get(&block.id) {
                    pane.update(cx, |pane, cx| pane.set_info(block.clone(), cx));
                }
                if let Some(view) = self.view.as_mut()
                    && let Some(info) = view.blocks.iter_mut().find(|b| b.id == block.id)
                {
                    *info = block;
                }
            }
            ConnEvent::Event(Event::OpenSession { session_id }) => {
                window.activate_window();
                cx.activate(true);
                self.open_session(host, session_id, window, cx);
            }
            ConnEvent::Event(Event::BrowserCommand { block_id, command }) => {
                if let Some((pane, _)) = self.browsers.get(&block_id) {
                    pane.update(cx, |pane, cx| pane.apply_command(&command, cx));
                }
            }
            ConnEvent::Event(Event::BlockBell { .. }) => {}
        }
        cx.notify();
    }

    fn refresh_sessions(&mut self, host: usize, cx: &mut Context<Self>) {
        let Some(control) = self.hosts[host].control.clone() else {
            return;
        };
        let task = self.rt.spawn(async move { control.call::<Vec<SessionSummary>>(Method::SessionList {}).await });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(mut sessions)) = task.await {
                sessions.sort_by_key(|s| s.created_at);
                let _ = this.update(cx, |this, cx| {
                    this.hosts[host].sessions = sessions;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Show `session_id` on `host`, detaching the current session's blocks.
    pub(crate) fn open_session(&mut self, host: usize, session_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if host == self.active_host && self.view.as_ref().is_some_and(|v| v.session.id == session_id) {
            return;
        }
        if host != self.active_host {
            self.leave_host(cx);
        }
        self.detach_view();
        self.active_host = host;
        if self.control().is_none() {
            let cached = self.hosts[host].tree.as_ref().is_some_and(|t| t.view.session.id == session_id);
            self.wanted_session = Some(session_id);
            if cached {
                self.show_cached(window, cx);
            } else {
                self.status = Some(format!("{} is not connected", self.hosts[host].info.name));
            }
            cx.notify();
            return;
        }
        self.choose_session(Some(session_id), window, cx);
    }

    /// Show `host` (its cached tree until it connects, then its last or
    /// newest session).
    pub(crate) fn switch_host(&mut self, host: usize, window: &mut Window, cx: &mut Context<Self>) {
        if host == self.active_host {
            return;
        }
        self.leave_host(cx);
        self.detach_view();
        self.active_host = host;
        self.wanted_session = self.hosts[host].tree.as_ref().map(|t| t.view.session.id.clone());
        self.status = Some(format!("Connecting to {}…", self.hosts[host].info.name));
        self.show_cached(window, cx);
        if self.control().is_some() {
            let wanted = self.wanted_session.take();
            self.choose_session(wanted, window, cx);
        }
        cx.notify();
    }

    /// Before switching hosts: checkpoint the shown panes and keep their
    /// mirrors in memory, so coming back paints at once.
    fn leave_host(&mut self, cx: &mut Context<Self>) {
        self.checkpoint_all(cx);
        self.checkpointed.clear();
        let mirrors: Vec<(String, silo_client::Mirror)> =
            self.panes.iter().map(|(id, (pane, _))| (id.clone(), pane.update(cx, |pane, _| pane.take_mirror()))).collect();
        self.hosts[self.active_host].restored.extend(mirrors);
    }

    /// Drop every pane (closing its data stream) and give up ownership.
    pub(crate) fn detach_view(&mut self) {
        let blocks: Vec<String> = self.panes.keys().chain(self.browsers.keys()).cloned().collect();
        self.panes.clear();
        self.browsers.clear();
        self.view = None;
        self.server_focus = None;
        self.motion.reset();
        if !blocks.is_empty() {
            self.call(Method::DisownBlock { block_ids: blocks });
        }
    }

    /// Create a session (named, or numbered by the server) and show it.
    pub(crate) fn create_session(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.active_host;
        let Some(control) = self.control() else {
            return;
        };
        let cwd = self.focused_block(window, cx).and_then(|b| self.cwd_of(&b, cx)).or_else(|| std::env::var("HOME").ok());
        let task = self.rt.spawn(async move {
            control
                .call::<Created>(Method::SessionCreate {
                    name,
                    spawn: SpawnSpec { cwd, command: Vec::new(), size: None },
                    kind: BlockKind::Terminal,
                    url: None,
                })
                .await
        });
        cx.spawn_in(window, async move |this, cx| match task.await {
            Ok(Ok(created)) => {
                let _ = this.update_in(cx, |this, window, cx| this.open_session(host, created.session_id, window, cx));
            }
            Ok(Err(error)) => {
                let _ = this.update(cx, |this, cx| {
                    tracing::warn!("create session: {error:#}");
                    this.status = Some("Could not create the session.".into());
                    cx.notify();
                });
            }
            Err(_) => {}
        })
        .detach();
    }

    pub(crate) fn rename_session(&mut self, session_id: String, name: String) {
        self.call(Method::SessionRename { session_id, name });
    }

    /// Attach to `preferred`, else the newest session, else a new one.
    pub(crate) fn choose_session(&mut self, preferred: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(control) = self.control() else {
            return;
        };
        let task = self.rt.spawn(async move {
            let sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await?;
            let pick = preferred
                .and_then(|p| sessions.iter().find(|s| s.id == p || s.name == p))
                .or_else(|| sessions.iter().max_by_key(|s| s.created_at))
                .map(|s| s.id.clone());
            let id = match pick {
                Some(id) => id,
                None => {
                    let home = std::env::var("HOME").ok();
                    let spawn = SpawnSpec { cwd: home, command: Vec::new(), size: None };
                    let created: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await?;
                    created.session_id
                }
            };
            control.call::<SessionView>(Method::SessionView { session_id: id }).await
        });
        cx.spawn_in(window, async move |this, cx| {
            let view = match task.await {
                Ok(Ok(view)) => view,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        tracing::warn!("open session: {error:#}");
                        this.status = Some("Could not open the session.".into());
                        cx.notify();
                    });
                    return;
                }
                Err(_) => return,
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if this.view.as_ref().is_some_and(|v| v.session.id != view.session.id) {
                    this.detach_view();
                }
                this.server_focus = None;
                this.status = None;
                this.set_view(view, window, cx);
            });
        })
        .detach();
    }

    pub(crate) fn active_window(&self) -> Option<&WindowInfo> {
        let view = self.view.as_ref()?;
        view.session.windows.iter().find(|w| w.id == view.session.active_window).or_else(|| view.session.windows.first())
    }

    fn set_view(&mut self, view: SessionView, window: &mut Window, cx: &mut Context<Self>) {
        self.ahead_of = None;
        window.set_window_title(&format!("{} — silo", view.session.name));
        if self.active_host == 0 && self.state.last_session.as_deref() != Some(&view.session.id) {
            self.state.last_session = Some(view.session.id.clone());
            hosts::save_state(&self.home, &self.state);
        }
        self.track_motion(&view, window, cx);
        let live = self.control().is_some();
        if live && let (Some(old), Some(writer)) = (&self.view, self.active_cache()) {
            writer.forget(cache::closed_blocks(old, &view));
        }
        self.view = Some(view);
        let view = self.view.clone().expect("view");
        // Panes for every block of the active window; keep panes of other
        // windows that are still alive, drop the rest.
        let all: Vec<String> = view.session.windows.iter().flat_map(crate::floats::window_blocks).collect();
        self.panes.retain(|id, _| all.contains(id));
        self.browsers.retain(|id, _| all.contains(id));
        if live {
            self.hosts[self.active_host].restored.retain(|id, _| all.contains(id));
        }
        let active = self.active_window().cloned();
        let browsers: Vec<String> = view
            .blocks
            .iter()
            .filter(|b| crate::browser_pane::kind_of(Some(b)) == crate::browser_pane::Kind::Browser)
            .map(|b| b.id.clone())
            .collect();
        if let Some(active) = &active {
            for block in crate::floats::window_blocks(active) {
                if browsers.contains(&block) {
                    self.ensure_browser(&block, window, cx);
                } else if !self.panes.contains_key(&block) {
                    let control = self.control();
                    let rt = self.rt.clone();
                    let id = block.clone();
                    let restored = self.hosts[self.active_host].restored.remove(&block);
                    let pane = cx.new(|cx| Pane::new(id, rt, control, restored, window, cx));
                    let subscription = cx.subscribe_in(&pane, window, |this, pane, event: &PaneEvent, _, cx| match event {
                        PaneEvent::Activated => {
                            let block = pane.read(cx).block_id.clone();
                            this.pane_activated(block);
                            cx.notify();
                        }
                        PaneEvent::NewShell => {
                            let block = pane.read(cx).block_id.clone();
                            this.new_shell(block, cx);
                        }
                    });
                    self.panes.insert(block, (pane, subscription));
                }
            }
        }
        let infos = view.blocks.clone();
        for info in infos {
            if let Some((pane, _)) = self.browsers.get(&info.id) {
                pane.update(cx, |pane, cx| pane.set_info(info, cx));
            } else if let Some((pane, _)) = self.panes.get(&info.id) {
                pane.update(cx, |pane, cx| pane.set_info(info, cx));
            }
        }
        // Follow the server's focus when it changed (split, new window,
        // another client); otherwise keep the local focus.
        if let Some(active) = active
            && self.switcher.is_none()
            && self.add_host.is_none()
        {
            let focused = active.focused.clone();
            let local = self.panes.values().any(|(p, _)| p.read(cx).focus.is_focused(window))
                || self
                    .browsers
                    .values()
                    .any(|(p, _)| p.read(cx).focus.contains_focused(window, cx) || p.read(cx).address.read(cx).focus.is_focused(window));
            if self.server_focus.as_deref() != Some(&focused) || !local {
                self.server_focus = Some(focused.clone());
                self.focus_block(&focused, window, cx);
            }
        }
        self.save_tree();
        cx.notify();
    }

    /// Start entry/exit motion for windows and blocks that appeared or
    /// disappeared since the last view of this session. Runs before the
    /// new view replaces the old one, so closed panes can leave a ghost.
    fn track_motion(&mut self, view: &SessionView, window: &Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let scope = view.session.id.clone();
        let windows = self.motion.windows.sync(&scope, view.session.windows.iter().map(|w| w.id.clone()).collect());
        let blocks = self.motion.blocks.sync(&scope, view.session.windows.iter().flat_map(|w| w.layout.blocks()).collect());
        if !(windows.added.is_empty() && windows.removed.is_empty() && blocks.added.is_empty() && blocks.removed.is_empty()) {
            tracing::debug!(?windows, ?blocks, reduce_motion = cx.reduce_motion(), "motion");
        }
        if cx.reduce_motion() {
            return;
        }
        let mut tab_ghosts = Vec::new();
        if let Some(old) = &self.view {
            for id in &windows.removed {
                if let Some(index) = old.session.windows.iter().position(|w| &w.id == id) {
                    let title = self.window_title(&old.session.windows[index], cx);
                    let icon = self.tab_icon(&old.session.windows[index]);
                    tab_ghosts.push(TabGhost { id: id.clone(), title, icon, index, started: now });
                }
            }
        }
        self.motion.tab_ghosts.extend(tab_ghosts);
        for id in &windows.added {
            self.motion.tab_enter.insert(id.clone(), now);
        }
        // Pane motion only inside a window that stays active and unzoomed;
        // tab motion covers whole windows coming and going.
        let old_active = self.active_window().filter(|w| !w.zoomed).cloned();
        let new_active = view.session.windows.iter().find(|w| w.id == view.session.active_window).or_else(|| view.session.windows.first());
        let (Some(old_active), Some(new_active)) = (old_active, new_active) else {
            return;
        };
        if old_active.id != new_active.id || new_active.zoomed {
            return;
        }
        let viewport = window.viewport_size();
        let body = Bounds::new(point(px(0.), px(TITLEBAR_HEIGHT)), size(viewport.width, viewport.height - px(TITLEBAR_HEIGHT)));
        let old_blocks = old_active.layout.blocks();
        for id in blocks.removed.iter().filter(|id| old_blocks.contains(id)) {
            if let Some((pane, _)) = self.panes.get(id)
                && let Some((frame, mut slot)) = pane.read(cx).ghost()
            {
                // Grow the terminal's slot to the whole card: header and outline.
                let title = self.pane_headers().then(|| crate::layout::header_labels(pane.read(cx).info.as_ref()).0);
                if title.is_some() {
                    slot.origin.y -= px(crate::layout::HEADER_HEIGHT);
                    slot.size.height += px(crate::layout::HEADER_HEIGHT);
                }
                if self.density() == hosts::Density::Comfortable {
                    slot = slot.dilate(px(1.0));
                }
                // Away from the sibling that takes over the space; geometry
                // decides when the block was not a direct split child.
                let edge = crate::layout::sibling_side(&old_active.layout, id).unwrap_or_else(|| motion::exit_edge(slot, body));
                tracing::debug!(block = %id, ?edge, "pane ghost");
                self.motion.pane_ghosts.push(PaneGhost { frame, slot, edge, title, started: now });
            }
        }
        for id in blocks.added {
            match crate::layout::parent_axis(&new_active.layout, id.as_str()) {
                Some(Axis::Horizontal) => self.motion.pane_enter.insert(id, (now, Edge::Right)),
                Some(Axis::Vertical) => self.motion.pane_enter.insert(id, (now, Edge::Bottom)),
                None => None,
            };
        }
    }

    pub(crate) fn focus_block(&self, block: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((pane, _)) = self.panes.get(block) {
            let handle = pane.read(cx).focus.clone();
            handle.focus(window, cx);
        } else if let Some((pane, _)) = self.browsers.get(block) {
            let handle = pane.read(cx).focus.clone();
            handle.focus(window, cx);
        }
    }

    /// Return keyboard focus to the terminal after closing an overlay.
    pub(crate) fn refocus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_window().map(|w| w.focused.clone()).filter(|b| self.panes.contains_key(b) || self.browsers.contains_key(b)) {
            Some(block) => self.focus_block(&block, window, cx),
            // No terminal (server stopped, no session): keep key bindings
            // working by focusing the workspace itself.
            None => self.focus.focus(window, cx),
        }
    }

    pub(crate) fn pane_activated(&mut self, block: String) {
        if self.server_focus.as_deref() == Some(&block) {
            return;
        }
        self.server_focus = Some(block.clone());
        self.call(Method::BlockFocus { block_id: block });
    }

    pub(crate) fn call(&self, method: Method) {
        let Some(control) = self.control() else {
            return;
        };
        self.rt.spawn(async move {
            if let Err(error) = control.call_value(method).await {
                tracing::warn!("request failed: {error:#}");
            }
        });
    }

    /// Sends a layout change already applied to the local view. Older
    /// snapshots are ignored until the server's confirms it; if the call
    /// fails, `revert` undoes it (unless a newer snapshot replaced it).
    pub(crate) fn call_optimistic(
        &mut self,
        method: Method,
        failure: &'static str,
        revert: impl FnOnce(&mut Self) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(control) = self.control() else {
            return revert(self);
        };
        let base = self.view.as_ref().map(|v| v.session.revision);
        self.ahead_of = base;
        let task = self.rt.spawn(async move { control.call_value(method).await });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(_)) = task.await {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                if this.ahead_of.is_some() && this.ahead_of == base {
                    this.ahead_of = None;
                    revert(this);
                }
                this.flash(failure.into(), cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn focused_block(&self, window: &Window, cx: &gpui::App) -> Option<String> {
        self.panes
            .iter()
            .find(|(_, (p, _))| p.read(cx).focus.is_focused(window))
            .map(|(id, _)| id.clone())
            .or_else(|| self.browser_focused(window, cx))
            .or_else(|| self.active_window().map(|w| w.focused.clone()))
    }

    pub(crate) fn focused_pane(&self, window: &Window, cx: &gpui::App) -> Option<Entity<Pane>> {
        let block = self.focused_block(window, cx)?;
        self.panes.get(&block).map(|(p, _)| p.clone())
    }

    pub(crate) fn cwd_of(&self, block: &str, cx: &gpui::App) -> Option<String> {
        self.panes.get(block).and_then(|(p, _)| p.read(cx).info.as_ref().and_then(|i| i.cwd.clone()))
    }

    pub(crate) fn new_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.view else { return };
        let cwd = self.focused_block(window, cx).and_then(|b| self.cwd_of(&b, cx));
        self.call(Method::WindowCreate {
            session_id: view.session.id.clone(),
            spawn: SpawnSpec { cwd, command: Vec::new(), size: None },
            kind: BlockKind::Terminal,
            url: None,
        });
    }

    pub(crate) fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.focused_block(window, cx) else {
            return;
        };
        self.split_block(block, axis, cx);
    }

    /// Split `block` (not necessarily the focused one, e.g. from its header).
    pub(crate) fn split_block(&mut self, block: String, axis: Axis, cx: &gpui::App) {
        let cwd = self.cwd_of(&block, cx);
        tracing::debug!(%block, ?axis, "split block");
        self.call(Method::BlockSplit {
            block_id: block,
            axis,
            spawn: SpawnSpec { cwd, command: Vec::new(), size: None },
            kind: BlockKind::Terminal,
            url: None,
        });
    }

    pub(crate) fn close_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(block) = self.focused_block(window, cx) {
            self.close_block_id(block);
        }
    }

    pub(crate) fn close_block_id(&mut self, block: String) {
        tracing::debug!(%block, "close block");
        self.call(Method::BlockClose { block_id: block });
    }

    /// Replace a block without a process by a fresh shell in its slot:
    /// split it, then close it so the new block takes the whole slot. A
    /// float can't be split, so its window's first tiled block is, and the
    /// new block is floated where the old one was.
    pub(crate) fn new_shell(&mut self, block: String, cx: &mut Context<Self>) {
        let Some(control) = self.control() else {
            return;
        };
        let Some(view) = &self.view else { return };
        let Some(w) = view.session.windows.iter().find(|w| crate::floats::window_blocks(w).contains(&block)) else {
            return;
        };
        let float = w.floats.iter().find(|f| f.id == block).map(|f| f.rect);
        let Some(anchor) = (if float.is_some() { w.layout.blocks().into_iter().next() } else { Some(block.clone()) }) else {
            return;
        };
        let cwd = self.cwd_of(&block, cx);
        let task = self.rt.spawn(async move {
            let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
            let created: Created =
                control.call(Method::BlockSplit { block_id: anchor, axis: Axis::Horizontal, spawn, kind: BlockKind::Terminal, url: None }).await?;
            if let Some(rect) = float {
                control.call_value(Method::BlockFloat { block_id: created.block_id, rect: Some(rect) }).await?;
            }
            control.call_value(Method::BlockClose { block_id: block }).await?;
            anyhow::Ok(())
        });
        cx.spawn(async move |this, cx| {
            let error = match task.await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => format!("{error:#}"),
                Err(error) => error.to_string(),
            };
            tracing::warn!("new shell: {error}");
            let _ = this.update(cx, |this, cx| {
                this.flash("Couldn't start a new shell".into(), cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Zoom `block` (the server also focuses it), or unzoom the window.
    pub(crate) fn zoom_block(&mut self, block: String) {
        let zoomed = self.active_window().is_some_and(|w| w.zoomed);
        tracing::debug!(%block, zoomed = !zoomed, "zoom block");
        self.call(Method::BlockZoom { block_id: block, zoomed: !zoomed });
    }

    pub(crate) fn density(&self) -> hosts::Density {
        self.state.density
    }

    pub(crate) fn pane_headers(&self) -> bool {
        self.state.show_pane_headers
    }

    pub(crate) fn set_density(&mut self, density: hosts::Density, cx: &mut Context<Self>) {
        if self.state.density != density {
            self.state.density = density;
            hosts::save_state(&self.home, &self.state);
            tracing::debug!(?density, "appearance");
        }
        cx.notify();
    }

    pub(crate) fn set_pane_headers(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.state.show_pane_headers != show {
            self.state.show_pane_headers = show;
            hosts::save_state(&self.home, &self.state);
            tracing::debug!(show, "pane headers");
        }
        cx.notify();
    }

    /// The stored UI font preference (empty = Automatic).
    pub(crate) fn ui_font_family(&self) -> &str {
        &self.state.ui_font_family
    }

    /// Change the UI font (empty = Automatic): persist the exact family and
    /// re-render all chrome in it. The terminal font is untouched.
    pub(crate) fn set_ui_font_family(&mut self, family: String, window: &mut Window, cx: &mut Context<Self>) {
        let family = family.trim().to_owned();
        if self.state.ui_font_family != family {
            self.state.ui_font_family = family;
            hosts::save_state(&self.home, &self.state);
            tracing::debug!(family = %self.state.ui_font_family, "ui font");
        }
        theme::apply_ui_font(&self.state.ui_font_family, cx);
        window.refresh();
        cx.notify();
    }

    /// Checkmark state of a setting command in the palette.
    pub(crate) fn checked(&self, command: crate::commands::Command) -> bool {
        use crate::commands::Command;
        match command {
            Command::AppearanceComfortable => self.state.density == hosts::Density::Comfortable,
            Command::AppearanceCompact => self.state.density == hosts::Density::Compact,
            Command::ShowPaneHeaders => self.state.show_pane_headers,
            Command::HidePaneHeaders => !self.state.show_pane_headers,
            _ => false,
        }
    }

    pub(crate) fn cycle_focus(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_window() else {
            return;
        };
        let blocks = crate::floats::window_blocks(active);
        if blocks.is_empty() {
            return;
        }
        let current = self.focused_block(window, cx).and_then(|b| blocks.iter().position(|x| *x == b)).unwrap_or(0);
        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
        let next = (current as isize + step).rem_euclid(blocks.len() as isize) as usize;
        self.focus_block(&blocks[next], window, cx);
    }

    pub(crate) fn select_window(&mut self, index: usize) {
        let Some(view) = &self.view else { return };
        if let Some(w) = view.session.windows.get(index) {
            self.call(Method::WindowSelect { session_id: view.session.id.clone(), window_id: w.id.clone() });
        }
    }

    /// ⌘1–8: the n-th tab, ⌘9 the last one; the n-th session row while the
    /// switcher is open.
    fn number_key(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            return;
        }
        if self.switcher.is_some() {
            self.switch_to_row(index, window, cx);
        } else if let Some(view) = &self.view {
            self.select_window(tab_index(index, view.session.windows.len()));
        }
    }

    pub(crate) fn toggle_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(block) = self.focused_block(window, cx) {
            self.zoom_block(block);
        }
    }

    /// Text of the status bar along the window's bottom edge, if shown.
    pub(crate) fn status_bar(&self) -> Option<String> {
        self.status.clone().filter(|_| self.view.is_some() && self.incompatible().is_none())
    }

    pub(crate) fn window_title(&self, w: &WindowInfo, cx: &gpui::App) -> String {
        if let Some(name) = &w.name {
            return name.clone();
        }
        if let Some((pane, _)) = self.browsers.get(&w.focused) {
            return pane.read(cx).title();
        }
        self.panes.get(&w.focused).map_or_else(
            || self.view.as_ref().and_then(|v| v.blocks.iter().find(|b| b.id == w.focused)).map_or_else(|| "shell".into(), crate::pane::block_title),
            |(p, _)| p.read(cx).title(),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        if self.motion.prune(now) {
            window.request_animation_frame();
        }
        // A click on bare chrome (titlebar, gaps, menus) focuses the
        // workspace itself; hand focus back to the terminal so typing
        // keeps working. Overlays own their focus and are left alone.
        if self.focus.is_focused(window)
            && self.switcher.is_none()
            && self.palette.is_none()
            && self.settings.is_none()
            && self.add_host.is_none()
            && !self.appearance_menu.open
            && self.active_window().is_some_and(|w| self.panes.contains_key(&w.focused) || self.browsers.contains_key(&w.focused))
        {
            cx.defer_in(window, |this, window, cx| {
                // Re-check: an overlay may have taken focus meanwhile.
                if this.focus.is_focused(window)
                    && this.switcher.is_none()
                    && this.palette.is_none()
                    && this.add_host.is_none()
                    && this.settings.is_none()
                    && !this.appearance_menu.open
                {
                    this.refocus_terminal(window, cx);
                }
            });
        }
        let card = match self.incompatible().cloned() {
            Some(server) => Some(self.render_incompatible(&server, cx)),
            None => self.render_remote_problem(cx),
        };
        let body: AnyElement = match self.active_window().cloned() {
            Some(w) => match card {
                // The last content stays painted, read-only, under the card.
                Some(card) => div()
                    .relative()
                    .size_full()
                    .child(self.render_body(&w, now, window, cx))
                    .child(div().id("incompatible-scrim").occlude().absolute().inset_0().bg(gpui::rgba((theme::surface() << 8) | 0xd8)).child(card))
                    .into_any_element(),
                None => self.render_body(&w, now, window, cx),
            },
            None => match card {
                Some(card) => card,
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(24.0))
                    .text_color(rgb(theme::muted()))
                    .child(div().max_w(px(520.0)).text_center().child(self.status.clone().unwrap_or_default()))
                    .into_any_element(),
            },
        };
        let ghosts = self.render_pane_ghosts(now, window);
        let appearance = self.render_appearance_menu(cx);
        let settings = self.render_settings(window, cx);
        let titlebar = self.render_titlebar(window, cx);
        let switcher = self.render_switcher(window, cx);
        let sheet = self.render_add_host(cx);
        let palette = self.render_palette(window, cx);
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::surface()))
            .text_color(rgb(theme::foreground()))
            .font_family(theme::ui_font())
            .map(|el| crate::commands::handle_actions(el, cx))
            .on_action(cx.listener(|this, _: &SelectWindow1, window, cx| this.number_key(0, window, cx)))
            .on_action(cx.listener(|this, _: &SelectWindow2, window, cx| this.number_key(1, window, cx)))
            .on_action(cx.listener(|this, _: &SelectWindow3, window, cx| this.number_key(2, window, cx)))
            .on_action(cx.listener(|this, _: &SelectWindow4, window, cx| this.number_key(3, window, cx)))
            .on_action(cx.listener(|this, _: &SelectWindow5, window, cx| this.number_key(4, window, cx)))
            .on_action(cx.listener(|this, _: &SelectWindow6, window, cx| this.number_key(5, window, cx)))
            .on_action(cx.listener(|this, _: &SelectWindow7, window, cx| this.number_key(6, window, cx)))
            .on_action(cx.listener(|this, _: &SelectWindow8, window, cx| this.number_key(7, window, cx)))
            .on_action(cx.listener(|this, _: &SelectWindow9, window, cx| this.number_key(8, window, cx)))
            .child(titlebar)
            .child(div().flex_1().min_h_0().child(body))
            .when_some(self.status_bar(), |el, status| {
                el.child(
                    div().absolute().bottom(px(8.0)).left_0().right_0().flex().justify_center().child(
                        div()
                            .px(px(10.0))
                            .py(px(4.0))
                            .continuous(8.0)
                            .bg(rgb(theme::popover()))
                            .border_1()
                            .border_color(rgb(theme::border()))
                            .text_size(px(11.0))
                            .text_color(rgb(theme::muted()))
                            .child(status),
                    ),
                )
            })
            .children(ghosts)
            .children(appearance)
            .children(settings)
            .children(switcher)
            .children(sheet)
            .children(palette)
    }
}

/// ⌘1–9: tabs, or session rows while the switcher is open.
pub fn bind_number_keys(cx: &mut gpui::App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("cmd-1", SelectWindow1, None),
        KeyBinding::new("cmd-2", SelectWindow2, None),
        KeyBinding::new("cmd-3", SelectWindow3, None),
        KeyBinding::new("cmd-4", SelectWindow4, None),
        KeyBinding::new("cmd-5", SelectWindow5, None),
        KeyBinding::new("cmd-6", SelectWindow6, None),
        KeyBinding::new("cmd-7", SelectWindow7, None),
        KeyBinding::new("cmd-8", SelectWindow8, None),
        KeyBinding::new("cmd-9", SelectWindow9, None),
    ]);
}

/// The tab for number key `index` (0-based): ⌘9 is always the last tab.
fn tab_index(index: usize, tabs: usize) -> usize {
    if index == 8 { tabs.saturating_sub(1) } else { index }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cmd_nine_is_the_last_tab() {
        assert_eq!(super::tab_index(8, 3), 2);
        assert_eq!(super::tab_index(8, 12), 11);
        assert_eq!(super::tab_index(1, 3), 1);
    }
}
