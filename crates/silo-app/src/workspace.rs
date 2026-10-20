//! The app window: one session of one host, its windows as tabs, and the
//! active window's split layout of terminal panes. The titlebar session
//! pill switches sessions and hosts (see `switcher.rs`).

use std::{collections::HashMap, path::PathBuf, time::Instant};

use gpui::{
    AnyElement, App, Bounds, Context, Entity, FocusHandle, IntoElement, Render, Subscription, Task, Window, actions, div, point, prelude::*, px, rgb,
    size,
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
    hosts::{self, AppState, HostState},
    hosts_state::{HostEntry, HostEvent, HostsState},
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

pub struct Workspace {
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) home: PathBuf,
    /// The process's hosts (shared by every window).
    pub(crate) shared: Entity<HostsState>,
    /// This window's snapshot of `shared`'s hosts, synced on its changes.
    pub(crate) hosts: Vec<HostEntry>,
    pub(crate) active_host: usize,
    pub(crate) status: Option<String>,
    /// The `flash` message still showing; connecting keeps it.
    pub(crate) flashing: Option<String>,
    /// Upgrade Server is running (see `upgrade_server`).
    pub(crate) upgrading: bool,
    /// Shown in the status bar once the upgraded server connects.
    pub(crate) notice: Option<String>,
    pub(crate) view: Option<SessionView>,
    /// Revision an optimistic local layout change was applied on top of;
    /// snapshots up to it are stale and ignored. See `call_optimistic`.
    pub(crate) ahead_of: Option<u64>,
    /// Split ratios applied locally but not yet sent (window, path, ratio);
    /// `ratio_flush` sends them once resizing pauses.
    pub(crate) pending_ratios: Vec<crate::palette::PendingRatio>,
    pub(crate) ratio_flush: Option<Task<()>>,
    /// The divider being dragged, from mouse-down to mouse-up.
    pub(crate) divider_drag: Option<crate::layout::DividerDrag>,
    /// Session to show once the host connects (restored or requested).
    pub(crate) wanted_session: Option<String>,
    pub(crate) panes: HashMap<String, (Entity<Pane>, Subscription)>,
    /// Browser blocks' panes (terminal blocks are in `panes`).
    pub(crate) browsers: HashMap<String, (Entity<crate::browser_pane::BrowserPane>, Subscription)>,
    /// The server's focused block of the active window, as last applied.
    server_focus: Option<String>,
    /// A blank browser this client created: its address bar gets focus
    /// once its pane exists.
    pub(crate) address_pending: Option<String>,
    pub(crate) focus: FocusHandle,
    pub(crate) switcher: Option<Switcher>,
    pub(crate) add_host: Option<AddHostSheet>,
    pub(crate) palette: Option<crate::palette::Palette>,
    pub(crate) picker: Option<crate::window_picker::WindowPicker>,
    pub(crate) motion: Motion,
    pub(crate) appearance_menu: crate::appearance::AppearanceMenu,
    pub(crate) new_tab_menu: crate::new_tab_menu::NewTabMenu,
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
    /// The window surface easing toward the focused block's backdrop, the
    /// theme background it started from, and this frame's chrome colours.
    blend: crate::adaptive::Blend,
    blend_theme: u32,
    pub(crate) chrome: crate::adaptive::Chrome,
    /// System › Reduce transparency, kept current by `_transparency`.
    reduce_transparency: bool,
    _transparency: Task<()>,
    /// The window background appearance last applied (blurred or not).
    blurred: Option<bool>,
    /// This window's snapshot of the app preferences (see `sync_hosts`).
    state: AppState,
    _checkpoints: Task<()>,
    /// When each block was last checkpointed.
    checkpointed: HashMap<String, Instant>,
    _quit: Subscription,
    /// Window bounds and activation.
    _window: [Subscription; 2],
    /// `shared`'s changes, connection events and offer events.
    _hosts: [Subscription; 3],
}

