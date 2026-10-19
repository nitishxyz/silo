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
    hosts::{HostState, Transport as HostTransport},
    settings_panel::{
        CARD_RADIUS, ServerAction, SettingsDevice, SettingsEvent, SettingsHost, SettingsPanel, SettingsServer, SettingsShortcut, SettingsSnapshot,
        TerminalSettings, UiFontSettings,
    },
    terminal_prefs::{self, TerminalPrefs},
    theme,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

/// The local server's paired devices and a pending invite.
#[derive(Default)]
pub(crate) struct Pairing {
    pub devices: Vec<silo_proto::control::PairedDevice>,
    pub invite: Option<String>,
}

/// "Last seen …" for a device (`last_seen` 0 = never).
fn seen(last_seen: u64, now: u64) -> String {
    if last_seen == 0 {
        return "Never connected".into();
    }
    match now.saturating_sub(last_seen) {
        s if s < 60 => "Last seen just now".into(),
        s if s < 3600 => format!("Last seen {} min ago", s / 60),
        s if s < 86_400 => format!("Last seen {} h ago", s / 3600),
        s => format!("Last seen {} d ago", s / 86_400),
    }
}

/// A server message for display: words that look like paths are dropped.
fn without_paths(reason: &str) -> String {
    reason.split_whitespace().filter(|w| !w.contains('/')).collect::<Vec<_>>().join(" ")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub struct SettingsModal {
    pub panel: Entity<SettingsPanel>,
    _sub: Subscription,
    /// Refreshes paired devices while the Server page is shown.
    _devices: gpui::Task<()>,
}

/// After a pairing call: `None` leaves the panel as is, `Some(message)`
/// refreshes it with an optional message.
type Refresh = Option<Option<String>>;

/// How often the paired devices list is re-read while visible.
const DEVICES_REFRESH: std::time::Duration = std::time::Duration::from_secs(10);

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
                address: h.info.transport.remote_id().unwrap_or_default(),
                status: host_status(&h.state, h.paused.load(std::sync::atomic::Ordering::Acquire)),
                removable: !h.info.local,
            })
            .collect();
        let shortcuts = Command::ALL.iter().filter_map(|c| Some(SettingsShortcut { label: c.title().to_owned(), shortcut: c.hint()? })).collect();
        let local = self.hosts.first();
        let paused = local.is_some_and(|h| h.paused.load(std::sync::atomic::Ordering::Acquire));
        let connected = local.and_then(|h| h.control.as_ref());
        let incompatible = local.and_then(|h| h.incompatible.as_ref());
        let server = SettingsServer {
            status: match (connected, paused, local.map(|h| &h.state)) {
                (Some(_), _, _) => "Running".into(),
                (None, _, _) if incompatible.is_some() => silo_client::DifferentBuild::LABEL.into(),
                (None, true, _) => "Stopped".into(),
                (None, false, Some(HostState::Unreachable(reason))) => {
                    format!("Unavailable: {reason}")
                }
                _ => "Connecting…".into(),
            },
            version: connected.map(|c| c.hello().server.version.clone()).or_else(|| incompatible.and_then(|s| s.version.clone())),
            has_log: silo_proto::paths::log(&self.home).exists(),
            can_start: paused,
            can_stop: connected.is_some(),
            can_restart: connected.is_some(),
            managed: local.is_some_and(
                |h| matches!(&h.info.transport, HostTransport::Local { service: Ok(s), .. } if s.manager == silo_client::service::Manager::Launchd),
            ),
            endpoint_id: connected.map(|c| c.hello().server.endpoint_id.clone()).filter(|id| !id.is_empty()),
            remote_error: connected.and_then(|c| c.hello().server.remote_error.as_deref()).map(without_paths),
            devices: self
                .pairing
                .devices
                .iter()
                .map(|d| SettingsDevice { id: d.id.clone(), name: d.name.clone(), seen: seen(d.last_seen, unix_now()) })
                .collect(),
            invite: self.pairing.invite.clone(),
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
        let ui_font = UiFontSettings { family: self.ui_font_family().to_owned(), choices: theme::ui_fonts(cx), missing: ui.missing };
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

    /// Run a pairing RPC on the local server, then apply its result.
    fn pairing_call<T: serde::de::DeserializeOwned + Send + 'static>(
        &mut self,
        method: silo_proto::control::Method,
        apply: impl FnOnce(&mut Self, anyhow::Result<T>) -> Refresh + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(control) = self.hosts.first().and_then(|h| h.control.clone()) else {
            return;
        };
        let task = self.rt.spawn(async move { control.call::<T>(method).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                // Unchanged: leave the panel (and any confirmation) alone.
                if let Some(message) = apply(this, result) {
                    this.refresh_settings(message, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        use silo_proto::control::{Method, PairedDevices};
        self.pairing_call(
            Method::PairList {},
            |this, result: anyhow::Result<PairedDevices>| {
                let devices = result.inspect_err(|e| tracing::debug!("pair.list: {e:#}")).ok()?.devices;
                if devices == this.pairing.devices {
                    return None;
                }
                this.pairing.devices = devices;
                Some(None)
            },
            cx,
        );
    }

    fn create_invite(&mut self, cx: &mut Context<Self>) {
        use silo_proto::control::{Method, PairInvite};
        self.pairing_call(
            Method::PairCreate { ttl_secs: 600, purpose: silo_proto::control::InvitePurpose::Device },
            |this, result: anyhow::Result<PairInvite>| match result {
                Ok(invite) => {
                    this.pairing.invite = Some(invite.ticket);
                    Some(None)
                }
                Err(error) if error.downcast_ref::<silo_client::Offline>().is_some() => Some(Some("Not online yet. Try again shortly.".into())),
                Err(error) => {
                    tracing::warn!("pair.create: {error:#}");
                    Some(Some("Could not create a ticket.".into()))
                }
            },
            cx,
        );
    }

    fn revoke_device(&mut self, id: String, cx: &mut Context<Self>) {
        let method = silo_proto::control::Method::PairRevoke { id: id.clone() };
        self.pairing_call(
            method,
            move |this, result: anyhow::Result<serde_json::Value>| match result {
                Ok(_) => {
                    this.pairing.devices.retain(|d| d.id != id);
                    Some(None)
                }
                Err(error) => {
                    tracing::warn!("pair.revoke: {error:#}");
                    Some(Some("Could not revoke the device.".into()))
                }
            },
            cx,
        );
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
            self.pairing.invite = None;
            self.refresh_devices(cx);
            let snapshot = self.settings_snapshot(None, cx);
            let panel = cx.new(|cx| SettingsPanel::new(snapshot, window, cx));
            let sub = cx.subscribe_in(&panel, window, |this, _, event: &SettingsEvent, window, cx| this.settings_event(event, window, cx));
            let devices = cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(DEVICES_REFRESH).await;
                    let alive = this.update(cx, |this, cx| {
                        if this.settings.as_ref().is_some_and(|m| m.panel.read(cx).showing_server()) {
                            this.refresh_devices(cx);
                        }
                    });
                    if alive.is_err() {
                        return;
                    }
                }
            });
            self.settings = Some(SettingsModal { panel, _sub: sub, _devices: devices });
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
        let save_problem = saved.err().map(|e| format!("Applied, but could not save terminal settings: {e}"));
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
            SettingsEvent::ResetTerminal => self
                .update_terminal_prefs(|p| *p = TerminalPrefs::default(), window, cx)
                .or_else(|| Some("Terminal settings restored to defaults.".into())),
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
            SettingsEvent::PairDevice => {
                self.create_invite(cx);
                None
            }
            SettingsEvent::RevokeDevice(id) => {
                self.revoke_device(id.clone(), cx);
                None
            }
            SettingsEvent::OpenServerLog => {
                self.execute(Command::ShowServerLog, window, cx);
                None
            }
            // The host sheet replaces the panel; it is a modal of its own.
            SettingsEvent::AddHost => {
                self.close_settings(window, cx);
                self.open_add_host(None, None, window, cx);
                return;
            }
            SettingsEvent::RenameHost(id) => {
                let Ok(index) = id.parse::<usize>() else {
                    return;
                };
                self.close_settings(window, cx);
                self.open_rename_host(index, window, cx);
                return;
            }
            SettingsEvent::RemoveHost(id) => match id.parse::<usize>().map_err(|e| e.to_string()).and_then(|i| self.remove_host(i, cx)) {
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
        HostState::NotPaired => "Not paired".into(),
        HostState::DifferentBuild => "Different server build".into(),
    }
}

/// Commands that still run while the settings panel is open; everything
/// else would act on the hidden terminal and is swallowed (⌘W closes the
/// panel instead).
pub(crate) fn allowed_while_open(command: Command) -> bool {
    matches!(command, Command::OpenSettings | Command::Quit | Command::ToggleFullScreen | Command::CycleWindows)
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
    fn remote_error_hides_paths() {
        assert_eq!(without_paths("bind iroh endpoint: /Users/x/Library/key: denied"), "bind iroh endpoint: denied");
        assert_eq!(without_paths("no network"), "no network");
    }

    #[test]
    fn last_seen_is_short() {
        assert_eq!(seen(0, 100), "Never connected");
        assert_eq!(seen(100, 130), "Last seen just now");
        assert_eq!(seen(100, 100 + 5 * 60), "Last seen 5 min ago");
        assert_eq!(seen(100, 100 + 3 * 3600), "Last seen 3 h ago");
        assert_eq!(seen(100, 100 + 2 * 86_400), "Last seen 2 d ago");
        assert_eq!(host_status(&HostState::NotPaired, false), "Not paired");
        assert_eq!(host_status(&HostState::DifferentBuild, false), "Different server build");
        assert_eq!(crate::switcher::state_label(&HostState::Unreachable("x".into()), false), Some("offline"));
        assert_eq!(crate::switcher::state_label(&HostState::NotPaired, false), Some("not paired"));
        assert_eq!(crate::switcher::state_label(&HostState::Connected, false), None);
    }

    #[test]
    fn every_bound_command_is_listed_as_a_shortcut() {
        let listed: Vec<String> = Command::ALL
            .iter()
            .filter_map(|c| Some(SettingsShortcut { label: c.title().to_owned(), shortcut: c.hint()? }))
            .map(|s| s.label)
            .collect();
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
