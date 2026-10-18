//! The app window: one session of one host, its windows as tabs, and the
//! active window's split layout of terminal panes. The titlebar session
//! pill switches sessions and hosts (see `switcher.rs`).

use std::{collections::HashMap, path::PathBuf};

use gpui::{
    AnyElement, Context, Entity, FocusHandle, IntoElement, Render, Subscription, Task, Window, actions, div, prelude::*,
    px, relative, rgb,
};
use silo_client::Control;
use silo_proto::{
    control::{Created, Event, Method, SpawnSpec},
    model::{Axis, Layout, SessionSummary, SessionView, WindowInfo},
};

use crate::{
    add_host::AddHostSheet,
    conn::ConnEvent,
    hosts::{self, AppState, HostInfo, HostState},
    pane::{Pane, PaneEvent},
    switcher::Switcher,
    theme,
};

actions!(
    silo,
    [
        NewWindow,
        SplitRight,
        SplitDown,
        CloseBlock,
        FocusPrevious,
        FocusNext,
        ToggleZoom,
        Copy,
        Paste,
        Quit,
        NewSession,
        ToggleSessions,
        SelectWindow1,
        SelectWindow2,
        SelectWindow3,
        SelectWindow4,
        SelectWindow5,
        SelectWindow6,
        SelectWindow7,
        SelectWindow8,
        SelectWindow9,
    ]
);

pub const TITLEBAR_HEIGHT: f32 = 46.0;

pub struct HostEntry {
    pub info: HostInfo,
    pub state: HostState,
    pub control: Option<Control>,
    /// Sessions, oldest first (the order of their ⌃⌘ shortcuts).
    pub sessions: Vec<SessionSummary>,
    _conn: Option<Task<()>>,
}

pub struct Workspace {
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) home: PathBuf,
    pub(crate) hosts: Vec<HostEntry>,
    pub(crate) active_host: usize,
    pub(crate) status: Option<String>,
    pub(crate) view: Option<SessionView>,
    /// Session to show once the host connects (restored or requested).
    pub(crate) wanted_session: Option<String>,
    pub(crate) panes: HashMap<String, (Entity<Pane>, Subscription)>,
    /// The server's focused block of the active window, as last applied.
    server_focus: Option<String>,
    pub(crate) focus: FocusHandle,
    pub(crate) switcher: Option<Switcher>,
    pub(crate) add_host: Option<AddHostSheet>,
    state: AppState,
}

impl Workspace {
    pub fn new(rt: tokio::runtime::Handle, home: PathBuf, exe: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = hosts::load_state(&home);
        let wanted_session = std::env::var("SILO_SESSION").ok().filter(|s| !s.is_empty()).or_else(|| state.last_session.clone());
        let mut this = Self {
            rt,
            home: home.clone(),
            hosts: Vec::new(),
            active_host: 0,
            status: Some("connecting to server…".into()),
            view: None,
            wanted_session,
            panes: HashMap::new(),
            server_focus: None,
            focus: cx.focus_handle(),
            switcher: None,
            add_host: None,
            state,
        };
        this.add_host_entry(HostInfo::local(home.clone(), exe), window, cx);
        for remote in hosts::load(&home) {
            this.add_host_entry(HostInfo::remote(&remote), window, cx);
        }
        this
    }