impl Workspace {
    /// A window on the process's hosts, showing `session` (host index,
    /// session id) or, without one, the last session of this Mac.
    pub fn new(shared: Entity<HostsState>, session: Option<(usize, String)>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (rt, home) = (shared.read(cx).rt.clone(), shared.read(cx).home.clone());
        let state = shared.read(cx).app_state().clone();
        let last = || std::env::var("SILO_SESSION").ok().filter(|s| !s.is_empty()).or_else(|| state.last_session.clone());
        let requested = session.as_ref().map(|(_, id)| id.clone());
        let (active_host, wanted_session) = session.map_or_else(|| (0, last()), |(host, id)| (host, Some(id)));
        let checkpoints = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(cache::CHECKPOINT_TICK).await;
                if this.update(cx, |this, cx| this.checkpoint_next(cx)).is_err() {
                    return;
                }
            }
        });
        // `HostsState` flushes the caches after every window's checkpoints.
        let quit = cx.on_app_quit(|this, cx| {
            this.checkpoint_all(cx);
            async {}
        });
        // A window closed mid theme preview must not leave it installed.
        cx.on_release(|this, cx| this.release_preview(cx)).detach();
        let hosts_changed = cx.observe(&shared, |this, _, cx| {
            this.sync_hosts(cx);
            cx.notify();
        });
        let host_events = cx.subscribe_in(&shared, window, |this, _, event: &HostEvent, window, cx| this.host_event(event, window, cx));
        let offer_events =
            cx.subscribe_in(&shared, window, |this, _, event: &crate::offers::OfferEvent, window, cx| this.offer_event(event, window, cx));
        let mut reduce = crate::material::watch();
        let reduce_transparency = *reduce.borrow_and_update();
        let transparency = cx.spawn(async move |this, cx| {
            while reduce.changed().await.is_ok() {
                let value = *reduce.borrow_and_update();
                tracing::debug!(reduce = value, "reduce transparency");
                if this
                    .update(cx, |this, cx| {
                        this.reduce_transparency = value;
                        this.refresh_settings(None, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let mut this = Self {
            rt,
            home: home.clone(),
            shared,
            hosts: Vec::new(),
            active_host,
            status: Some("connecting to server…".into()),
            flashing: None,
            upgrading: false,
            notice: None,
            view: None,
            ahead_of: None,
            pending_ratios: Vec::new(),
            ratio_flush: None,
            divider_drag: None,
            wanted_session,
            panes: HashMap::new(),
            browsers: HashMap::new(),
            server_focus: None,
            address_pending: None,
            focus: cx.focus_handle(),
            switcher: None,
            add_host: None,
            palette: None,
            picker: None,
            motion: Motion::default(),
            appearance_menu: crate::appearance::AppearanceMenu::new(window, cx),
            new_tab_menu: crate::new_tab_menu::NewTabMenu::new(window, cx),
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
            blend: crate::adaptive::Blend::new(theme::background()),
            blend_theme: theme::background(),
            chrome: crate::adaptive::Chrome::on(theme::background()),
            reduce_transparency,
            _transparency: transparency,
            blurred: None,
            state,
            _checkpoints: checkpoints,
            checkpointed: HashMap::new(),
            _quit: quit,
            // Fullscreen changes the titlebar inset; re-render at once.
            _window: [
                cx.observe_window_bounds(window, |_, _, cx| cx.notify()),
                // A remote write held while the app was in the background
                // lands once the user is back at its focused block.
                cx.observe_window_activation(window, |this, window, cx| {
                    let Some(pane) = this.focused_pane(window, cx).filter(|_| window.is_window_active()) else { return };
                    let (block, host) = (pane.read(cx).block_id.clone(), pane.read(cx).host.clone());
                    if let Some(text) = crate::clipboard::writes(|w| w.take(&block, Instant::now())) {
                        this.copied(text, host, cx);
                    }
                }),
            ],
            _hosts: [hosts_changed, host_events, offer_events],
        };
        this.sync_hosts(cx);
        // Paint the cached tree until the server answers (unless another
        // session was asked for).
        let cached = this.shared.read(cx).tree(this.active_host).map(|t| t.view.session.id.clone());
        if requested.is_none() || requested == cached {
            this.show_cached(window, cx);
        }
        if let Some(control) = this.control() {
            this.connected(this.active_host, control, window, cx);
        }
        this
    }

    /// Copy the shared hosts and app preferences into this window's
    /// snapshot.
    pub(crate) fn sync_hosts(&mut self, cx: &mut Context<Self>) {
        let shared = self.shared.read(cx);
        self.hosts = shared.entries().to_vec();
        self.state.clone_from(shared.app_state());
    }

    /// Cache of the shown (active) host.
    fn active_cache(&self, cx: &App) -> Option<CacheWriter> {
        self.shared.read(cx).cache(self.active_host)
    }

    /// Checkpoint one changed pane of the shown host, the one saved
    /// longest ago, at most every `CHECKPOINT_INTERVAL` per block. Capture
    /// encodes the snapshot here on the UI thread; the write is queued to
    /// the host's cache worker.
    fn checkpoint_next(&mut self, cx: &mut Context<Self>) {
        if self.active_cache(cx).is_none() {
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
        let (Some(writer), Some((pane, _))) = (self.active_cache(cx), self.panes.get(id)) else {
            return;
        };
        if let Some(checkpoint) = pane.update(cx, |pane, _| pane.checkpoint()) {
            writer.save_block(id.to_owned(), checkpoint);
            self.checkpointed.insert(id.to_owned(), Instant::now());
        }
    }

    /// Remember the shown host's live tree (in memory and its cache).
    fn save_tree(&mut self, cx: &mut Context<Self>) {
        let Some(view) = &self.view else { return };
        let entry = &self.hosts[self.active_host];
        if entry.control.is_none() {
            return;
        }
        let tree = CachedTree { sessions: entry.sessions.clone(), view: view.clone() };
        let host = self.active_host;
        self.shared.update(cx, |shared, _| shared.save_tree(host, tree));
    }

    /// Paint the active host's last known tree until its server answers.
    fn show_cached(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = self.shared.read(cx).tree(self.active_host).cloned() else {
            return;
        };
        self.status = Some(OFFLINE.into());
        self.set_view(tree.view, window, cx);
    }

    pub(crate) fn control(&self) -> Option<Control> {
        self.hosts.get(self.active_host).and_then(|h| h.control.clone())
    }

    pub(crate) fn block_info(&self, block: &str) -> Option<&silo_proto::model::BlockInfo> {
        self.view.as_ref()?.blocks.iter().find(|b| b.id == block)
    }

    /// Whether this window's host has `feature` (see `silo_proto::compat`);
    /// assumed while it is not connected.
    pub(crate) fn host_supports(&self, feature: &str) -> bool {
        self.control().is_none_or(|c| c.peer_supports(feature))
    }

    /// What this window's host cannot do, said once.
    pub(crate) fn not_supported(&self) -> String {
        format!("Not supported by {}", self.hosts[self.active_host].info.name)
    }

    /// The message for a failed request: an older host's missing method
    /// says so, anything else is `what`.
    pub(crate) fn failed(&self, error: &anyhow::Error, what: &str) -> String {
        if error.is::<silo_client::NotSupported>() { self.not_supported() } else { what.to_owned() }
    }

    /// A connection event, already applied to the shared hosts.
    fn host_event(&mut self, event: &HostEvent, window: &mut Window, cx: &mut Context<Self>) {
        let host = event.host;
        let active = host == self.active_host;
        // This window's last view of the host, before the event.
        let old = self.hosts.get(host).map(|h| h.state.clone());
        self.sync_hosts(cx);
        match &event.event {
            ConnEvent::Connected(control) => self.connected(host, control.clone(), window, cx),
            ConnEvent::Disconnected(reason) => {
                // Retries repeat the same failure; only a change refreshes
                // the settings panel (which would reset its confirmations).
                if !matches!(&old, Some(HostState::Unreachable(old)) if old == reason) {
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
            ConnEvent::Quit | ConnEvent::Addr(_) => {}
            ConnEvent::NotPaired | ConnEvent::Incompatible | ConnEvent::IncompatibleServer(_) => {
                self.refresh_settings(None, cx);
                if active {
                    // A different build keeps the last content painted,
                    // read-only, under the card.
                    self.motion.reset();
                    self.status = None;
                }
            }
            ConnEvent::Event(Event::SessionsChanged { .. }) => self.save_tree(cx),
            ConnEvent::Resync if active => {
                let shown = self.view.as_ref().map(|v| v.session.id.clone());
                self.choose_session(shown, window, cx);
            }
            ConnEvent::Resync => {}
            ConnEvent::Event(_) if !active => {}
            ConnEvent::Event(Event::SessionChanged { view }) => {
                let stale = self.ahead_of.is_some_and(|base| view.session.revision <= base);
                if !stale && self.view.as_ref().is_some_and(|v| v.session.id == view.session.id) {
                    let view = self.with_pending_ratios(view.clone());
                    self.set_view(view, window, cx);
                }
            }
            ConnEvent::Event(Event::SessionClosed { session_id }) => {
                if self.view.as_ref().is_some_and(|v| v.session.id == *session_id) {
                    // Its blocks are gone with it: nothing to disown.
                    self.panes.clear();
                    self.browsers.clear();
                    self.view = None;
                    self.server_focus = None;
                    self.address_pending = None;
                    self.motion.reset();
                    self.focus.focus(window, cx);
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
                    *info = block.clone();
                }
            }
            ConnEvent::Event(Event::OpenSession { session_id }) => {
                window.activate_window();
                cx.activate(true);
                self.open_session(host, session_id.clone(), window, cx);
            }
            ConnEvent::Event(Event::BrowserCommand { block_id, command }) => {
                if let Some((pane, _)) = self.browsers.get(block_id) {
                    pane.update(cx, |pane, cx| pane.apply_command(command, cx));
                }
            }
            ConnEvent::Event(Event::BlockBell { .. } | Event::HostOffered { .. }) => {}
        }
        cx.notify();
    }

    /// `host` connected (or was already connected when this window opened).
    fn connected(&mut self, host: usize, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        self.upgrading = false;
        self.refresh_settings(None, cx);
        if let Some(reason) = control.hello().server.remote_error.as_deref().filter(|_| host == 0) {
            tracing::warn!("server remote access unavailable: {reason}");
        }
        if host == 0 && self.settings.is_some() {
            self.refresh_devices(cx);
        }
        if host != self.active_host {
            return;
        }
        // The view after a reconnect only records ids.
        self.motion.reset();
        self.clear_status();
        self.upgrade_connected(cx);
        for (pane, _) in self.panes.values() {
            pane.update(cx, |pane, cx| pane.set_control(control.clone(), cx));
        }
        for (pane, _) in self.browsers.values() {
            pane.update(cx, |pane, cx| pane.set_control(control.clone(), cx));
        }
        let preferred = self.view.as_ref().map(|v| v.session.id.clone()).or_else(|| self.wanted_session.take());
        self.choose_session(preferred, window, cx);
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
            let cached = self.shared.read(cx).tree(host).is_some_and(|t| t.view.session.id == session_id);
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
        self.wanted_session = self.shared.read(cx).tree(host).map(|t| t.view.session.id.clone());
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
        let host = self.active_host;
        self.shared.update(cx, |shared, _| shared.put_restored(host, mirrors));
    }

    /// Drop every pane (closing its data stream) and give up ownership.
    pub(crate) fn detach_view(&mut self) {
        let blocks: Vec<String> = self.panes.keys().chain(self.browsers.keys()).cloned().collect();
        self.panes.clear();
        self.browsers.clear();
        self.view = None;
        self.server_focus = None;
        self.address_pending = None;
        self.motion.reset();
        if !blocks.is_empty() {
            self.call(Method::DisownBlock { block_ids: blocks });
        }
    }

    /// Create a session on this window's host (named, or numbered by the
    /// server) and show it, in the focused block's directory, else the
    /// server's home.
    pub(crate) fn create_session(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.active_host;
        let Some(control) = self.control() else {
            return;
        };
        let cwd = self.focused_block(window, cx).and_then(|b| self.cwd_of(&b, cx));
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
                    this.status = Some(this.failed(&error, "Could not create the session."));
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

    /// Attach to `preferred`, else the newest session, else a new one (a
    /// host is never shown without a session, like servers keep one).
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
                    let spawn = SpawnSpec { cwd: None, command: Vec::new(), size: None };
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
                        this.status = Some(this.failed(&error, "Could not open the session."));
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
                this.clear_status();
                this.set_view(view, window, cx);
                this.upgrade_connected(cx);
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
        if self.active_host == 0 {
            let id = view.session.id.clone();
            self.shared.update(cx, |shared, cx| shared.update_state(|s| s.last_session = Some(id), cx));
        }
        self.track_motion(&view, window, cx);
        let live = self.control().is_some();
        if live && let Some(old) = &self.view {
            let (host, closed) = (self.active_host, cache::closed_blocks(old, &view));
            self.shared.update(cx, |shared, _| shared.forget_blocks(host, closed));
        }
        self.view = Some(view);
        let view = self.view.clone().expect("view");
        // Panes for every block of the active window; keep panes of other
        // windows that are still alive, drop the rest.
        let all: Vec<String> = view.session.windows.iter().flat_map(crate::floats::window_blocks).collect();
        self.panes.retain(|id, _| all.contains(id));
        self.browsers.retain(|id, _| all.contains(id));
        let active = self.active_window().cloned();
        let of_kind =
            |kind| -> Vec<String> { view.blocks.iter().filter(|b| crate::browser_pane::kind_of(Some(b)) == kind).map(|b| b.id.clone()).collect() };
        let browsers = of_kind(crate::browser_pane::Kind::Browser);
        let unsupported = of_kind(crate::browser_pane::Kind::Unsupported);
        if let Some(active) = &active {
            for block in crate::floats::window_blocks(active) {
                if browsers.contains(&block) {
                    self.ensure_browser(&block, window, cx);
                } else if !self.panes.contains_key(&block) && !unsupported.contains(&block) {
                    let control = self.control();
                    let rt = self.rt.clone();
                    let id = block.clone();
                    let host = self.active_host;
                    let host_name = Some(&self.hosts[host].info).filter(|h| !h.local).map(|h| h.name.clone());
                    let restored = self.shared.update(cx, |shared, _| shared.take_restored(host, &block));
                    let pane = cx.new(|cx| Pane::new(id, host_name, rt, control, restored, window, cx));
                    let subscription = cx.subscribe_in(&pane, window, |this, pane, event: &PaneEvent, window, cx| match event {
                        PaneEvent::Activated => {
                            let block = pane.read(cx).block_id.clone();
                            if let Some(text) = crate::clipboard::writes(|w| w.take(&block, Instant::now())) {
                                this.copied(text, pane.read(cx).host.clone(), cx);
                            }
                            this.pane_activated(block);
                            cx.notify();
                        }
                        PaneEvent::NewShell => {
                            let block = pane.read(cx).block_id.clone();
                            this.new_shell(block, cx);
                        }
                        PaneEvent::ClipboardWrite(text) => {
                            let (block, host) = (pane.read(cx).block_id.clone(), pane.read(cx).host.clone());
                            // A remote host writes only while the user sees the block; else on its next focus.
                            let shown =
                                window.is_window_active() && this.active_window().is_some_and(|w| crate::floats::window_blocks(w).contains(&block));
                            let now = Instant::now();
                            if host.is_none() || shown {
                                if crate::clipboard::writes(|w| w.apply(&block, text, now)) {
                                    this.copied(text.clone(), host, cx);
                                }
                            } else {
                                crate::clipboard::writes(|w| w.defer(&block, text.clone(), now));
                            }
                        }
                        PaneEvent::Notice(message) => {
                            this.flash(message.clone(), cx);
                            cx.notify();
                        }
                        PaneEvent::Backdrop => cx.notify(),
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
        self.focus_new_browser(window, cx);
        self.save_tree(cx);
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
        self.motion.forget_blocks(&blocks.removed);
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
        let new_active = view.session.windows.iter().find(|w| w.id == view.session.active_window).or_else(|| view.session.windows.first());
        // Zoom and unzoom grow the pane over its siblings and back.
        if let (Some(old), Some(new)) = (self.active_window(), new_active)
            && old.id == new.id
            && old.zoomed != new.zoomed
        {
            let block = if new.zoomed { new.focused.clone() } else { old.focused.clone() };
            let blocks = new.layout.blocks();
            if blocks.len() > 1 && blocks.contains(&block) {
                tracing::debug!(%block, zoomed = new.zoomed, "zoom motion");
                let settle = if self.frosted() { crate::motion::ZOOM_SETTLE } else { std::time::Duration::ZERO };
                self.motion.start_zoom(block, new.zoomed, settle, now);
            }
        }
        // Pane motion only inside a window that stays active and unzoomed;
        // tab motion covers whole windows coming and going.
        let old_active = self.active_window().filter(|w| !w.zoomed).cloned();
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
                let fill = self.card_fill(id, cx);
                self.motion.pane_ghosts.push(PaneGhost { frame, slot, edge, title, fill, started: now });
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

    fn copied(&mut self, text: String, host: Option<String>, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.flash(crate::clipboard::copied_label(host.as_deref()), cx);
        cx.notify();
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
            let error = match task.await {
                Ok(Ok(_)) => return,
                Ok(Err(error)) => error,
                Err(error) => error.into(),
            };
            let _ = this.update(cx, |this, cx| {
                if this.ahead_of.is_some() && this.ahead_of == base {
                    this.ahead_of = None;
                    revert(this);
                }
                this.flash(this.failed(&error, failure), cx);
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
        tracing::debug!(?density, "appearance");
        self.shared.update(cx, |shared, cx| shared.update_state(|s| s.density = density, cx));
    }

    pub(crate) fn set_pane_headers(&mut self, show: bool, cx: &mut Context<Self>) {
        tracing::debug!(show, "pane headers");
        self.shared.update(cx, |shared, cx| shared.update_state(|s| s.show_pane_headers = show, cx));
    }

    pub(crate) fn match_running_app(&self) -> bool {
        self.state.match_running_app
    }

    pub(crate) fn set_match_running_app(&mut self, on: bool, cx: &mut Context<Self>) {
        tracing::debug!(on, "match running app");
        self.shared.update(cx, |shared, cx| shared.update_state(|s| s.match_running_app = on, cx));
    }

    /// The background block `id`'s program or page paints, when the chrome
    /// matches running apps.
    pub(crate) fn backdrop(&self, id: &str, cx: &gpui::App) -> Option<u32> {
        let adaptive = if self.frosted() { self.state.tint_with_app } else { self.state.match_running_app };
        if !adaptive {
            return None;
        }
        match (self.panes.get(id), self.browsers.get(id)) {
            (Some((pane, _)), _) => pane.read(cx).backdrop(),
            (None, Some((pane, _))) => pane.read(cx).backdrop(),
            (None, None) => None,
        }
    }

    /// The fill of block `id`'s card: its backdrop, else the theme's.
    pub(crate) fn card_fill(&self, id: &str, cx: &gpui::App) -> u32 {
        self.backdrop(id, cx).unwrap_or_else(theme::background)
    }

    /// The material the user chose and why Frosted cannot render, if so.
    pub(crate) fn material(&self) -> (crate::material::Material, Result<(), crate::material::Unavailable>) {
        let available = crate::material::availability(crate::material::blur_supported(), self.reduce_transparency);
        (self.state.material, available)
    }

    /// The window renders frosted (blurred, with veils).
    pub(crate) fn frosted(&self) -> bool {
        let (choice, available) = self.material();
        crate::material::effective(choice, available) == crate::material::Material::Frosted
    }

    pub(crate) fn set_material(&mut self, material: crate::material::Material, cx: &mut Context<Self>) {
        tracing::debug!(?material, "material");
        self.shared.update(cx, |shared, cx| shared.update_state(|s| s.material = material, cx));
    }

    pub(crate) fn tint_with_app(&self) -> bool {
        self.state.tint_with_app
    }

    pub(crate) fn set_tint_with_app(&mut self, on: bool, cx: &mut Context<Self>) {
        self.shared.update(cx, |shared, cx| shared.update_state(|s| s.tint_with_app = on, cx));
    }

    pub(crate) fn translucent_apps(&self) -> bool {
        self.state.translucent_apps
    }

    pub(crate) fn set_translucent_apps(&mut self, on: bool, cx: &mut Context<Self>) {
        self.shared.update(cx, |shared, cx| shared.update_state(|s| s.translucent_apps = on, cx));
    }

    /// The colours of a card filled with `fill`: when frosted, a veil at
    /// the window's one alpha, so card and window are one material.
    pub(crate) fn card_chrome(&self, fill: u32) -> crate::adaptive::Chrome {
        if self.frosted() { crate::adaptive::Chrome::veiled(fill, self.chrome.alpha) } else { crate::adaptive::Chrome::on(fill) }
    }

    /// How block `id`'s terminal paints cell backgrounds on its card of
    /// colour `fill`: frosted, the default background and the card's colour
    /// are left to the card (veiled, or its opaque look when floating).
    pub(crate) fn translucency(&self, id: &str, fill: u32, card: crate::adaptive::Chrome, cx: &gpui::App) -> Option<silo_render::Translucency> {
        let byte = |a: f32| (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        self.frosted().then(|| silo_render::Translucency {
            card: fill,
            veiled: self.translucent_apps().then(|| self.panes.get(id).and_then(|(p, _)| p.read(cx).backdrop())).flatten(),
            opaque_alpha: byte(crate::material::CELL_OPAQUE.max(card.alpha)),
        })
    }

    /// Blur behind the window exactly while it renders frosted.
    fn apply_material(&mut self, window: &mut Window) {
        let frosted = self.frosted();
        if self.blurred != Some(frosted) {
            self.blurred = Some(frosted);
            window.set_background_appearance(if frosted {
                gpui::WindowBackgroundAppearance::Blurred
            } else {
                gpui::WindowBackgroundAppearance::Opaque
            });
        }
    }

    /// Step the window surface toward the focused block's backdrop (fully
    /// when its pane fills the window) and derive this frame's chrome.
    fn step_chrome(&mut self, now: Instant, window: &mut Window, cx: &mut Context<Self>) {
        let theme_bg = theme::background();
        let alone = self.active_window().is_some_and(|w| w.zoomed || crate::floats::window_blocks(w).len() <= 1);
        let backdrop = self.focused_block(window, cx).and_then(|id| self.backdrop(&id, cx));
        let target = crate::adaptive::chrome_target(theme_bg, backdrop, alone);
        if self.blend_theme != theme_bg {
            self.blend_theme = theme_bg;
            self.blend = crate::adaptive::Blend::new(target);
        }
        let (surface, moving) = self.blend.step(target, now, !cx.reduce_motion());
        if moving {
            window.request_animation_frame();
        }
        self.chrome = if self.frosted() {
            crate::adaptive::Chrome::veiled(surface, self.veil_alpha(surface, cx))
        } else {
            crate::adaptive::Chrome::on(surface)
        };
        // Floating surfaces drawn after this (this window's overlays) use
        // the same material.
        theme::set_veils((self.chrome.alpha < 1.0).then(|| crate::material::overlay(surface, self.chrome.alpha, theme_bg, theme::foreground())));
    }

    /// The one veil alpha of this frame: the least at which text reads on
    /// the window surface and on every visible card, the theme's terminal
    /// foreground included on terminal cards whose own colour it reads on
    /// (a program on a colour it does not read on uses its own text).
    fn veil_alpha(&self, surface: u32, cx: &gpui::App) -> f32 {
        let (theme_bg, fg) = (theme::background(), theme::foreground());
        let alpha = |color: u32, text: &[u32]| crate::material::readable_alpha(color, crate::material::VEIL, theme_bg, text);
        let mut veil = alpha(surface, &[]);
        for id in self.active_window().map(crate::floats::window_blocks).unwrap_or_default() {
            let fill = self.card_fill(&id, cx);
            let fg_text = self.panes.contains_key(&id) && theme::contrast(fg, fill) >= theme::TEXT_CONTRAST;
            veil = veil.max(if fg_text { alpha(fill, &[fg]) } else { alpha(fill, &[]) });
        }
        veil
    }

    /// The stored UI font preference (empty = Automatic).
    pub(crate) fn ui_font_family(&self) -> &str {
        &self.state.ui_font_family
    }

    /// Change the UI font (empty = Automatic): persist the exact family and
    /// re-render all chrome in it. The terminal font is untouched.
    pub(crate) fn set_ui_font_family(&mut self, family: String, window: &mut Window, cx: &mut Context<Self>) {
        let family = family.trim().to_owned();
        tracing::debug!(%family, "ui font");
        theme::apply_ui_font(&family, cx);
        self.shared.update(cx, |shared, cx| shared.update_state(|s| s.ui_font_family = family, cx));
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
        if self.settings.is_some() || self.picker.is_some() {
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
        self.step_chrome(now, window, cx);
        self.apply_material(window);
        // A click on bare chrome (titlebar, gaps, menus) focuses the
        // workspace itself; hand focus back to the terminal so typing
        // keeps working. Overlays own their focus and are left alone.
        if self.focus.is_focused(window)
            && self.switcher.is_none()
            && self.palette.is_none()
            && self.picker.is_none()
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
                    && this.picker.is_none()
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
        // Frosted: no fill stacks under the cards; the titlebar, gutters
        // and cards each paint the one veil.
        let frosted = self.frosted();
        let cards = self.active_window().is_some();
        let body: AnyElement = match self.active_window().cloned() {
            Some(w) => match card {
                // The last content stays painted, read-only, around the card.
                Some(card) => div()
                    .relative()
                    .size_full()
                    .child(self.render_body(&w, now, window, cx))
                    .child(div().id("incompatible-scrim").occlude().absolute().inset_0().child(card))
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
        let new_tab = self.render_new_tab_menu(window, cx);
        let settings = self.render_settings(window, cx);
        let titlebar = self.render_titlebar(window, cx);
        let switcher = self.render_switcher(window, cx);
        let sheet = self.render_add_host(cx);
        let offers = self.render_offers(window, cx);
        let palette = self.render_palette(window, cx);
        let picker = self.render_picker(window, cx);
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .when(!frosted, |el| el.bg(self.chrome.fill()))
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
            .child(div().flex_none().when(frosted, |el| el.bg(self.chrome.fill())).child(titlebar))
            .child(div().flex_1().min_h_0().when(frosted && !cards, |el| el.bg(self.chrome.fill())).child(body))
            .when_some(self.status_bar(), |el, status| {
                el.child(
                    div().absolute().bottom(px(8.0)).left_0().right_0().flex().justify_center().child(
                        div()
                            .px(px(10.0))
                            .py(px(4.0))
                            .continuous(8.0)
                            .bg(theme::overlay_fill())
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
            .children(new_tab)
            .children(settings)
            .children(switcher)
            .children(offers)
            .children(sheet)
            .children(palette)
            .children(picker)
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
