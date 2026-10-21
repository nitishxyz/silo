//! ⌘P command palette and the command dispatcher. Every command — from a
//! key binding, the menu or the palette — runs through [`Workspace::run`].

use std::sync::atomic::Ordering;

use gpui::{
    AnyElement, Context, Entity, FontWeight, HighlightStyle, IntoElement, ScrollHandle, StyledText, Subscription, Window, div, prelude::*, px, rgb,
};
use silo_proto::{
    control::{Direction, Method},
    layout_ops,
    model::Axis,
};

use crate::{status::Status, theme::Continuous as _};

/// Quiet time after the last resize step before ratios go to the server.
const RATIO_FLUSH: std::time::Duration = std::time::Duration::from_millis(120);

/// A split ratio applied locally, not yet sent; `shape` is its window's
/// layout when set, so the path only applies to that same tree.
pub(crate) struct PendingRatio {
    window_id: String,
    path: Vec<usize>,
    ratio: f32,
    shape: silo_proto::model::Layout,
}

impl PendingRatio {
    fn fits(&self, windows: &[silo_proto::model::WindowInfo]) -> bool {
        windows.iter().any(|w| w.id == self.window_id && layout_ops::same_shape(&w.layout, &self.shape))
    }
}
use crate::{
    commands::{Command, fuzzy},
    finder::{self, Preview, Right, Target},
    settings_panel::{SettingsEvent, SettingsSnapshot},
    settings_table::{Choice, Kind, SettingId},
    terminal_prefs::TerminalPrefs,
    text_field::{FieldEvent, TextField},
    theme,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

/// The ⌘P finder (see `finder.rs`) and its sub-steps.
pub struct Palette {
    field: Entity<TextField>,
    /// The search query, restored when an input step goes back.
    query: String,
    /// A choice list's filter.
    filter: String,
    selected: usize,
    step: Step,
    scroll: ScrollHandle,
    error: Option<String>,
    /// Something worth knowing about the last change; not an error.
    note: Option<String>,
    /// Settings as they were when the step opened (or last changed).
    snapshot: SettingsSnapshot,
    /// Recent picks, newest first.
    recent: Vec<String>,
    _sub: Subscription,
}

#[derive(Clone, Debug)]
enum Step {
    Search,
    Input(Input),
    Confirm(Command),
    RemoveHost,
    /// A choice setting's values; the theme is previewed live.
    Choice(SettingId, Option<Box<Preview<TerminalPrefs>>>),
    /// A number setting, typed.
    Number(SettingId),
}

#[derive(Clone, Debug)]
enum Input {
    /// (session id, current name)
    RenameSession(String, String),
    /// (window id, current title)
    RenameTab(String, String),
    /// (host index, current name)
    HostName(usize, String),
    OpenUrl(String),
}

impl Step {
    /// End a theme preview this step runs: the original theme to put back
    /// when another was shown. Any other step, or a step whose preview was
    /// ended, gives `None`.
    fn end_preview(&mut self) -> Option<TerminalPrefs> {
        let Self::Choice(_, preview) = self else { return None };
        preview.take()?.cancel()
    }
}

impl Input {
    fn prompt(&self) -> String {
        match self {
            Self::RenameSession(..) => "Rename session".into(),
            Self::RenameTab(..) => "Rename tab (empty to reset)".into(),
            Self::HostName(..) => "Rename host".into(),
            Self::OpenUrl(_) => "Open in a new browser".into(),
        }
    }

    /// Rename fields start empty, showing the current name.
    fn placeholder(&self) -> String {
        match self {
            Self::RenameSession(_, current) | Self::RenameTab(_, current) | Self::HostName(_, current) => current.clone(),
            Self::OpenUrl(_) => "Address".into(),
        }
    }
}

#[derive(Clone, Debug)]
enum Entry {
    Item(finder::Item),
    Choice { choice: Choice, ranges: Vec<std::ops::Range<usize>> },
    Confirm(Command),
    Back,
    RemoveHost(usize),
}

impl Workspace {
    /// Run a command (key binding, menu or palette).
    pub(crate) fn run(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        // The settings panel is modal: workspace shortcuts must not act on
        // the terminal behind it (⌘D splitting, ⌘W closing a pane, …).
        if self.settings.is_some() && !crate::settings::allowed_while_open(command) {
            if matches!(command, Command::ClosePane | Command::CloseTab) {
                self.close_settings(window, cx);
            }
            return;
        }
        // The window picker is modal the same way; ⌘W closes it.
        if self.picker.is_some() && !crate::window_picker::allowed_while_open(command) {
            if matches!(command, Command::ClosePane | Command::CloseTab) {
                self.close_picker(window, cx);
            }
            return;
        }
        if command.feature().is_some_and(|f| !self.host_supports(f)) {
            return self.flash(Status::warning(self.not_supported()), cx);
        }
        // Restart Server on an incompatible server is the upgrade, which
        // keeps sessions when it can and says so when it could not.
        let upgrade = matches!(command, Command::RestartServer) && self.incompatible().is_some();
        if command.destructive() && !upgrade {
            self.open_palette_step(Step::Confirm(command), String::new(), window, cx);
            return;
        }
        self.execute(command, window, cx);
    }

    /// A command's palette subtitle in the current state.
    pub(crate) fn subtitle(&self, command: Command) -> Option<&'static str> {
        match command {
            Command::RestartServer if self.incompatible().is_some() => Some("Upgrades it to this build"),
            other => other.subtitle(),
        }
    }

    pub(crate) fn execute(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        tracing::debug!(?command, "run");
        if !matches!(command, Command::Copy | Command::Paste | Command::SelectAll) {
            self.close_palette(window, cx);
        }
        if !matches!(command, Command::Copy | Command::Paste | Command::SelectAll | Command::NewTabChooser) {
            self.close_new_tab_menu(window, cx);
        }
        let block = self.focused_block(window, cx);
        let server_command = matches!(command, Command::StartServer | Command::StopServer | Command::RestartServer);
        if server_command && !self.hosts.get(self.active_host).is_some_and(|h| h.info.local) {
            self.flash(Status::info("Switch to This Mac to manage its server"), cx);
            return;
        }
        match command {
            Command::NewSession => {
                self.close_switcher(window, cx);
                self.create_session(None, window, cx);
            }
            Command::NewWindow => self.toggle_picker(window, cx),
            Command::CycleWindows => crate::window_picker::cycle_windows(window, cx),
            Command::RenameSession => {
                if let Some(view) = &self.view {
                    let (id, name) = (view.session.id.clone(), view.session.name.clone());
                    self.open_palette_step(Step::Input(Input::RenameSession(id, name)), String::new(), window, cx);
                }
            }
            Command::CloseSession => {
                if let Some(view) = &self.view {
                    self.call(Method::SessionClose { session_id: view.session.id.clone(), force: false });
                }
            }
            Command::SwitchSession => self.toggle_switcher(window, cx),
            Command::NextSession => self.step_session(1, window, cx),
            Command::PreviousSession => self.step_session(-1, window, cx),
            Command::NewTab => self.new_tab_like_focused(window, cx),
            Command::NewTerminalTab => self.new_window(window, cx),
            Command::NewBrowserTab => self.new_browser_tab(window, cx),
            Command::NewTabChooser => self.toggle_new_tab_menu(window, cx),
            Command::CloseTab => {
                if let Some(w) = self.active_window() {
                    self.call(Method::WindowClose { window_id: w.id.clone(), force: false });
                }
            }
            Command::RenameTab => {
                if let Some(w) = self.active_window().cloned() {
                    let current = w.name.clone().unwrap_or_else(|| self.window_title(&w, cx));
                    self.open_palette_step(Step::Input(Input::RenameTab(w.id.clone(), current)), String::new(), window, cx);
                }
            }
            Command::NextTab => self.step_tab(1),
            Command::PreviousTab => self.step_tab(-1),
            Command::SplitRight => self.split(Axis::Horizontal, window, cx),
            Command::SplitDown => self.split(Axis::Vertical, window, cx),
            Command::NewPane => {
                if let Some(block) = block {
                    let axis = self.auto_axis(&block);
                    self.split_block(block, axis, cx);
                }
            }
            Command::ClosePane => self.close_block(window, cx),
            Command::FocusNextPane => self.cycle_focus(1, window, cx),
            Command::FocusPreviousPane => self.cycle_focus(-1, window, cx),
            Command::GrowPane | Command::ShrinkPane => {
                let delta = if command == Command::GrowPane { layout_ops::RESIZE_STEP } else { -layout_ops::RESIZE_STEP };
                self.resize_focused(|layout, id| layout_ops::resize_share(layout, id, delta).into_iter().collect(), window, cx);
            }
            Command::MovePaneEdgeLeft => self.resize_focused(|l, id| layout_ops::move_edge(l, id, Direction::Left).into_iter().collect(), window, cx),
            Command::MovePaneEdgeRight => {
                self.resize_focused(|l, id| layout_ops::move_edge(l, id, Direction::Right).into_iter().collect(), window, cx)
            }
            Command::MovePaneEdgeUp => self.resize_focused(|l, id| layout_ops::move_edge(l, id, Direction::Up).into_iter().collect(), window, cx),
            Command::MovePaneEdgeDown => self.resize_focused(|l, id| layout_ops::move_edge(l, id, Direction::Down).into_iter().collect(), window, cx),
            Command::EqualizePanes => self.resize_focused(|l, _| layout_ops::equalize(l), window, cx),
            Command::FocusPaneLeft => self.focus_direction(Direction::Left, window, cx),
            Command::FocusPaneRight => self.focus_direction(Direction::Right, window, cx),
            Command::FocusPaneUp => self.focus_direction(Direction::Up, window, cx),
            Command::FocusPaneDown => self.focus_direction(Direction::Down, window, cx),
            Command::ZoomPane => self.toggle_zoom(window, cx),
            Command::FloatPane => self.float_block(window, cx),
            Command::DockPane => self.dock_block(window, cx),
            Command::NewBrowser => {
                if let Some(block) = block {
                    self.new_browser(block, window, cx);
                }
            }
            Command::OpenUrl => {
                if let Some(block) = block {
                    self.open_palette_step(Step::Input(Input::OpenUrl(block)), String::new(), window, cx);
                }
            }
            Command::AddHost => {
                self.close_palette(window, cx);
                self.open_add_host(None, None, window, cx);
            }
            Command::RemoveHost => {
                if self.hosts.iter().any(|h| !h.info.local && !h.removed) {
                    self.open_palette_step(Step::RemoveHost, String::new(), window, cx);
                } else {
                    self.flash(Status::info("No remote hosts to remove"), cx);
                }
            }
            Command::ShareSession => self.open_share_sheet(false, window, cx),
            Command::SharePane => self.open_share_sheet(true, window, cx),
            Command::OpenSharedLink => {
                self.close_palette(window, cx);
                self.open_add_host(None, None, window, cx);
            }
            Command::RenameHost => {
                if self.available(Command::RenameHost, cx) {
                    self.open_rename_host(self.active_host, window, cx);
                } else {
                    self.flash(Status::info("This Mac can't be renamed"), cx);
                }
            }
            Command::Copy => {
                if let Some((browser, _)) = block.as_ref().and_then(|b| self.browsers.get(b)) {
                    browser.read(cx).edit(silo_web::Edit::Copy);
                } else if let Some(pane) = self.focused_pane(window, cx) {
                    pane.update(cx, |pane, cx| pane.copy(cx));
                }
            }
            Command::Paste => {
                if let Some((browser, _)) = block.as_ref().and_then(|b| self.browsers.get(b)) {
                    browser.read(cx).edit(silo_web::Edit::Paste);
                } else if let Some(pane) = self.focused_pane(window, cx) {
                    pane.update(cx, |pane, cx| pane.paste(cx));
                }
            }
            Command::SelectAll => {
                self.close_palette(window, cx);
                if let Some((browser, _)) = block.as_ref().and_then(|b| self.browsers.get(b)) {
                    browser.read(cx).edit(silo_web::Edit::SelectAll);
                } else if let Some(pane) = self.focused_pane(window, cx) {
                    pane.update(cx, |pane, cx| pane.select_all(cx));
                }
            }
            Command::ClearScrollback => {
                if let Some(block_id) = block {
                    self.call(Method::TerminalClear { block_id });
                }
            }
            Command::ResetTerminal => {
                if let Some(block_id) = block {
                    self.call(Method::TerminalReset { block_id });
                }
            }
            Command::RestartServer if self.incompatible().is_some() => self.upgrade_server(cx),
            Command::RestartServer => {
                self.status = Some(Status::progress("Restarting server…"));
                self.call(Method::ServerShutdown { force: false });
            }
            Command::StopServer => {
                if let Some(host) = self.hosts.get(self.active_host) {
                    host.paused.store(true, Ordering::Release);
                }
                self.call(Method::ServerShutdown { force: false });
                // Stays off, at login too, until Start Server or the next app launch.
                if let Some(service) = self.local_service().filter(|s| s.manager == silo_client::service::Manager::Launchd) {
                    self.rt.spawn_blocking(move || {
                        use silo_client::service::Action;
                        let stopped = service.apply(&silo_client::service::System, &[Action::Disable, Action::Bootout]);
                        if let Err(error) = stopped {
                            tracing::warn!("stop service: {error:#}");
                        }
                    });
                }
                self.detach_view();
                self.focus.focus(window, cx);
                self.status = Some(Status::warning(crate::workspace::SERVER_STOPPED));
            }
            Command::StartServer => {
                if let Some(host) = self.hosts.get(self.active_host) {
                    host.paused.store(false, Ordering::Release);
                }
                self.status = Some(Status::progress("Starting server…"));
            }
            Command::ShowServerLog => {
                let log = silo_proto::paths::log(&self.home);
                let opened = std::process::Command::new("/usr/bin/open").arg("-a").arg("Console").arg(&log).status();
                if !opened.is_ok_and(|s| s.success()) {
                    let _ = std::process::Command::new("/usr/bin/open").arg(&log).status();
                }
            }
            Command::ReloadTerminalSettings => self.reload_terminal_settings(window, cx),
            Command::ToggleFullScreen => window.toggle_fullscreen(),
            Command::OpenSettings => self.open_settings(window, cx),
            Command::AppearanceComfortable => self.set_density(crate::hosts::Density::Comfortable, cx),
            Command::AppearanceCompact => self.set_density(crate::hosts::Density::Compact, cx),
            Command::ShowPaneHeaders => self.set_pane_headers(true, cx),
            Command::HidePaneHeaders => self.set_pane_headers(false, cx),
            Command::OpenTerminalSettingsFile => self.open_terminal_settings_file(cx),
            Command::CheckForUpdates => {
                let me = cx.entity_id();
                self.updater.update(cx, |updater, cx| updater.check(Some(me), cx));
            }
            Command::CommandPalette => self.toggle_palette(window, cx),
            Command::HideSilo => cx.hide(),
            Command::HideOthers => cx.hide_other_apps(),
            Command::ShowAll => cx.unhide_other_apps(),
            Command::Quit => cx.quit(),
        }
        cx.notify();
    }

    /// Whether a command applies right now (hidden from the palette if not).
    pub(crate) fn available(&self, command: Command, cx: &gpui::App) -> bool {
        let host = self.hosts.get(self.active_host);
        let connected = host.is_some_and(|h| h.control.is_some());
        let paused = host.is_some_and(|h| h.paused.load(Ordering::Acquire));
        // Remote servers are never started, stopped or upgraded from here.
        let local = host.is_some_and(|h| h.info.local);
        let tabs = self.view.as_ref().map_or(0, |v| v.session.windows.len());
        let panes = self.active_window().map_or(0, |w| w.layout.blocks().len());
        if command.feature().is_some_and(|f| !self.host_supports(f)) {
            return false;
        }
        match command {
            Command::StartServer => local && paused,
            Command::StopServer => local && connected,
            Command::RestartServer => local && (connected || self.incompatible().is_some()),
            Command::NextTab | Command::PreviousTab => tabs > 1,
            Command::NextSession | Command::PreviousSession => host.is_some_and(|h| h.sessions.len() > 1),
            Command::FocusNextPane
            | Command::FocusPreviousPane
            | Command::FocusPaneLeft
            | Command::FocusPaneRight
            | Command::FocusPaneUp
            | Command::FocusPaneDown
            | Command::GrowPane
            | Command::ShrinkPane
            | Command::MovePaneEdgeLeft
            | Command::MovePaneEdgeRight
            | Command::MovePaneEdgeUp
            | Command::MovePaneEdgeDown
            | Command::EqualizePanes
            | Command::ZoomPane => panes > 1,
            Command::RemoveHost => self.hosts.iter().any(|h| !h.info.local && !h.removed),
            Command::RenameHost => host.is_some_and(|h| !h.info.local && !h.removed),
            Command::ShareSession | Command::SharePane => connected && self.view.is_some() && !host.is_some_and(|h| h.info.transport.is_shared()),
            // On the server's focused block: tiled with a sibling, or floating.
            Command::FloatPane => connected && panes > 1 && self.active_window().is_some_and(|w| w.layout.contains(&w.focused)),
            Command::DockPane => connected && self.active_window().is_some_and(|w| crate::floats::is_float(w, &w.focused)),
            Command::CommandPalette => false,
            Command::CheckForUpdates => self.updater.read(cx).enabled(),
            Command::NewSession => connected,
            Command::RenameSession
            | Command::CloseSession
            | Command::NewTab
            | Command::NewTerminalTab
            | Command::NewBrowserTab
            | Command::NewTabChooser
            | Command::CloseTab
            | Command::RenameTab
            | Command::SplitRight
            | Command::SplitDown
            | Command::NewPane
            | Command::NewBrowser
            | Command::OpenUrl
            | Command::ClosePane
            | Command::ClearScrollback
            | Command::ResetTerminal => connected && self.view.is_some(),
            _ => true,
        }
    }

    /// A status that clears itself after a few seconds.
    pub(crate) fn flash(&mut self, status: Status, cx: &mut Context<Self>) {
        self.flash_for(status, std::time::Duration::from_secs(4), cx);
    }

    /// A status that clears itself after `shown`.
    pub(crate) fn flash_for(&mut self, status: Status, shown: std::time::Duration, cx: &mut Context<Self>) {
        self.status = Some(status.clone());
        self.flashing = Some(status.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(shown).await;
            let _ = this.update(cx, |this, cx| {
                if this.flashing.as_ref() == Some(&status) {
                    this.flashing = None;
                }
                if this.status.as_ref() == Some(&status) {
                    this.status = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A status kept (like a flash) until the next one replaces it: work
    /// under way that will report how it went.
    pub(crate) fn hold(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = Some(status.clone());
        self.flashing = Some(status);
        cx.notify();
    }

    /// Clear the status line, except a flash still showing.
    pub(crate) fn clear_status(&mut self) {
        if self.status != self.flashing {
            self.status = None;
        }
    }

    fn step_tab(&mut self, step: isize) {
        let Some(view) = &self.view else { return };
        let count = view.session.windows.len();
        if count < 2 {
            return;
        }
        let current = view.session.windows.iter().position(|w| w.id == view.session.active_window).unwrap_or(0);
        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
        let next = (current as isize + step).rem_euclid(count as isize) as usize;
        self.select_window(next);
    }

    pub(crate) fn step_session(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.active_host;
        let sessions = &self.hosts[host].sessions;
        if sessions.len() < 2 {
            return;
        }
        let current = self.view.as_ref().and_then(|v| sessions.iter().position(|s| s.id == v.session.id)).unwrap_or(0);
        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
        let next = (current as isize + step).rem_euclid(sessions.len() as isize) as usize;
        let id = sessions[next].id.clone();
        self.open_session(host, id, window, cx);
    }

    /// Focus the geometric neighbour of the focused pane (⌃⌘H/J/K/L, ⌥⌘arrows).
    /// No neighbour, or an open overlay, is a no-op.
    fn focus_direction(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.is_some() || self.palette.is_some() || self.add_host.is_some() || self.picker.is_some() {
            return;
        }
        let Some(current) = self.focused_block(window, cx) else { return };
        let rect = |b: gpui::Bounds<gpui::Pixels>| (f32::from(b.left()), f32::from(b.top()), f32::from(b.right()), f32::from(b.bottom()));
        let Some(from) = self.block_bounds(&current, cx).map(rect) else { return };
        let Some(layout) = self.active_window().map(|w| w.layout.clone()) else { return };
        let mut ids = Vec::new();
        let mut rects = Vec::new();
        for id in layout.blocks() {
            if id == current {
                continue;
            }
            if let Some(r) = self.block_bounds(&id, cx) {
                ids.push(id);
                rects.push(rect(r));
            }
        }
        if let Some(index) = layout_ops::neighbour(from, &rects, direction) {
            let id = ids[index].clone();
            self.focus_block(&id, window, cx);
        }
    }

    /// Resize splits of the active, unzoomed window around the focused pane.
    fn resize_focused(
        &mut self,
        changes: impl FnOnce(&silo_proto::model::Layout, &str) -> Vec<layout_ops::RatioChange>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(focused) = self.focused_block(window, cx) else { return };
        let Some(w) = self.active_window().filter(|w| !w.zoomed) else { return };
        let window_id = w.id.clone();
        let changes = changes(&w.layout, &focused);
        self.set_ratios(&window_id, changes, cx);
    }

    /// Apply split ratios locally at once; the server gets them once
    /// resizing pauses, so held keys resize the PTYs only at the end.
    pub(crate) fn set_ratios(&mut self, window_id: &str, changes: Vec<layout_ops::RatioChange>, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else { return };
        let Some(w) = view.session.windows.iter_mut().find(|w| w.id == window_id) else { return };
        for (path, ratio) in changes {
            if w.layout.set_ratio(&path, ratio) {
                self.pending_ratios.retain(|p| !(p.window_id == window_id && p.path == path));
                self.pending_ratios.push(PendingRatio { window_id: window_id.to_owned(), path, ratio, shape: w.layout.clone() });
            }
        }
        self.ratio_flush = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RATIO_FLUSH).await;
            let _ = this.update(cx, |this, cx| this.flush_ratios(cx));
        }));
        cx.notify();
    }

    pub(crate) fn flush_ratios(&mut self, cx: &mut Context<Self>) {
        self.ratio_flush = None;
        let windows = self.view.as_ref().map(|v| v.session.windows.as_slice()).unwrap_or_default();
        let pending: Vec<_> = std::mem::take(&mut self.pending_ratios).into_iter().filter(|p| p.fits(windows)).collect();
        for PendingRatio { window_id, path, ratio, .. } in pending {
            self.call_optimistic(Method::LayoutRatio { window_id, path, ratio }, "Couldn't resize panes", |_| {}, cx);
        }
    }

    /// A server snapshot with the not yet sent ratios applied on top; those
    /// whose window changed shape (split, close) are dropped.
    pub(crate) fn with_pending_ratios(&mut self, mut view: silo_proto::model::SessionView) -> silo_proto::model::SessionView {
        self.pending_ratios.retain(|p| p.fits(&view.session.windows));
        for p in &self.pending_ratios {
            if let Some(w) = view.session.windows.iter_mut().find(|w| w.id == p.window_id) {
                w.layout.set_ratio(&p.path, p.ratio);
            }
        }
        view
    }

    // ---- palette UI ----

    /// Open the palette searching for `query`.
    pub(crate) fn open_palette_with(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette_step(Step::Search, query, window, cx);
    }

    pub(crate) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
        } else {
            self.open_palette_step(Step::Search, String::new(), window, cx);
        }
    }

    /// Ask for a new name of remote host `index`, starting from the current
    /// one, selected.
    pub(crate) fn open_rename_host(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.hosts.get(index).map(|h| h.info.name.clone()).unwrap_or_default();
        self.open_palette_step(Step::Input(Input::HostName(index, name.clone())), name, window, cx);
        if let Some(field) = self.palette.as_ref().map(|p| p.field.clone()) {
            field.update(cx, |f, cx| f.select_all(cx));
        }
    }

    fn open_palette_step(&mut self, step: Step, text: String, window: &mut Window, cx: &mut Context<Self>) {
        // Leaving a theme list (for any other step) puts the theme back.
        self.cancel_preview(window, cx);
        self.close_switcher(window, cx);
        // One modal at a time; the palette replaces the settings panel.
        self.settings = None;
        let snapshot = matches!(step, Step::Search | Step::Choice(..) | Step::Number(_)).then(|| self.settings_snapshot(None, cx));
        if self.palette.is_none() {
            let field = cx.new(|cx| TextField::new(finder::PLACEHOLDER, cx));
            let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| this.palette_event(event, window, cx));
            self.palette = Some(Palette {
                field,
                query: String::new(),
                filter: String::new(),
                selected: 0,
                step: Step::Search,
                scroll: ScrollHandle::new(),
                error: None,
                note: None,
                snapshot: SettingsSnapshot::default(),
                recent: finder::load_recent(&self.home),
                _sub: sub,
            });
        }
        let Some(palette) = self.palette.as_mut() else { return };
        if let Some(snapshot) = snapshot {
            palette.snapshot = snapshot;
        }
        palette.selected = match &step {
            // Start on the current value.
            Step::Choice(id, _) => id.choices(&palette.snapshot).iter().position(|c| c.selected).unwrap_or(0),
            _ => 0,
        };
        palette.step = step.clone();
        palette.error = None;
        palette.note = None;
        palette.filter.clear();
        let placeholder = match &step {
            Step::Search => finder::PLACEHOLDER.to_owned(),
            Step::Input(input) => input.placeholder(),
            Step::Confirm(_) => "↩ confirm · esc back".into(),
            Step::RemoveHost => "Choose a host to remove".into(),
            Step::Choice(..) => "Filter".into(),
            Step::Number(id) => id.number(&palette.snapshot).unwrap_or_default(),
        };
        let restore = if matches!(step, Step::Search) { palette.query.clone() } else { text };
        let field = palette.field.clone();
        let selected = palette.selected;
        let row = self.palette_row(selected, cx);
        if let Some(palette) = self.palette.as_ref() {
            palette.scroll.scroll_to_item(row);
        }
        field.update(cx, |f, cx| {
            f.set_placeholder(placeholder, cx);
            f.replace_text(restore, cx);
        });
        let focus = field.read(cx).focus.clone();
        focus.focus(window, cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_preview(window, cx);
        if let Some(palette) = self.palette.take() {
            drop(palette);
            self.refocus_terminal(window, cx);
            cx.notify();
        }
    }

    /// Put back the theme a choice list is previewing, if any, and end
    /// the preview. Re-reported to the servers: a pane that attached
    /// meanwhile reported the previewed one (the servers ignore a config
    /// they already have).
    fn cancel_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(original) = self.palette.as_mut().and_then(|p| p.step.end_preview()) {
            theme::install(original, theme::load_problem());
            self.apply_terminal_theme(crate::settings::Reach::Servers, window, cx);
        }
    }

    /// The window is gone: put back a theme it was previewing.
    pub(crate) fn release_preview(&mut self, cx: &mut gpui::App) {
        if let Some(original) = self.palette.as_mut().and_then(|p| p.step.end_preview()) {
            crate::settings::restore_terminal_theme(original, cx);
        }
    }

    fn entries(&self, cx: &gpui::App) -> Vec<Entry> {
        let Some(palette) = &self.palette else { return Vec::new() };
        match &palette.step {
            Step::Search => self.finder_items(&palette.query, &palette.snapshot, &palette.recent, cx).into_iter().map(Entry::Item).collect(),
            Step::Choice(id, _) => id
                .choices(&palette.snapshot)
                .into_iter()
                .filter_map(|choice| {
                    let (_, ranges) = fuzzy(&palette.filter, &choice.label)?;
                    Some(Entry::Choice { choice, ranges })
                })
                .collect(),
            Step::Input(_) | Step::Number(_) => Vec::new(),
            Step::Confirm(command) => vec![Entry::Confirm(*command), Entry::Back],
            Step::RemoveHost => {
                self.hosts.iter().enumerate().filter(|(_, h)| !h.info.local && !h.removed).map(|(i, _)| Entry::RemoveHost(i)).collect()
            }
        }
    }

    /// The list row of entry `selected` (search results have group labels).
    fn palette_row(&self, selected: usize, cx: &gpui::App) -> usize {
        if !self.palette.as_ref().is_some_and(|p| matches!(p.step, Step::Search)) {
            return selected;
        }
        let groups: Vec<finder::Group> = self.entries(cx).iter().filter_map(|e| if let Entry::Item(i) = e { Some(i.group) } else { None }).collect();
        finder::row_index(&groups, selected)
    }

    fn select_entry(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.palette_row(index, cx);
        let Some(palette) = self.palette.as_mut() else { return };
        palette.selected = index;
        palette.scroll.scroll_to_item(row);
        self.preview_selected(window, cx);
        cx.notify();
    }

    /// While choosing a theme, show the selected one everywhere.
    fn preview_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.entries(cx);
        let Some(palette) = self.palette.as_mut() else { return };
        let Step::Choice(_, Some(preview)) = &mut palette.step else { return };
        let Some(Entry::Choice { choice: Choice { event: SettingsEvent::SetTheme(id), .. }, .. }) = entries.get(palette.selected) else { return };
        let mut prefs = theme::prefs();
        prefs.theme.clone_from(id);
        if preview.show(prefs.clone()) {
            self.preview_terminal_prefs(prefs, window, cx);
        }
    }

    fn palette_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.entries(cx).len();
        let Some(palette) = self.palette.as_mut() else { return };
        match event {
            FieldEvent::Changed(text) => {
                palette.error = None;
                palette.note = None;
                match palette.step {
                    Step::Search if palette.query != *text => {
                        palette.query = text.clone();
                        self.select_entry(0, window, cx);
                    }
                    Step::Choice(..) if palette.filter != *text => {
                        palette.filter = text.clone();
                        self.select_entry(0, window, cx);
                    }
                    _ => {}
                }
            }
            FieldEvent::Up | FieldEvent::Down if count > 0 => {
                let delta = if matches!(event, FieldEvent::Up) { count - 1 } else { 1 };
                let next = (palette.selected + delta) % count;
                self.select_entry(next, window, cx);
            }
            FieldEvent::Up | FieldEvent::Down => {}
            FieldEvent::Cancel => {
                match &palette.step {
                    // A host rename is its own errand, from ⌘P or Settings.
                    Step::Search | Step::Input(Input::HostName(..)) => self.close_palette(window, cx),
                    _ => self.open_palette_step(Step::Search, String::new(), window, cx),
                }
                return;
            }
            FieldEvent::Submit => {
                let step = palette.step.clone();
                let text = palette.field.read(cx).text.trim().to_owned();
                match step {
                    Step::Input(input) => self.submit_input(input, text, window, cx),
                    Step::Number(id) => match id.parse(&text) {
                        Ok(event) => self.apply_setting(id, &event, window, cx),
                        Err(error) => {
                            palette.error = Some(error);
                            cx.notify();
                        }
                    },
                    _ => {
                        let selected = palette.selected;
                        if let Some(entry) = self.entries(cx).get(selected).cloned() {
                            self.activate_entry(entry, window, cx);
                        }
                    }
                }
                return;
            }
        }
        cx.notify();
    }

    /// Tab on a choice setting opens its values.
    fn palette_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(palette) = &self.palette else { return false };
        if !matches!(palette.step, Step::Search) {
            return false;
        }
        match self.entries(cx).get(palette.selected) {
            Some(Entry::Item(item)) if matches!(item.target, Target::Setting(id) if id.setting().kind == Kind::Choice) => {
                let item = item.clone();
                self.pick(item, window, cx);
                true
            }
            _ => false,
        }
    }

    fn submit_input(&mut self, input: Input, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let fail = |this: &mut Self, error: String, cx: &mut Context<Self>| {
            if let Some(p) = this.palette.as_mut() {
                p.error = Some(error);
            }
            cx.notify();
        };
        match input {
            // Empty keeps the current name.
            Input::RenameSession(id, _) => {
                if !text.is_empty() {
                    self.rename_session(id, text);
                }
                self.close_palette(window, cx);
            }
            Input::RenameTab(window_id, _) => {
                self.call(Method::WindowRename { window_id, name: (!text.is_empty()).then_some(text) });
                self.close_palette(window, cx);
            }
            Input::OpenUrl(block) => {
                let Some(url) = silo_proto::model::browser_url(&text) else {
                    return fail(self, "Enter an address.".into(), cx);
                };
                self.split_browser(block, Some(url));
                self.close_palette(window, cx);
            }
            Input::HostName(..) if text.is_empty() => self.close_palette(window, cx),
            Input::HostName(index, _) => match self.rename_host(index, text, cx) {
                Ok(()) => {
                    self.close_palette(window, cx);
                    self.refresh_settings(None, cx);
                }
                Err(error) => fail(self, error, cx),
            },
        }
    }

    /// Remember a finder pick for the empty query and the recency boost.
    fn remember_pick(&mut self, key: &str) {
        if key.is_empty() {
            return;
        }
        let Some(palette) = self.palette.as_mut() else { return };
        finder::remember(&mut palette.recent, key);
        finder::save_recent(&self.home, &palette.recent);
    }

    /// Apply a setting's change from a sub-step, then close.
    fn apply_setting(&mut self, id: SettingId, event: &SettingsEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_pick(&finder::setting_key(id));
        // Kept: closing must not put a previewed theme back (the change
        // below reports the kept one to the servers).
        if let Some(palette) = self.palette.as_mut() {
            drop(palette.step.end_preview());
        }
        let status = self.apply_settings_event(event, window, cx).and_then(crate::settings::Outcome::status);
        self.close_palette(window, cx);
        if let Some(status) = status {
            self.flash(status, cx);
        }
    }

    fn pick(&mut self, item: finder::Item, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(item.target, Target::OpenUrl(_)) {
            self.remember_pick(&item.key);
        }
        match item.target {
            Target::Command(command) => self.run(command, window, cx),
            Target::Setting(id) => match id.setting().kind {
                Kind::Toggle => {
                    let on = self.palette.as_ref().and_then(|p| id.on(&p.snapshot));
                    if let Some((on, event)) = on.and_then(|on| Some((!on, id.toggled(!on)?))) {
                        let outcome = self.apply_settings_event(&event, window, cx).unwrap_or_default();
                        if let Some(palette) = self.palette.as_mut() {
                            // The row's value shows the flip.
                            id.set_on(&mut palette.snapshot, on);
                            palette.note = outcome.note();
                            palette.error = outcome.error;
                        }
                    }
                }
                Kind::Choice => {
                    let preview = (id == SettingId::Theme).then(|| Box::new(Preview::new(theme::prefs())));
                    self.open_palette_step(Step::Choice(id, preview), String::new(), window, cx);
                }
                Kind::Number => self.open_palette_step(Step::Number(id), String::new(), window, cx),
                Kind::Open => {
                    self.close_palette(window, cx);
                    self.open_settings_at(id, window, cx);
                }
            },
            Target::Session { host, id } => {
                self.close_palette(window, cx);
                self.open_session(host, id, window, cx);
            }
            Target::Host(index) => {
                self.close_palette(window, cx);
                self.switch_host(index, window, cx);
            }
            Target::RenameHost(index) => self.open_rename_host(index, window, cx),
            Target::Upgrade(index) => {
                self.close_palette(window, cx);
                self.switch_host(index, window, cx);
                if self.hosts.get(index).is_some_and(|h| h.incompatible.is_some()) {
                    self.upgrade_server(cx);
                }
            }
            Target::Tab { index } => {
                self.close_palette(window, cx);
                self.select_window(index);
            }
            Target::Pane { index, block } => {
                self.close_palette(window, cx);
                let active = self.active_window().map(|w| w.id.clone());
                let target = self.view.as_ref().and_then(|v| v.session.windows.get(index)).map(|w| w.id.clone());
                if target.is_some() && target == active {
                    self.focus_block(&block, window, cx);
                } else {
                    // The server's focus is followed once the tab shows.
                    self.call(Method::BlockFocus { block_id: block });
                    self.select_window(index);
                }
            }
            Target::Page(url) | Target::OpenUrl(url) => {
                self.close_palette(window, cx);
                self.open_page_tab(url);
            }
        }
        cx.notify();
    }

    /// A new tab holding a browser on `url`.
    pub(crate) fn open_page_tab(&mut self, url: String) {
        let Some(view) = &self.view else { return };
        let spawn = silo_proto::control::SpawnSpec { cwd: None, command: Vec::new(), size: None };
        self.call(Method::WindowCreate { session_id: view.session.id.clone(), spawn, kind: silo_proto::model::BlockKind::Browser, url: Some(url) });
    }

    fn activate_entry(&mut self, entry: Entry, window: &mut Window, cx: &mut Context<Self>) {
        match entry {
            Entry::Item(item) => self.pick(item, window, cx),
            Entry::Choice { choice, .. } => {
                if let Some(Step::Choice(id, _)) = self.palette.as_ref().map(|p| p.step.clone()) {
                    self.apply_setting(id, &choice.event, window, cx);
                }
            }
            Entry::Confirm(command) => self.execute(command, window, cx),
            Entry::Back => self.open_palette_step(Step::Search, String::new(), window, cx),
            Entry::RemoveHost(index) => match self.remove_host(index, cx) {
                Ok(name) => {
                    self.flash(Status::success(format!("Removed “{name}”")), cx);
                    self.close_palette(window, cx);
                }
                Err(error) => {
                    if let Some(p) = self.palette.as_mut() {
                        p.error = Some(error);
                    }
                }
            },
        }
        cx.notify();
    }

    fn entry_row(&self, entry: &Entry, index: usize, selected: bool, cx: &mut Context<Self>) -> AnyElement {
        let highlight = HighlightStyle { color: Some(rgb(theme::accent()).into()), font_weight: Some(FontWeight::BOLD), ..Default::default() };
        let styled = |text: String, ranges: &[std::ops::Range<usize>]| {
            StyledText::new(text).with_highlights(ranges.iter().map(|r| (r.clone(), highlight)).collect::<Vec<_>>())
        };
        let check = |el: gpui::Div, on: bool| el.when(on, |el| el.child(crate::assets::icon("check", 12.0, theme::accent())));
        let (title, subtitle, right, danger): (AnyElement, Option<String>, Option<Right>, bool) = match entry {
            Entry::Item(item) => (
                check(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(div().flex().child(item.lead).child(styled(item.text.clone(), &item.found.ranges))),
                    item.check,
                )
                .into_any_element(),
                item.detail.clone(),
                item.right.clone(),
                false,
            ),
            Entry::Choice { choice, ranges, .. } => (
                check(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .children(choice.swatch.map(|(bg, fg)| {
                            div()
                                .size(px(16.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .continuous(4.0)
                                .bg(rgb(bg))
                                .border_1()
                                .border_color(rgb(theme::border()))
                                .text_size(px(9.0))
                                .text_color(rgb(fg))
                                .child("A")
                        }))
                        .child(styled(choice.label.clone(), ranges)),
                    choice.selected,
                )
                .into_any_element(),
                None,
                None,
                false,
            ),
            Entry::Confirm(command) => {
                let label = match command {
                    Command::CloseSession => {
                        format!("Close session “{}”", self.view.as_ref().map_or("", |v| v.session.name.as_str()))
                    }
                    other => other.title().to_owned(),
                };
                (div().child(label).into_any_element(), self.subtitle(*command).map(str::to_owned), Some(Right::Shortcut("↩".into())), true)
            }
            Entry::Back => (div().child("Cancel").into_any_element(), None, Some(Right::Shortcut("esc".into())), false),
            Entry::RemoveHost(i) => {
                let h = &self.hosts[*i];
                (div().child(format!("Remove “{}”", h.info.name)).into_any_element(), None, None, true)
            }
        };
        let entry = entry.clone();
        div()
            .id(("palette-row", index))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .h(px(36.0))
            .mx(px(6.0))
            .px(px(10.0))
            .continuous(8.0)
            .when(selected, |el| el.bg(theme::row_fill()))
            .when(!selected, |el| el.hover(|s| s.bg(theme::hover_fill())))
            .cursor_pointer()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(8.0))
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(div().flex_none().text_color(rgb(if danger { theme::danger() } else { theme::foreground() })).child(title))
                    .children(subtitle.map(|s| div().min_w_0().text_ellipsis().text_size(px(11.5)).text_color(rgb(theme::muted())).child(s))),
            )
            .children(right.map(|right| {
                match right {
                    Right::Shortcut(keys) => div()
                        .flex_none()
                        .px(px(6.0))
                        .py(px(1.0))
                        .continuous(5.0)
                        .bg(theme::hover_fill())
                        .text_size(px(11.5))
                        .text_color(rgb(theme::muted()))
                        .child(keys),
                    Right::Value(value) => {
                        div().flex_none().max_w(px(200.0)).text_ellipsis().text_size(px(12.0)).text_color(rgb(theme::muted())).child(value)
                    }
                }
            }))
            .on_mouse_move(cx.listener(move |this, _, window, cx| {
                if this.palette.as_ref().is_some_and(|p| p.selected != index) {
                    if let Some(p) = this.palette.as_mut() {
                        p.selected = index;
                    }
                    this.preview_selected(window, cx);
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| this.activate_entry(entry.clone(), window, cx)))
            .into_any_element()
    }

    fn group_label(label: &'static str, first: bool) -> AnyElement {
        div()
            .flex_none()
            .px(px(16.0))
            .when(!first, |el| el.pt(px(8.0)))
            .pb(px(2.0))
            .text_size(px(11.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(theme::muted()))
            .child(label)
            .into_any_element()
    }

    pub(crate) fn render_palette(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let palette = self.palette.as_ref()?;
        let entries = self.entries(cx);
        let header = match &palette.step {
            Step::Search => None,
            Step::Input(input) => Some(input.prompt()),
            Step::Confirm(command) => Some(format!("{}?", command.title())),
            Step::RemoveHost => Some("Remove which host?".into()),
            Step::Choice(id, _) | Step::Number(id) => Some(id.setting().title.to_owned()),
        };
        let footer = match &palette.step {
            Step::Input(Input::HostName(..)) => Some("↩ rename · esc cancel"),
            Step::Input(_) | Step::Number(_) => Some("↩ apply · esc back"),
            Step::Choice(_, Some(_)) => Some("↩ keep · esc restore"),
            Step::Choice(_, None) => Some("↩ apply · esc back"),
            _ => None,
        };
        let input_step = matches!(palette.step, Step::Input(_) | Step::Number(_));
        let viewport = f32::from(window.viewport_size().height);
        let mut list = div()
            .id("palette-list")
            .flex()
            .flex_col()
            .py(px(4.0))
            .max_h(px((viewport - 200.0).clamp(120.0, 420.0)))
            .overflow_y_scroll()
            .track_scroll(&palette.scroll);
        let mut previous = None;
        for (index, entry) in entries.iter().enumerate() {
            if let Entry::Item(item) = entry
                && previous != Some(item.group)
            {
                list = list.child(Self::group_label(item.group.label(), previous.is_none()));
                previous = Some(item.group);
            }
            list = list.child(self.entry_row(entry, index, index == palette.selected, cx));
        }
        let empty = entries.is_empty() && !input_step;
        let card = div()
            .id("palette")
            .occlude()
            .w(px(580.0))
            .flex()
            .flex_col()
            .continuous(14.0)
            .bg(theme::overlay_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_xl()
            .overflow_hidden()
            .font_family(theme::ui_font())
            .text_size(px(13.5))
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let k = &event.keystroke;
                if k.key == "tab" && !k.modifiers.shift && this.palette_tab(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .children(header.map(|h| div().px(px(16.0)).pt(px(10.0)).text_size(px(11.5)).text_color(rgb(theme::muted())).child(h)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .h(px(46.0))
                    .px(px(16.0))
                    .text_size(px(15.0))
                    .when(!input_step || !entries.is_empty(), |el| el.border_b_1().border_color(rgb(theme::border())))
                    .child(crate::assets::icon(
                        if matches!(palette.step, Step::Search) { "settings-search" } else { "terminal" },
                        16.0,
                        theme::muted(),
                    ))
                    .child(palette.field.clone()),
            )
            .children(palette.error.clone().map(|e| div().px(px(16.0)).py(px(6.0)).text_size(px(12.0)).text_color(rgb(theme::danger())).child(e)))
            .children(palette.note.clone().map(|n| div().px(px(16.0)).py(px(6.0)).text_size(px(11.5)).text_color(rgb(theme::muted())).child(n)))
            .when(empty, |el| el.child(div().px(px(16.0)).py(px(12.0)).text_color(rgb(theme::muted())).child("No matches")))
            .when(!entries.is_empty(), |el| el.child(list))
            .children(footer.map(|f| {
                div()
                    .px(px(16.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(rgb(theme::border()))
                    .text_size(px(11.5))
                    .text_color(rgb(theme::muted()))
                    .child(f)
            }));
        Some(
            div()
                .id("palette-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(TITLEBAR_HEIGHT + 40.0))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.close_palette(window, cx)))
                .child(card)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_fields_show_the_current_name() {
        assert_eq!(Input::RenameSession("s1".into(), "work".into()).placeholder(), "work");
        assert_eq!(Input::RenameTab("w1".into(), "zsh".into()).placeholder(), "zsh");
        assert_eq!(Input::HostName(1, "mini".into()).placeholder(), "mini");
        assert_eq!(Input::OpenUrl("b1".into()).placeholder(), "Address");
    }

    #[test]
    fn leaving_a_theme_preview_puts_the_original_back_once() {
        let original = TerminalPrefs::default();
        let previewing = |shown: &str| {
            let mut preview = Preview::new(original.clone());
            preview.show(TerminalPrefs { theme: shown.into(), ..original.clone() });
            Step::Choice(SettingId::Theme, Some(Box::new(preview)))
        };
        // Escape, closing, another step (Confirm, Rename…) or the window
        // going all end the preview the same way.
        let mut step = previewing("silo-light");
        assert_eq!(step.end_preview(), Some(original.clone()));
        assert!(matches!(step, Step::Choice(SettingId::Theme, None)), "the preview is gone");
        assert_eq!(step.end_preview(), None, "put back once");
        // Back on the original: nothing to put back.
        assert_eq!(previewing(&original.theme).end_preview(), None);
        for mut other in [Step::Search, Step::Confirm(Command::CloseSession), Step::RemoveHost, Step::Number(SettingId::FontSize)] {
            assert_eq!(other.end_preview(), None);
        }
        assert_eq!(Step::Choice(SettingId::CursorShape, None).end_preview(), None);
    }
}
