//! ⌘P command palette and the command dispatcher. Every command — from a
//! key binding, the menu or the palette — runs through [`Workspace::run`].

use std::sync::atomic::Ordering;

use gpui::{
    AnyElement, Context, Entity, FontWeight, HighlightStyle, IntoElement, ScrollHandle, StyledText, Subscription, Window, div, prelude::*, px, rgb,
};
use silo_proto::{control::Method, model::Axis};

use crate::theme::Continuous as _;
use crate::{
    commands::{Command, fuzzy},
    hosts::HostState,
    text_field::{FieldEvent, TextField},
    theme,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

pub struct Palette {
    field: Entity<TextField>,
    /// The search query, restored when an input step goes back.
    query: String,
    selected: usize,
    step: Step,
    scroll: ScrollHandle,
    error: Option<String>,
    _sub: Subscription,
}

#[derive(Clone, Debug)]
enum Step {
    Search,
    Input(Input),
    Confirm(Command),
    RemoveHost,
}

#[derive(Clone, Debug)]
enum Input {
    RenameSession(String),
    RenameTab(String),
    HostName(usize),
    OpenUrl(String),
}

impl Input {
    fn prompt(&self) -> String {
        match self {
            Self::RenameSession(_) => "Rename session".into(),
            Self::RenameTab(_) => "Rename tab (empty to reset)".into(),
            Self::HostName(_) => "Rename host".into(),
            Self::OpenUrl(_) => "Open in a new browser".into(),
        }
    }

    fn placeholder(&self) -> &'static str {
        match self {
            Self::RenameSession(_) | Self::RenameTab(_) => "New name",
            Self::HostName(_) => "Host name",
            Self::OpenUrl(_) => "Address",
        }
    }
}