    pub(crate) fn add_host_entry(&mut self, info: HostInfo, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.hosts.len();
        let mut events = crate::conn::start(&self.rt, info.transport.clone());
        let conn = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this.update_in(cx, |this, window, cx| this.conn_event(index, event, window, cx)).is_err() {
                    return;
                }
            }
        });
        self.hosts.push(HostEntry { info, state: HostState::Connecting, control: None, sessions: Vec::new(), _conn: Some(conn) });
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
                self.refresh_sessions(host, cx);
                if active {
                    self.status = None;
                    for (pane, _) in self.panes.values() {
                        pane.update(cx, |pane, cx| pane.set_control(control.clone(), cx));
                    }
                    let preferred = self.view.as_ref().map(|v| v.session.id.clone()).or_else(|| self.wanted_session.take());
                    self.choose_session(preferred, window, cx);
                }
            }
            ConnEvent::Disconnected(reason) => {
                let entry = &mut self.hosts[host];
                entry.control = None;
                entry.state = HostState::Unreachable(reason.clone());
                if active {
                    self.status = Some(format!("server unavailable: {reason}"));
                }
            }
            ConnEvent::Event(Event::SessionsChanged { mut sessions }) => {
                sessions.sort_by_key(|s| s.created_at);
                self.hosts[host].sessions = sessions;
            }
            ConnEvent::Event(_) if !active => {}
            ConnEvent::Event(Event::SessionChanged { view }) => {
                if self.view.as_ref().is_some_and(|v| v.session.id == view.session.id) {
                    self.set_view(view, window, cx);
                }
            }
            ConnEvent::Event(Event::SessionClosed { session_id }) => {
                if self.view.as_ref().is_some_and(|v| v.session.id == session_id) {
                    self.view = None;
                    self.choose_session(None, window, cx);
                }
            }
            ConnEvent::Event(Event::BlockChanged { block }) => {
                if let Some((pane, _)) = self.panes.get(&block.id) {
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
            ConnEvent::Event(Event::BlockBell { .. }) => {}
        }
        cx.notify();
    }

    fn refresh_sessions(&mut self, host: usize, cx: &mut Context<Self>) {
        let Some(control) = self.hosts[host].control.clone() else { return };
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
        self.detach_view();
        self.active_host = host;
        if self.control().is_none() {
            self.wanted_session = Some(session_id);
            self.status = Some(format!("{} is not connected", self.hosts[host].info.name));
            cx.notify();
            return;
        }
        self.choose_session(Some(session_id), window, cx);
    }

    /// Drop every pane (closing its data stream) and give up ownership.
    fn detach_view(&mut self) {
        let blocks: Vec<String> = self.panes.keys().cloned().collect();
        self.panes.clear();
        self.view = None;
        self.server_focus = None;
        if !blocks.is_empty() {
            self.call(Method::DisownBlock { block_ids: blocks });
        }
    }

    /// Create a session (named, or numbered by the server) and show it.
    pub(crate) fn create_session(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.active_host;
        let Some(control) = self.control() else { return };
        let cwd = self.focused_block(window, cx).and_then(|b| self.cwd_of(&b, cx)).or_else(|| std::env::var("HOME").ok());
        let task = self.rt.spawn(async move {
            control.call::<Created>(Method::SessionCreate { name, spawn: SpawnSpec { cwd, command: Vec::new(), size: None } }).await
        });
        cx.spawn_in(window, async move |this, cx| match task.await {
            Ok(Ok(created)) => {
                let _ = this.update_in(cx, |this, window, cx| this.open_session(host, created.session_id, window, cx));
            }
            Ok(Err(error)) => {
                let _ = this.update(cx, |this, cx| {
                    this.status = Some(format!("{error:#}"));
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
    fn choose_session(&mut self, preferred: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
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
                    let created: Created = control.call(Method::SessionCreate { name: None, spawn }).await?;
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
                        this.status = Some(format!("{error:#}"));
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
        window.set_window_title(&format!("{} — silo", view.session.name));
        if self.active_host == 0 && self.state.last_session.as_deref() != Some(&view.session.id) {
            self.state.last_session = Some(view.session.id.clone());
            hosts::save_state(&self.home, &self.state);
        }
        self.view = Some(view);
        let view = self.view.as_ref().expect("view");
        // Panes for every block of the active window; keep panes of other
        // windows that are still alive, drop the rest.
        let all: Vec<String> = view.session.windows.iter().flat_map(|w| w.layout.blocks()).collect();
        self.panes.retain(|id, _| all.contains(id));
        let active = self.active_window().cloned();
        if let Some(active) = &active {
            for block in active.layout.blocks() {
                if !self.panes.contains_key(&block) {
                    let control = self.control();
                    let rt = self.rt.clone();
                    let id = block.clone();
                    let pane = cx.new(|cx| Pane::new(id, rt, control, window, cx));
                    let subscription = cx.subscribe_in(&pane, window, |this, pane, event: &PaneEvent, _, cx| match event {
                        PaneEvent::Activated => {
                            let block = pane.read(cx).block_id.clone();
                            this.pane_activated(block);
                            cx.notify();
                        }
                    });
                    self.panes.insert(block, (pane, subscription));
                }
            }
        }
        let infos = view.blocks.clone();
        for info in infos {
            if let Some((pane, _)) = self.panes.get(&info.id) {
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
            let local = self.panes.values().any(|(p, _)| p.read(cx).focus.is_focused(window));
            if self.server_focus.as_deref() != Some(&focused) || !local {
                self.server_focus = Some(focused.clone());
                self.focus_block(&focused, window, cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn focus_block(&self, block: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((pane, _)) = self.panes.get(block) {
            let handle = pane.read(cx).focus.clone();
            handle.focus(window, cx);
        }
    }

    /// Return keyboard focus to the terminal after closing an overlay.
    pub(crate) fn refocus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(block) = self.active_window().map(|w| w.focused.clone()) {
            self.focus_block(&block, window, cx);
        }
    }

    fn pane_activated(&mut self, block: String) {
        if self.server_focus.as_deref() == Some(&block) {
            return;
        }
        self.server_focus = Some(block.clone());
        self.call(Method::BlockFocus { block_id: block });
    }

    pub(crate) fn call(&self, method: Method) {
        let Some(control) = self.control() else { return };
        self.rt.spawn(async move {
            if let Err(error) = control.call_value(method).await {
                tracing::warn!("request failed: {error:#}");
            }
        });
    }

    fn focused_block(&self, window: &Window, cx: &gpui::App) -> Option<String> {
        self.panes
            .iter()
            .find(|(_, (p, _))| p.read(cx).focus.is_focused(window))
            .map(|(id, _)| id.clone())
            .or_else(|| self.active_window().map(|w| w.focused.clone()))
    }

    fn focused_pane(&self, window: &Window, cx: &gpui::App) -> Option<Entity<Pane>> {
        let block = self.focused_block(window, cx)?;
        self.panes.get(&block).map(|(p, _)| p.clone())
    }

    fn cwd_of(&self, block: &str, cx: &gpui::App) -> Option<String> {
        self.panes.get(block).and_then(|(p, _)| p.read(cx).info.as_ref().and_then(|i| i.cwd.clone()))
    }

    pub(crate) fn new_window(&mut self, _: &NewWindow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.view else { return };
        let cwd = self.focused_block(window, cx).and_then(|b| self.cwd_of(&b, cx));
        self.call(Method::WindowCreate { session_id: view.session.id.clone(), spawn: SpawnSpec { cwd, command: Vec::new(), size: None } });
    }

    fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.focused_block(window, cx) else { return };
        let cwd = self.cwd_of(&block, cx);
        self.call(Method::BlockSplit { block_id: block, axis, spawn: SpawnSpec { cwd, command: Vec::new(), size: None } });
    }

    fn close_block(&mut self, _: &CloseBlock, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(block) = self.focused_block(window, cx) {
            self.call(Method::BlockClose { block_id: block });
        }
    }

    fn cycle_focus(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_window() else { return };
        let blocks = active.layout.blocks();
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

    /// ⌘1–9: the n-th tab, or the n-th session row while the switcher is
    /// open.
    fn number_key(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.is_some() {
            self.switch_to_row(index, window, cx);
        } else {
            self.select_window(index);
        }
    }

    fn toggle_zoom(&mut self, _: &ToggleZoom, window: &mut Window, cx: &mut Context<Self>) {
        let zoomed = self.active_window().is_some_and(|w| w.zoomed);
        if let Some(block) = self.focused_block(window, cx) {
            self.call(Method::BlockZoom { block_id: block, zoomed: !zoomed });
        }
    }

    pub(crate) fn window_title(&self, w: &WindowInfo, cx: &gpui::App) -> String {
        if let Some(name) = &w.name {
            return name.clone();
        }
        self.panes.get(&w.focused).map_or_else(
            || {
                self.view
                    .as_ref()
                    .and_then(|v| v.blocks.iter().find(|b| b.id == w.focused))
                    .map_or_else(|| "shell".into(), crate::pane::block_title)
            },
            |(p, _)| p.read(cx).title(),
        )
    }

    fn render_layout(&self, layout: &Layout, focused: Option<&str>, multiple: bool) -> AnyElement {
        match layout {
            Layout::Block { id } => {
                let border = if multiple && focused == Some(id.as_str()) { theme::accent() } else { theme::background() };
                let inner: AnyElement = match self.panes.get(id) {
                    Some((pane, _)) => pane.clone().into_any_element(),
                    None => div().size_full().bg(rgb(theme::background())).into_any_element(),
                };
                div().size_full().min_w_0().min_h_0().border_1().border_color(rgb(border)).child(inner).into_any_element()
            }
            Layout::Split { axis, ratio, first, second } => {
                let ratio = ratio.clamp(0.05, 0.95);
                let a = self.render_layout(first, focused, multiple);
                let b = self.render_layout(second, focused, multiple);
                let container = div().size_full().min_w_0().min_h_0().flex();
                match axis {
                    Axis::Horizontal => container
                        .flex_row()
                        .child(div().h_full().w(relative(ratio)).flex_none().min_w_0().child(a))
                        .child(div().h_full().w(px(1.0)).flex_none().bg(rgb(theme::border())))
                        .child(div().h_full().flex_1().min_w_0().child(b))
                        .into_any_element(),
                    Axis::Vertical => container
                        .flex_col()
                        .child(div().w_full().h(relative(ratio)).flex_none().min_h_0().child(a))
                        .child(div().w_full().h(px(1.0)).flex_none().bg(rgb(theme::border())))
                        .child(div().w_full().flex_1().min_h_0().child(b))
                        .into_any_element(),
                }
            }
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focused_block(window, cx);
        let body: AnyElement = match self.active_window() {
            Some(w) if w.zoomed => self.render_layout(&Layout::block(w.focused.clone()), None, false),
            Some(w) => self.render_layout(&w.layout, focused.as_deref(), w.layout.blocks().len() > 1),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(theme::muted()))
                .child(self.status.clone().unwrap_or_default())
                .into_any_element(),
        };
        let titlebar = self.render_titlebar(window, cx);
        let switcher = self.render_switcher(window, cx);
        let sheet = self.render_add_host(cx);
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::background()))
            .text_color(rgb(theme::foreground()))
            .font_family("Menlo")
            .on_action(cx.listener(Self::new_window))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| this.split(Axis::Horizontal, window, cx)))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| this.split(Axis::Vertical, window, cx)))
            .on_action(cx.listener(Self::close_block))
            .on_action(cx.listener(Self::toggle_zoom))
            .on_action(cx.listener(|this, _: &FocusPrevious, window, cx| this.cycle_focus(-1, window, cx)))
            .on_action(cx.listener(|this, _: &FocusNext, window, cx| this.cycle_focus(1, window, cx)))
            .on_action(cx.listener(|this, _: &NewSession, window, cx| {
                this.close_switcher(window, cx);
                this.create_session(None, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSessions, window, cx| this.toggle_switcher(window, cx)))
            .on_action(cx.listener(|this, _: &Copy, window, cx| {
                if let Some(pane) = this.focused_pane(window, cx) {
                    pane.update(cx, |pane, cx| pane.copy(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &Paste, window, cx| {
                if let Some(pane) = this.focused_pane(window, cx) {
                    pane.update(cx, |pane, cx| pane.paste(cx));
                }
            }))
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
            .when_some(self.status.clone().filter(|_| self.view.is_some()), |el, status| {
                el.child(
                    div()
                        .h(px(22.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .bg(rgb(theme::chrome()))
                        .text_size(px(11.0))
                        .text_color(rgb(theme::muted()))
                        .child(status),
                )
            })
            .children(switcher)
            .children(sheet)
    }
}

pub fn bind_keys(cx: &mut gpui::App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("cmd-t", NewWindow, None),
        KeyBinding::new("cmd-d", SplitRight, None),
        KeyBinding::new("cmd-shift-d", SplitDown, None),
        KeyBinding::new("cmd-w", CloseBlock, None),
        KeyBinding::new("cmd-[", FocusPrevious, None),
        KeyBinding::new("cmd-]", FocusNext, None),
        KeyBinding::new("cmd-shift-enter", ToggleZoom, None),
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-shift-n", NewSession, None),
        KeyBinding::new("cmd-shift-s", ToggleSessions, None),
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
