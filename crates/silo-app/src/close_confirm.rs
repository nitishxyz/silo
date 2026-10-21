//! Confirm before closing a busy block (`docs/PHASE7.md` §7.5). Closing a
//! pane, a tab or a session, and stopping or restarting the server, first
//! asks the server what would end (`blocks.busy`); if anything runs, one
//! dialog lists it with Cancel (the default) and Close (⌘⌫). Confirmed
//! closes are sent with `force: true`, the rest with `force: false`: when
//! a program started in between, the server refuses with `busy` and the
//! dialog opens from its list. The check is this app's request, so the
//! dialog shows only here.

use gpui::{AnyElement, Context, FocusHandle, FontWeight, IntoElement, KeyDownEvent, Window, div, prelude::*, px, rgb};
use silo_proto::control::{BusyBlock, BusyBlocks, Method, RpcError};

use crate::theme::Continuous as _;
use crate::{
    commands::Command,
    status::Status,
    theme,
    tooltip::Tooltip,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

/// Rows listed before "and N more".
const MAX_ROWS: usize = 8;

/// What a close ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CloseTarget {
    Pane(String),
    Tab(String),
    Session { id: String, name: String },
    StopServer,
    RestartServer,
}

impl CloseTarget {
    /// The close itself.
    fn method(&self, force: bool) -> Method {
        match self {
            Self::Pane(block_id) => Method::BlockClose { block_id: block_id.clone(), force },
            Self::Tab(window_id) => Method::WindowClose { window_id: window_id.clone(), force },
            Self::Session { id, .. } => Method::SessionClose { session_id: id.clone(), force },
            // Restart Server stops it; the service starts it again.
            Self::StopServer | Self::RestartServer => Method::ServerShutdown { force },
        }
    }

    /// `blocks.busy` for what the close ends.
    fn busy_query(&self) -> Method {
        let (mut session_id, mut window_id, mut block_ids) = (None, None, None);
        match self {
            Self::Pane(block) => block_ids = Some(vec![block.clone()]),
            Self::Tab(window) => window_id = Some(window.clone()),
            Self::Session { id, .. } => session_id = Some(id.clone()),
            Self::StopServer | Self::RestartServer => {}
        }
        Method::BlocksBusy { session_id, window_id, block_ids }
    }

    /// What closing does to the listed programs.
    fn consequence(&self, count: usize) -> String {
        let them = if count == 1 { "it" } else { "them" };
        match self {
            Self::Pane(_) => format!("Closing the pane ends {them}."),
            Self::Tab(_) => format!("Closing the tab ends {them}."),
            Self::Session { name, .. } => format!("Closing \u{201c}{name}\u{201d} ends every process in it."),
            Self::StopServer => "Stopping the server ends every session.".into(),
            Self::RestartServer => "Restarting the server ends every session.".into(),
        }
    }

    fn confirm_label(&self) -> &'static str {
        match self {
            Self::StopServer => "Stop",
            Self::RestartServer => "Restart",
            _ => "Close",
        }
    }
}

/// The dialog's first line: `vim is running in 'api'`, or a count.
pub(crate) fn headline(busy: &[BusyBlock]) -> String {
    match busy {
        [one] => format!("{} is running in '{}'", one.busy.process, one.block_title),
        many => format!("{} programs are running", many.len()),
    }
}

/// Whether `busy` spans several sessions, and several tabs.
fn spans(busy: &[BusyBlock]) -> (bool, bool) {
    let sessions = busy.iter().any(|b| b.session_id != busy[0].session_id);
    let tabs = busy.iter().any(|b| (&b.session_id, &b.window_id) != (&busy[0].session_id, &busy[0].window_id));
    (sessions, tabs)
}

/// How many rows show, and how many more there are.
fn shown(count: usize) -> (usize, usize) {
    let rows = count.min(MAX_ROWS);
    (rows, count - rows)
}

/// The programs a `busy` refusal lists; `None` for any other outcome.
pub(crate) fn refused(result: &anyhow::Result<serde_json::Value>) -> Option<Vec<BusyBlock>> {
    let error = result.as_ref().err()?.downcast_ref::<RpcError>()?;
    if error.code != RpcError::BUSY {
        return None;
    }
    let busy = serde_json::from_value::<BusyBlocks>(error.data.clone()?).ok()?.busy;
    (!busy.is_empty()).then_some(busy)
}

