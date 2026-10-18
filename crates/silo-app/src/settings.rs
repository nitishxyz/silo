//! Owner side of the settings panel (`settings_panel.rs`): builds snapshots
//! from the real sources (app.json layout settings, Silo's terminal.json,
//! hosts, the server connection, the command registry), applies the panel's
//! events with the same code the palette uses, and hosts the panel in a
//! modal.

use gpui::{AnyElement, Context, Entity, IntoElement, Subscription, Window, div, prelude::*, px, rgb};

use crate::theme::Continuous as _;
use crate::{
    commands::Command,
    fonts,
    hosts::HostState,
    settings_panel::{
        CARD_RADIUS, ServerAction, SettingsEvent, SettingsHost, SettingsPanel, SettingsServer, SettingsShortcut,
        SettingsSnapshot, TerminalSettings, UiFontSettings,
    },
    terminal_prefs::{self, TerminalPrefs},
    theme,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

pub struct SettingsModal {
    pub panel: Entity<SettingsPanel>,
    _sub: Subscription,
}

impl Workspace {
    pub(crate) fn settings_snapshot(&self, message: Option<String>, cx: &gpui::App) -> SettingsSnapshot {
        let hosts = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, h)| !h.removed)
            .map(|(index, h)| SettingsHost {
                id: index.to_string(),
                name: h.info.name.clone(),
                address: match &h.info.transport {
                    crate::hosts::Transport::Iroh { address } => address.clone(),
                    // The local host needs no address in the UI.
                    crate::hosts::Transport::Local { .. } => String::new(),
                },
                status: host_status(&h.state, h.paused.load(std::sync::atomic::Ordering::Acquire)),
                removable: !h.info.local,
            })
            .collect();
        let shortcuts = Command::ALL
            .iter()
            .filter_map(|c| Some(SettingsShortcut { label: c.title().to_owned(), shortcut: c.hint()? }))
            .collect();
        let local = self.hosts.first();
        let paused = local.is_some_and(|h| h.paused.load(std::sync::atomic::Ordering::Acquire));
        let connected = local.and_then(|h| h.control.as_ref());
        let incompatible = local.and_then(|h| h.incompatible.as_ref());
        let server = SettingsServer {
            status: match (connected, paused, local.map(|h| &h.state)) {
                (Some(_), _, _) => "Running".into(),
                (None, _, _) if incompatible.is_some() => {
                    format!("{} server running", incompatible.map_or("Incompatible", |s| s.age()))
                }
                (None, true, _) => "Stopped".into(),
                (None, false, Some(HostState::Unreachable(reason))) => format!("Unavailable: {reason}"),
                _ => "Connecting…".into(),
            },
            version: connected
                .map(|c| c.hello().server.version.clone())
                .or_else(|| incompatible.and_then(|s| s.version.clone())),
            has_log: silo_proto::paths::log(&self.home).exists(),
            can_start: paused,
            can_stop: connected.is_some(),
            can_restart: connected.is_some() || incompatible.is_some(),
        };
        let (_, font_in_use, _) = theme::summary();
        let prefs = theme::prefs();
        let mut terminal = TerminalSettings::from_prefs(&prefs, font_in_use);
        let installed = theme::font_names(cx);
        let automatic = fonts::automatic(&installed);
        terminal.font_choices = theme::monospace_fonts(cx);
        terminal.font_automatic = automatic.family;
        terminal.font_automatic_nerd = automatic.nerd;
        terminal.font_missing = !prefs.font_family.is_empty() && fonts::find(&prefs.font_family, &installed).is_none();
        // A missing font is flagged on its own row.
        terminal.problems = theme::load_problem().into_iter().collect();
        let ui = theme::ui_font_state();
        let ui_font = UiFontSettings {
            family: self.ui_font_family().to_owned(),
            choices: theme::ui_fonts(cx),
            missing: ui.missing,
        };
        SettingsSnapshot {
            density: self.density(),
            show_pane_headers: self.pane_headers(),
            ui_font,
            terminal,
            hosts,
            shortcuts,
            server,
            version: env!("CARGO_PKG_VERSION").into(),
            revision: format!("Ghostty {}", short(silo_vt::GHOSTTY_REV)),
            revision_full: silo_vt::GHOSTTY_REV.to_owned(),
            message,
        }
    }

    /// ⌘, / "Settings…": open the panel, or focus it if already open.
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_switcher(window, cx);
        if self.palette.is_some() {
            self.close_palette(window, cx);
        }
        self.appearance_menu.open = false;
        if self.settings.is_none() {
            // Fonts may have been installed or removed since launch.
            theme::refresh_fonts(cx);
            theme::apply_font(cx);
            theme::apply_ui_font(self.ui_font_family(), cx);
            window.refresh();
            let snapshot = self.settings_snapshot(None, cx);
            let panel = cx.new(|cx| SettingsPanel::new(snapshot, window, cx));
            let sub = cx.subscribe_in(&panel, window, |this, _, event: &SettingsEvent, window, cx| this.settings_event(event, window, cx));
            self.settings = Some(SettingsModal { panel, _sub: sub });
        }
        if let Some(modal) = &self.settings {
            modal.panel.update(cx, |panel, cx| panel.focus_search(window, cx));
        }
        cx.notify();
    }

    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.take().is_some() {
            self.refocus_terminal(window, cx);
            cx.notify();
        }
    }

    /// Push fresh state into an open panel (after a change, or when hosts,
    /// the server or the theme changed underneath it).
    pub(crate) fn refresh_settings(&mut self, message: Option<String>, cx: &mut Context<Self>) {
        if let Some(modal) = &self.settings {
            let snapshot = self.settings_snapshot(message, cx);
            modal.panel.update(cx, |panel, cx| panel.refresh(snapshot, cx));
        }
    }

    /// Change the terminal preferences: validate, apply live (theme, font,
    /// every attached terminal), then save atomically. Returns a problem to
    /// show, if any; a failed save keeps the change for this run and says so.
    pub(crate) fn update_terminal_prefs(
        &mut self,
        change: impl FnOnce(&mut TerminalPrefs),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let mut prefs = theme::prefs();
        change(&mut prefs);
        let (prefs, problems) = prefs.validated();
        let saved = terminal_prefs::save(&self.home, &prefs);
        let save_problem = saved.err().map(|e| {
            format!("Applied, but could not save terminal settings: {e}")
        });
        theme::install(prefs, save_problem.clone());
        self.apply_terminal_theme(window, cx);
        let mut messages: Vec<String> = save_problem.into_iter().chain(problems).collect();
        if messages.is_empty() {
            return None;
        }
        messages.dedup();
        Some(messages.join(" "))
    }

    /// Install the current theme's font and colors everywhere.
    fn apply_terminal_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        theme::apply_font(cx);
        for (pane, _) in self.panes.values() {
            pane.update(cx, |pane, cx| pane.apply_theme(cx));
        }
        window.refresh();
    }

    /// Re-read terminal.json (after editing it by hand).
    pub(crate) fn reload_terminal_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let loaded = terminal_prefs::load(&self.home);
        let problem = loaded.problem.clone();
        theme::refresh_fonts(cx);
        theme::install(loaded.prefs, loaded.problem);
        theme::apply_ui_font(self.ui_font_family(), cx);
        self.apply_terminal_theme(window, cx);
        self.flash(problem.unwrap_or_else(|| "Terminal settings reloaded".into()), cx);
        self.refresh_settings(None, cx);
    }

    /// Open terminal.json in a text editor, writing the current settings
    /// first when the file does not exist yet.
    pub(crate) fn open_terminal_settings_file(&mut self, cx: &mut Context<Self>) {
        let path = terminal_prefs::path(&self.home);
        if !path.exists()
            && let Err(error) = terminal_prefs::save(&self.home, &theme::prefs())
        {
            self.flash(format!("Could not create the terminal settings file: {error}"), cx);
            return;
        }
        if let Err(error) = std::process::Command::new("/usr/bin/open").arg("-t").arg(&path).spawn() {
            self.flash(format!("Could not open the terminal settings file: {error}"), cx);
        }
    }

    fn settings_event(&mut self, event: &SettingsEvent, window: &mut Window, cx: &mut Context<Self>) {
        let message = match event {
            SettingsEvent::Close => {
                self.close_settings(window, cx);
                return;
            }
            SettingsEvent::SetDensity(density) => {
                self.set_density(*density, cx);
                None
            }
            SettingsEvent::SetPaneHeaders(show) => {
                self.set_pane_headers(*show, cx);
                None
            }
            SettingsEvent::SetUiFontFamily(family) => {
                self.set_ui_font_family(family.clone(), window, cx);
                None
            }
            SettingsEvent::SetTheme(id) => {
                let id = id.clone();
                self.update_terminal_prefs(|p| p.theme = id, window, cx)
            }
            SettingsEvent::SetFontFamily(family) => {
                let family = family.clone();
                self.update_terminal_prefs(|p| p.font_family = family, window, cx)
            }
            SettingsEvent::SetFontSize(size) => {
                let size = *size;
                self.update_terminal_prefs(|p| p.font_size = size, window, cx)
            }
            SettingsEvent::SetColor(slot, color) => {
                let (slot, color) = (*slot, *color);
                self.update_terminal_prefs(|p| p.colors.set(slot, color), window, cx)
            }
            SettingsEvent::SetCursorShape(shape) => {
                let shape = *shape;
                self.update_terminal_prefs(|p| p.cursor_shape = shape, window, cx)
            }
            SettingsEvent::SetCursorBlink(blink) => {
                let blink = *blink;
                self.update_terminal_prefs(|p| p.cursor_blink = blink, window, cx)
            }
            SettingsEvent::SetScrollback(lines) => {
                let lines = *lines;
                self.update_terminal_prefs(|p| p.scrollback_lines = lines, window, cx)
            }
            SettingsEvent::ResetTerminal => {
                self.update_terminal_prefs(|p| *p = TerminalPrefs::default(), window, cx)
                    .or_else(|| Some("Terminal settings restored to defaults.".into()))
            }
            SettingsEvent::ReloadTerminal => {
                self.execute(Command::ReloadTerminalSettings, window, cx);
                theme::problems().first().cloned().or_else(|| Some("Terminal settings reloaded.".into()))
            }
            SettingsEvent::RevealConfig => {
                let path = terminal_prefs::path(&self.home);
                if path.exists() || terminal_prefs::save(&self.home, &theme::prefs()).is_ok() {
                    cx.reveal_path(&path);
                    None
                } else {
                    Some("Could not create the terminal settings file.".into())
                }
            }
            SettingsEvent::OpenConfig => {
                self.execute(Command::OpenTerminalSettingsFile, window, cx);
                None
            }
            SettingsEvent::OpenServerLog => {
                self.execute(Command::ShowServerLog, window, cx);
                None
            }
            // The host sheet replaces the panel; it is a modal of its own.
            SettingsEvent::AddHost => {
                self.close_settings(window, cx);
                self.add_host = Some(crate::add_host::AddHostSheet::new(window, cx));
                cx.notify();
                return;
            }
            SettingsEvent::RemoveHost(id) => match id.parse::<usize>().map_err(|e| e.to_string()).and_then(|i| self.remove_host(i)) {
                Ok(name) => Some(format!("Removed host “{name}”.")),
                Err(error) => Some(error),
            },
            // The panel asked for confirmation already; run the commands
            // directly instead of through the palette's confirm step.
            SettingsEvent::Server(action) => {
                let command = match action {
                    ServerAction::Start => Command::StartServer,
                    ServerAction::Stop => Command::StopServer,
                    ServerAction::Restart => Command::RestartServer,
                };
                self.execute(command, window, cx);
                // Stop moves focus to the workspace; keep the panel usable.
                if let Some(modal) = &self.settings {
                    modal.panel.update(cx, |panel, cx| panel.focus_search(window, cx));
                }
                self.status.clone()
            }
        };
        self.refresh_settings(message, cx);
        cx.notify();
    }

    pub(crate) fn render_settings(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let modal = self.settings.as_ref()?;
        let viewport = window.viewport_size();
        // Keep in sync with the width the panel assumes for its own layout.
        let width = (f32::from(viewport.width) - 64.0).clamp(420.0, 880.0);
        let height = (f32::from(viewport.height) - TITLEBAR_HEIGHT - 44.0).clamp(300.0, 640.0);
        Some(
            div()
                .id("settings-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(TITLEBAR_HEIGHT + 12.0))
                .bg(rgb(0x0000_0000).opacity(0.25))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.close_settings(window, cx)))
                .child(
                    div()
                        .id("settings-card")
                        .occlude()
                        .w(px(width))
                        .h(px(height))
                        .continuous(CARD_RADIUS)
                        // Clips to the bounds inside the border; the panel
                        // rounds its own fills to the inner radius.
                        .overflow_hidden()
                        .bg(rgb(theme::background()))
                        .border_1()
                        .border_color(rgb(theme::border()))
                        .shadow_xl()
                        // Clicks inside must not reach the backdrop.
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(modal.panel.clone()),
                )
                .into_any_element(),
        )
    }
}