#[derive(Clone, Debug)]
enum Entry {
    Command { command: Command, ranges: Vec<std::ops::Range<usize>> },
    Switch { host: usize, id: String, name: String, ranges: Vec<std::ops::Range<usize>> },
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
    fn subtitle(&self, command: Command) -> Option<&'static str> {
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
        let block = self.focused_block(window, cx);
        let server_command = matches!(command, Command::StartServer | Command::StopServer | Command::RestartServer);
        if server_command && !self.hosts.get(self.active_host).is_some_and(|h| h.info.local) {
            self.flash("Switch to This Mac to manage its server.".into(), cx);
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
                    self.open_palette_step(Step::Input(Input::RenameSession(id)), name, window, cx);
                }
            }
            Command::CloseSession => {
                if let Some(view) = &self.view {
                    self.call(Method::SessionClose { session_id: view.session.id.clone() });
                }
            }
            Command::SwitchSession => self.toggle_switcher(window, cx),
            Command::NextSession => self.step_session(1, window, cx),
            Command::PreviousSession => self.step_session(-1, window, cx),
            Command::NewTab => self.new_window(window, cx),
            Command::CloseTab => {
                if let Some(w) = self.active_window() {
                    self.call(Method::WindowClose { window_id: w.id.clone() });
                }
            }
            Command::RenameTab => {
                if let Some(w) = self.active_window().cloned() {
                    let current = w.name.clone().unwrap_or_else(|| self.window_title(&w, cx));
                    self.open_palette_step(Step::Input(Input::RenameTab(w.id.clone())), current, window, cx);
                }
            }
            Command::NextTab => self.step_tab(1),
            Command::PreviousTab => self.step_tab(-1),
            Command::SplitRight => self.split(Axis::Horizontal, window, cx),
            Command::SplitDown => self.split(Axis::Vertical, window, cx),
            Command::ClosePane => self.close_block(window, cx),
            Command::FocusNextPane => self.cycle_focus(1, window, cx),
            Command::FocusPreviousPane => self.cycle_focus(-1, window, cx),
            Command::FocusPaneLeft => self.focus_direction(crate::focus::Direction::Left, window, cx),
            Command::FocusPaneRight => self.focus_direction(crate::focus::Direction::Right, window, cx),
            Command::FocusPaneUp => self.focus_direction(crate::focus::Direction::Up, window, cx),
            Command::FocusPaneDown => self.focus_direction(crate::focus::Direction::Down, window, cx),
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
                    self.flash("No remote hosts to remove.".into(), cx);
                }
            }
            Command::Copy => {
                if let Some((browser, _)) = block.as_ref().and_then(|b| self.browsers.get(b)) {
                    browser.read(cx).forward_chord("c");
                } else if let Some(pane) = self.focused_pane(window, cx) {
                    pane.update(cx, |pane, cx| pane.copy(cx));
                }
            }
            Command::Paste => {
                if let Some((browser, _)) = block.as_ref().and_then(|b| self.browsers.get(b)) {
                    let text = cx.read_from_clipboard().and_then(|i| i.text());
                    browser.update(cx, |pane, _| pane.paste(text.as_deref()));
                } else if let Some(pane) = self.focused_pane(window, cx) {
                    pane.update(cx, |pane, cx| pane.paste(cx));
                }
            }
            Command::SelectAll => {
                self.close_palette(window, cx);
                if let Some((browser, _)) = block.as_ref().and_then(|b| self.browsers.get(b)) {
                    browser.read(cx).forward_chord("a");
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
                self.status = Some("Restarting server…".into());
                self.call(Method::ServerShutdown {});
            }
            Command::StopServer => {
                if let Some(host) = self.hosts.get(self.active_host) {
                    host.paused.store(true, Ordering::Release);
                }
                self.call(Method::ServerShutdown {});
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
                self.status = Some("Server stopped. Run “Start Server” from the command palette (⌘P).".into());
            }
            Command::StartServer => {
                if let Some(host) = self.hosts.get(self.active_host) {
                    host.paused.store(false, Ordering::Release);
                }
                self.status = Some("Starting server…".into());
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
            Command::CommandPalette => self.toggle_palette(window, cx),
            Command::Quit => cx.quit(),
        }
        cx.notify();
    }

    /// Whether a command applies right now (hidden from the palette if not).
    fn available(&self, command: Command) -> bool {
        let host = self.hosts.get(self.active_host);
        let connected = host.is_some_and(|h| h.control.is_some());
        let paused = host.is_some_and(|h| h.paused.load(Ordering::Acquire));
        // Remote servers are never started, stopped or upgraded from here.
        let local = host.is_some_and(|h| h.info.local);
        let tabs = self.view.as_ref().map_or(0, |v| v.session.windows.len());
        let panes = self.active_window().map_or(0, |w| w.layout.blocks().len());
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
            | Command::ZoomPane => panes > 1,
            Command::RemoveHost => self.hosts.iter().any(|h| !h.info.local && !h.removed),
            // On the server's focused block: tiled with a sibling, or floating.
            Command::FloatPane => connected && panes > 1 && self.active_window().is_some_and(|w| w.layout.contains(&w.focused)),
            Command::DockPane => connected && self.active_window().is_some_and(|w| crate::floats::is_float(w, &w.focused)),
            Command::CommandPalette => false,
            Command::NewSession => connected,
            Command::RenameSession
            | Command::CloseSession
            | Command::NewTab
            | Command::CloseTab
            | Command::RenameTab
            | Command::SplitRight
            | Command::SplitDown
            | Command::NewBrowser
            | Command::OpenUrl
            | Command::ClosePane
            | Command::ClearScrollback
            | Command::ResetTerminal => connected && self.view.is_some(),
            _ => true,
        }
    }

    /// A status message that clears itself after a few seconds.
    pub(crate) fn flash(&mut self, message: String, cx: &mut Context<Self>) {
        self.status = Some(message.clone());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
            let _ = this.update(cx, |this, cx| {
                if this.status.as_deref() == Some(message.as_str()) {
                    this.status = None;
                    cx.notify();
                }
            });
        })
        .detach();
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

    fn step_session(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
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

    /// Focus the geometric neighbour of the focused pane (⌘H/J/L, ⌥⌘arrows).
    /// No neighbour, or an open overlay, is a no-op.
    fn focus_direction(&mut self, direction: crate::focus::Direction, window: &mut Window, cx: &mut Context<Self>) {
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
        if let Some(index) = crate::focus::neighbour(from, &rects, direction) {
            let id = ids[index].clone();
            self.focus_block(&id, window, cx);
        }
    }

    // ---- palette UI ----

    pub(crate) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
        } else {
            self.open_palette_step(Step::Search, String::new(), window, cx);
        }
    }

    /// Ask for a new name of remote host `index`.
    pub(crate) fn open_rename_host(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.hosts.get(index).map(|h| h.info.name.clone()).unwrap_or_default();
        self.open_palette_step(Step::Input(Input::HostName(index)), name, window, cx);
    }

    fn open_palette_step(&mut self, step: Step, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.close_switcher(window, cx);
        // One modal at a time; the palette replaces the settings panel.
        self.settings = None;
        if self.palette.is_none() {
            let field = cx.new(|cx| TextField::new("Type a command…", cx));
            let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| this.palette_event(event, window, cx));
            self.palette =
                Some(Palette { field, query: String::new(), selected: 0, step: Step::Search, scroll: ScrollHandle::new(), error: None, _sub: sub });
        }
        let Some(palette) = self.palette.as_mut() else { return };
        palette.step = step.clone();
        palette.selected = 0;
        palette.error = None;
        let placeholder = match &step {
            Step::Search => "Type a command or session…".to_owned(),
            Step::Input(input) => input.placeholder().to_owned(),
            Step::Confirm(_) => "↩ confirm · esc back".into(),
            Step::RemoveHost => "Choose a host to remove".into(),
        };
        let restore = if matches!(step, Step::Search) { palette.query.clone() } else { text };
        let field = palette.field.clone();
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
        if self.palette.take().is_some() {
            self.refocus_terminal(window, cx);
            cx.notify();
        }
    }

    fn entries(&self) -> Vec<Entry> {
        let Some(palette) = &self.palette else { return Vec::new() };
        match &palette.step {
            Step::Search => {
                let query = palette.query.trim();
                let mut scored: Vec<(i32, usize, Entry)> = Command::ALL
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| self.available(**c))
                    .filter_map(|(i, c)| fuzzy(query, c.title()).map(|(score, ranges)| (score, i, Entry::Command { command: *c, ranges })))
                    .collect();
                if !query.is_empty() {
                    scored.sort_by_key(|(score, i, _)| (-score, *i));
                }
                let mut entries: Vec<Entry> = scored.into_iter().map(|(_, _, e)| e).collect();
                if !query.is_empty() {
                    let current = self.view.as_ref().map(|v| v.session.id.as_str());
                    let mut sessions: Vec<(i32, Entry)> = Vec::new();
                    for (host, entry) in self.hosts.iter().enumerate() {
                        if entry.removed || entry.state != HostState::Connected {
                            continue;
                        }
                        for s in &entry.sessions {
                            if host == self.active_host && Some(s.id.as_str()) == current {
                                continue;
                            }
                            if let Some((score, ranges)) = fuzzy(query, &s.name) {
                                sessions.push((score, Entry::Switch { host, id: s.id.clone(), name: s.name.clone(), ranges }));
                            }
                        }
                    }
                    sessions.sort_by_key(|(score, _)| -score);
                    entries.extend(sessions.into_iter().map(|(_, e)| e));
                }
                entries
            }
            Step::Input(_) => Vec::new(),
            Step::Confirm(command) => vec![Entry::Confirm(*command), Entry::Back],
            Step::RemoveHost => {
                self.hosts.iter().enumerate().filter(|(_, h)| !h.info.local && !h.removed).map(|(i, _)| Entry::RemoveHost(i)).collect()
            }
        }
    }

    fn palette_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.entries().len();
        let Some(palette) = self.palette.as_mut() else { return };
        match event {
            FieldEvent::Changed(text) => {
                if matches!(palette.step, Step::Search) {
                    palette.query = text.clone();
                    palette.selected = 0;
                    palette.scroll.scroll_to_item(0);
                }
                palette.error = None;
            }
            FieldEvent::Up | FieldEvent::Down if count > 0 => {
                let delta = if matches!(event, FieldEvent::Up) { count - 1 } else { 1 };
                palette.selected = (palette.selected + delta) % count;
                palette.scroll.scroll_to_item(palette.selected);
            }
            FieldEvent::Up | FieldEvent::Down => {}
            FieldEvent::Cancel => {
                if matches!(palette.step, Step::Search) {
                    self.close_palette(window, cx);
                } else {
                    self.open_palette_step(Step::Search, String::new(), window, cx);
                }
                return;
            }
            FieldEvent::Submit => {
                let step = palette.step.clone();
                let text = palette.field.read(cx).text.trim().to_owned();
                match step {
                    Step::Input(input) => self.submit_input(input, text, window, cx),
                    _ => {
                        let selected = palette.selected;
                        if let Some(entry) = self.entries().get(selected).cloned() {
                            self.activate_entry(entry, window, cx);
                        }
                    }
                }
                return;
            }
        }
        cx.notify();
    }

    fn submit_input(&mut self, input: Input, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let fail = |this: &mut Self, error: String, cx: &mut Context<Self>| {
            if let Some(p) = this.palette.as_mut() {
                p.error = Some(error);
            }
            cx.notify();
        };
        match input {
            Input::RenameSession(id) => {
                if text.is_empty() {
                    return fail(self, "A session needs a name.".into(), cx);
                }
                self.rename_session(id, text);
                self.close_palette(window, cx);
            }
            Input::RenameTab(window_id) => {
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
            Input::HostName(index) => match self.rename_host(index, text, cx) {
                Ok(()) => {
                    self.close_palette(window, cx);
                    self.refresh_settings(None, cx);
                }
                Err(error) => fail(self, error, cx),
            },
        }
    }

    fn activate_entry(&mut self, entry: Entry, window: &mut Window, cx: &mut Context<Self>) {
        match entry {
            Entry::Command { command, .. } => self.run(command, window, cx),
            Entry::Switch { host, id, .. } => {
                self.close_palette(window, cx);
                self.open_session(host, id, window, cx);
            }
            Entry::Confirm(command) => self.execute(command, window, cx),
            Entry::Back => self.open_palette_step(Step::Search, String::new(), window, cx),
            Entry::RemoveHost(index) => match self.remove_host(index, cx) {
                Ok(name) => {
                    self.flash(format!("Removed host “{name}”."), cx);
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
        let (title, subtitle, hint, danger): (AnyElement, Option<String>, Option<String>, bool) = match entry {
            Entry::Command { command, ranges } => (
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(styled(command.title().into(), ranges))
                    .when(self.checked(*command), |el| el.child(crate::assets::icon("check", 12.0, theme::accent())))
                    .into_any_element(),
                Some(self.subtitle(*command).map_or_else(|| command.group().to_owned(), |s| format!("{} · {s}", command.group()))),
                command.hint(),
                false,
            ),
            Entry::Switch { host, name, ranges, .. } => (
                div().flex().child("Switch to ").child(styled(name.clone(), ranges)).into_any_element(),
                Some(self.hosts[*host].info.name.clone()),
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
                (div().child(label).into_any_element(), self.subtitle(*command).map(str::to_owned), Some("↩".into()), true)
            }
            Entry::Back => (div().child("Cancel").into_any_element(), None, Some("esc".into()), false),
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
            .when(selected, |el| el.bg(rgb(theme::row_active())))
            .when(!selected, |el| el.hover(|s| s.bg(rgb(theme::tab_hover()))))
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
            .children(hint.map(|h| {
                div()
                    .flex_none()
                    .px(px(6.0))
                    .py(px(1.0))
                    .continuous(5.0)
                    .bg(rgb(theme::tab_hover()))
                    .text_size(px(11.5))
                    .text_color(rgb(theme::muted()))
                    .child(h)
            }))
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if let Some(p) = this.palette.as_mut()
                    && p.selected != index
                {
                    p.selected = index;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| this.activate_entry(entry.clone(), window, cx)))
            .into_any_element()
    }

    pub(crate) fn render_palette(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let palette = self.palette.as_ref()?;
        let entries = self.entries();
        let header = match &palette.step {
            Step::Search => None,
            Step::Input(input) => Some(input.prompt()),
            Step::Confirm(command) => Some(format!("{}?", command.title())),
            Step::RemoveHost => Some("Remove which host?".into()),
        };
        let input_step = matches!(palette.step, Step::Input(_));
        let viewport = f32::from(window.viewport_size().height);
        let mut list = div()
            .id("palette-list")
            .flex()
            .flex_col()
            .py(px(4.0))
            .max_h(px((viewport - 200.0).clamp(120.0, 420.0)))
            .overflow_y_scroll()
            .track_scroll(&palette.scroll);
        for (index, entry) in entries.iter().enumerate() {
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
            .bg(rgb(theme::popover()))
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_xl()
            .overflow_hidden()
            .font_family(theme::ui_font())
            .text_size(px(13.5))
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
                    .child(crate::assets::icon("terminal", 16.0, theme::muted()))
                    .child(palette.field.clone()),
            )
            .children(palette.error.clone().map(|e| div().px(px(16.0)).py(px(6.0)).text_size(px(12.0)).text_color(rgb(theme::danger())).child(e)))
            .when(input_step, |el| {
                el.child(div().px(px(16.0)).py(px(8.0)).text_size(px(11.5)).text_color(rgb(theme::muted())).child("↩ apply · esc back"))
            })
            .when(empty, |el| el.child(div().px(px(16.0)).py(px(12.0)).text_color(rgb(theme::muted())).child("No matching commands")))
            .when(!entries.is_empty(), |el| el.child(list));
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