/// The open dialog.
pub(crate) struct CloseConfirm {
    /// The host the close goes to.
    host: usize,
    target: CloseTarget,
    busy: Vec<BusyBlock>,
    /// Each row's session › tab, when the list spans several.
    places: Vec<Option<String>>,
    focus: FocusHandle,
}

impl Workspace {
    /// Settings: ask before closing running programs.
    pub(crate) fn confirm_close(&self) -> bool {
        self.state.confirm_close
    }

    pub(crate) fn set_confirm_close(&mut self, on: bool, cx: &mut Context<Self>) {
        self.shared.update(cx, |shared, cx| shared.update_state(|s| s.confirm_close = on, cx));
    }

    /// The close a destructive command makes, when its busy check applies.
    pub(crate) fn close_target_of(&self, command: Command) -> Option<CloseTarget> {
        let local = self.hosts.get(self.active_host).is_some_and(|h| h.info.local);
        match command {
            Command::CloseSession => self.view.as_ref().map(|v| CloseTarget::Session { id: v.session.id.clone(), name: v.session.name.clone() }),
            Command::StopServer if local => Some(CloseTarget::StopServer),
            Command::RestartServer if local => Some(CloseTarget::RestartServer),
            _ => None,
        }
    }

    /// Close Pane / Close Tab: ask if something runs, else close at once.
    pub(crate) fn close(&mut self, target: CloseTarget, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.active_host;
        if !self.confirm_close() {
            return self.send_close(host, target, true, window, cx);
        }
        self.when_idle(target.clone(), window, cx, move |this, window, cx| this.send_close(host, target, false, window, cx));
    }

    /// Ask the server what `target` would end: the dialog if anything
    /// runs, else `idle`. A failed check counts as idle (the server still
    /// refuses a busy `force: false` close).
    pub(crate) fn when_idle(
        &mut self,
        target: CloseTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
        idle: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let host = self.active_host;
        let Some(control) = self.control() else { return idle(self, window, cx) };
        let query = target.busy_query();
        let task = self.rt.spawn(async move { control.call::<BusyBlocks>(query).await });
        cx.spawn_in(window, async move |this, cx| {
            let busy = match task.await.map_err(anyhow::Error::from).and_then(|r| r) {
                Ok(found) => found.busy,
                Err(error) => {
                    tracing::warn!("blocks.busy: {error:#}");
                    Vec::new()
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if busy.is_empty() {
                    idle(this, window, cx);
                } else {
                    this.open_close_confirm(host, target, busy, window, cx);
                }
            });
        })
        .detach();
    }