fn host_status(state: &HostState, paused: bool) -> String {
    match state {
        _ if paused => "Stopped".into(),
        HostState::Connecting => "Connecting…".into(),
        HostState::Connected => "Connected".into(),
        HostState::Unreachable(reason) => format!("Unreachable: {reason}"),
    }
}

/// Commands that still run while the settings panel is open; everything
/// else would act on the hidden terminal and is swallowed (⌘W closes the
/// panel instead).
pub(crate) fn allowed_while_open(command: Command) -> bool {
    matches!(command, Command::OpenSettings | Command::Quit | Command::ToggleFullScreen)
}

fn short(rev: &str) -> &str {
    rev.get(..8).unwrap_or(rev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_status_and_revision_text() {
        assert_eq!(host_status(&HostState::Connected, false), "Connected");
        assert_eq!(host_status(&HostState::Connected, true), "Stopped");
        assert_eq!(host_status(&HostState::Unreachable("refused".into()), false), "Unreachable: refused");
        assert_eq!(short("d2865b8f578fa4e85e2f2b85ef97f5ef69de951f"), "d2865b8f");
        assert_eq!(short("abc"), "abc");
    }

    #[test]
    fn every_bound_command_is_listed_as_a_shortcut() {
        let listed: Vec<String> =
            Command::ALL.iter().filter_map(|c| Some(SettingsShortcut { label: c.title().to_owned(), shortcut: c.hint()? })).map(|s| s.label).collect();
        assert!(listed.contains(&"Settings…".to_owned()));
        assert!(listed.contains(&"Split Right".to_owned()));
        assert!(!listed.contains(&"Rename Session…".to_owned()), "unbound commands have no shortcut row");
    }

    #[test]
    fn terminal_commands_are_blocked_while_settings_are_open() {
        for command in [Command::SplitRight, Command::SplitDown, Command::ClosePane, Command::NewTab, Command::Paste] {
            assert!(!allowed_while_open(command), "{command:?} must not reach the terminal");
        }
        assert!(allowed_while_open(Command::OpenSettings));
        assert!(allowed_while_open(Command::Quit));
    }
}