    /// Send the close; a `busy` refusal opens the dialog.
    pub(crate) fn send_close(&mut self, host: usize, target: CloseTarget, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.hosts.get(host) else { return };
        let Some(control) = entry.control.clone() else { return };
        tracing::debug!(?target, force, "close");
        // Paused as soon as the server agreed, before its connection
        // drops, so the connection loop does not start it again.
        let paused = matches!(target, CloseTarget::StopServer).then(|| entry.paused.clone());
        match target {
            CloseTarget::RestartServer => self.status = Some(Status::progress("Restarting server…")),
            CloseTarget::StopServer => self.status = Some(Status::progress("Stopping server…")),
            _ => {}
        }
        let method = target.method(force);
        let task = self.rt.spawn(async move {
            let result = control.call_value(method).await;
            if let Some(paused) = paused
                && refused(&result).is_none()
            {
                paused.store(true, std::sync::atomic::Ordering::Release);
            }
            result
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(busy) = refused(&result) {
                    if matches!(target, CloseTarget::StopServer | CloseTarget::RestartServer) {
                        this.clear_status();
                    }
                    return this.open_close_confirm(host, target, busy, window, cx);
                }
                if let Err(error) = &result {
                    tracing::warn!("close failed: {error:#}");
                }
                // Stopped, or the connection went with it.
                if target == CloseTarget::StopServer {
                    this.server_stopped(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_close_confirm(&mut self, host: usize, target: CloseTarget, busy: Vec<BusyBlock>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(window, cx);
        let places = self.places(&busy, cx);
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let deferred = focus.clone();
        window.defer(cx, move |window, cx| deferred.focus(window, cx));
        self.close_confirm = Some(CloseConfirm { host, target, busy, places, focus });
        cx.notify();
    }

    /// Each row's place: the session (and tab) when the list spans
    /// several sessions, the tab when it spans several tabs.
    fn places(&self, busy: &[BusyBlock], cx: &gpui::App) -> Vec<Option<String>> {
        if busy.is_empty() {
            return Vec::new();
        }
        let (sessions, tabs) = spans(busy);
        busy.iter()
            .map(|b| {
                let tab = self
                    .view
                    .as_ref()
                    .filter(|v| v.session.id == b.session_id)
                    .and_then(|v| v.session.windows.iter().find(|w| w.id == b.window_id))
                    .map(|w| w.name.clone().unwrap_or_else(|| self.window_title(w, cx)));
                match (sessions, tabs) {
                    (true, _) => Some(tab.map_or_else(|| b.session_name.clone(), |tab| format!("{} \u{203a} {tab}", b.session_name))),
                    (false, true) => tab,
                    (false, false) => None,
                }
            })
            .collect()
    }

    /// Cancel: nothing is closed.
    pub(crate) fn dismiss_close_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_confirm.take().is_some() {
            self.refocus_after_close_confirm(window, cx);
            cx.notify();
        }
    }

    fn confirm_close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.close_confirm.take() else { return };
        self.refocus_after_close_confirm(window, cx);
        self.send_close(dialog.host, dialog.target, true, window, cx);
    }

    fn refocus_after_close_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.settings {
            Some(modal) => modal.panel.update(cx, |panel, cx| panel.focus_search(window, cx)),
            None => self.refocus_terminal(window, cx),
        }
    }

    /// After Stop Server: off until Start Server or the next launch.
    fn server_stopped(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        if self.settings.is_none() {
            self.focus.focus(window, cx);
        }
        self.status = Some(Status::warning(crate::workspace::SERVER_STOPPED));
    }

    pub(crate) fn render_close_confirm(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.close_confirm.as_ref()?;
        let busy = &dialog.busy;
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .h(px(28.0))
                .px(px(14.0))
                .flex()
                .items_center()
                .continuous(8.0)
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .when(primary, |el| el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())).hover(|s| s.opacity(0.88)))
                .when(!primary, |el| el.bg(theme::control_fill()).text_color(rgb(theme::danger())).hover(|s| s.bg(theme::row_fill())))
                .child(label)
        };
        let mut headline = div().id("close-confirm-headline").font_weight(FontWeight::SEMIBOLD).text_size(px(14.0)).child(headline(busy));
        if let [one] = busy.as_slice()
            && let Some(command) = one.busy.command.clone()
        {
            headline = headline.tooltip(Tooltip::text(command));
        }
        let (rows, more) = shown(busy.len());
        let list = (busy.len() > 1).then(|| {
            let mut list = div().flex().flex_col().gap(px(2.0)).py(px(2.0));
            for (i, (b, place)) in busy.iter().zip(&dialog.places).take(rows).enumerate() {
                let mut row = div()
                    .id(("close-confirm-row", i))
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(8.0))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(div().flex_none().font_weight(FontWeight::MEDIUM).child(b.busy.process.clone()))
                    .child(div().min_w_0().text_ellipsis().child(format!("in '{}'", b.block_title)))
                    .children(place.clone().map(|p| div().min_w_0().text_ellipsis().text_size(px(12.0)).text_color(rgb(theme::muted())).child(p)));
                if let Some(command) = b.busy.command.clone() {
                    row = row.tooltip(Tooltip::text(command));
                }
                list = list.child(row);
            }
            list.when(more > 0, |el| el.child(div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(format!("and {more} more"))))
        });
        let card = div()
            .id("close-confirm")
            .track_focus(&dialog.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let k = &event.keystroke;
                match k.key.as_str() {
                    "backspace" if k.modifiers.platform => this.confirm_close_dialog(window, cx),
                    "escape" | "enter" => this.dismiss_close_confirm(window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .occlude()
            .w(px(400.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(18.0))
            .continuous(14.0)
            .bg(theme::overlay_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_xl()
            .font_family(theme::ui_font())
            .text_size(px(13.0))
            .child(headline)
            .children(list)
            .child(div().text_size(px(12.5)).text_color(rgb(theme::muted())).child(dialog.target.consequence(busy.len())))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .pt(px(6.0))
                    .child(
                        button("close-confirm-close", dialog.target.confirm_label(), false)
                            .tooltip(Tooltip::text("⌘⌫"))
                            .on_click(cx.listener(|this, _, window, cx| this.confirm_close_dialog(window, cx))),
                    )
                    .child(
                        button("close-confirm-cancel", "Cancel", true)
                            .on_click(cx.listener(|this, _, window, cx| this.dismiss_close_confirm(window, cx))),
                    ),
            );
        Some(
            div()
                .id("close-confirm-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(TITLEBAR_HEIGHT + 60.0))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.dismiss_close_confirm(window, cx)))
                .child(card)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::model::Busy;

    use super::*;

    fn busy(process: &str, title: &str, session: &str, window: &str) -> BusyBlock {
        BusyBlock {
            block_id: format!("{session}-{window}-{title}"),
            session_id: session.into(),
            window_id: window.into(),
            session_name: format!("{session} name"),
            block_title: title.into(),
            busy: Busy { process: process.into(), command: None, since: 0 },
        }
    }

    #[test]
    fn one_program_is_named_several_are_counted() {
        assert_eq!(headline(&[busy("vim", "api", "s", "w")]), "vim is running in 'api'");
        assert_eq!(
            headline(&[busy("vim", "api", "s", "w"), busy("cargo", "build", "s", "w"), busy("ssh", "prod", "s", "w")]),
            "3 programs are running"
        );
    }

    #[test]
    fn rows_stop_at_eight() {
        assert_eq!(shown(3), (3, 0));
        assert_eq!(shown(8), (8, 0));
        assert_eq!(shown(11), (8, 3));
    }

    #[test]
    fn places_show_only_when_the_list_spans_them() {
        let tab = [busy("vim", "a", "s", "w"), busy("top", "b", "s", "w")];
        assert_eq!(spans(&tab), (false, false));
        let session = [busy("vim", "a", "s", "w1"), busy("top", "b", "s", "w2")];
        assert_eq!(spans(&session), (false, true));
        let server = [busy("vim", "a", "s1", "w"), busy("top", "b", "s2", "w")];
        assert_eq!(spans(&server), (true, true));
    }

    #[test]
    fn a_busy_refusal_carries_its_list() {
        let list = vec![busy("sleep", "shell", "s", "w")];
        let data = serde_json::to_value(BusyBlocks { busy: list.clone() }).unwrap();
        let refusal: anyhow::Result<serde_json::Value> = Err(RpcError::new(RpcError::BUSY, "sleep is running in 'shell'").with_data(data).into());
        assert_eq!(refused(&refusal), Some(list));
        assert_eq!(refused(&Err(RpcError::not_found("block b").into())), None);
        assert_eq!(refused(&Err(RpcError::new(RpcError::BUSY, "no data").into())), None);
        assert_eq!(refused(&Ok(serde_json::json!({}))), None);
    }

    #[test]
    fn targets_close_and_check_what_they_end() {
        let pane = CloseTarget::Pane("b1".into());
        assert!(matches!(pane.method(true), Method::BlockClose { force: true, .. }));
        assert!(matches!(pane.busy_query(), Method::BlocksBusy { session_id: None, window_id: None, block_ids: Some(ids) } if ids == ["b1"]));
        let tab = CloseTarget::Tab("w1".into());
        assert!(matches!(tab.busy_query(), Method::BlocksBusy { window_id: Some(w), .. } if w == "w1"));
        let session = CloseTarget::Session { id: "s1".into(), name: "work".into() };
        assert!(matches!(session.method(false), Method::SessionClose { force: false, .. }));
        assert!(matches!(CloseTarget::StopServer.busy_query(), Method::BlocksBusy { session_id: None, window_id: None, block_ids: None }));
        assert!(matches!(CloseTarget::RestartServer.method(true), Method::ServerShutdown { force: true }));
        assert_eq!(tab.consequence(3), "Closing the tab ends them.");
        assert_eq!(session.consequence(1), "Closing \u{201c}work\u{201d} ends every process in it.");
    }
}
