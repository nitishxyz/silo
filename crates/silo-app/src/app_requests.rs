//! Forwarded `app.*` requests (`docs/PHASE7.md` §4.2): the local server
//! sends one to this app as `app.request`; the frontmost window (or the
//! one showing the named block) runs it and answers with `app.reply`.

use gpui::{Context, Window};
use serde_json::{Value, json};
use silo_proto::control::{AppCall, Density, Method, RpcError};

use crate::{
    commands::{Api, Command},
    settings_table::{Kind, SETTINGS, SettingId},
    workspace::Workspace,
};

/// A setting's API id: its name in snake case (`font_size`,
/// `color_foreground`).
pub(crate) fn setting_key(id: SettingId) -> String {
    let mut key = String::new();
    for c in format!("{id:?}").chars().filter(|c| c.is_alphanumeric()) {
        if c.is_uppercase() && !key.is_empty() {
            key.push('_');
        }
        key.extend(c.to_lowercase());
    }
    key
}

fn setting(key: &str) -> Result<SettingId, RpcError> {
    SETTINGS.iter().map(|s| s.id).find(|id| setting_key(*id) == key).ok_or_else(|| RpcError::not_found(format!("setting {key}")))
}

impl Workspace {
    /// A forwarded request from host `host`: run here if this window is
    /// the one it is for, and reply.
    pub(crate) fn app_request(&mut self, host: usize, request_id: &str, call: &AppCall, window: &mut Window, cx: &mut Context<Self>) {
        if call.method.starts_with("browser.") {
            self.browser_request(host, request_id, call, window, cx);
            return;
        }
        // Only this Mac's server forwards requests to its app.
        let Some(control) = self.hosts.get(host).filter(|h| h.info.local).and_then(|h| h.control.clone()) else { return };
        if !self.runs_requests(call, window, cx) {
            return;
        }
        let parsed = serde_json::from_value::<Method>(json!({"method": call.method, "params": call.params}));
        let quit = matches!(parsed, Ok(Method::AppQuit { .. }));
        let result = match parsed {
            Ok(method) if !quit => self.run_app_method(host, method, window, cx),
            Ok(_) => Ok(json!({})),
            Err(error) => Err(RpcError::bad_request(error.to_string())),
        };
        tracing::debug!(method = call.method, ok = result.is_ok(), "app request");
        let (result, error) = match result {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error)),
        };
        let reply = Method::AppReply { request_id: request_id.to_owned(), result, error };
        let sent = self.rt.spawn(async move { control.call_value(reply).await });
        if quit {
            // Answered first: quitting ends the connection.
            cx.spawn(async move |_, cx| {
                let _ = sent.await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
        cx.notify();
    }

    fn browser_request(&mut self, host: usize, request_id: &str, call: &AppCall, window: &Window, cx: &mut Context<Self>) {
        let Some(block) = call.params.get("block_id").and_then(Value::as_str) else { return };
        // A block nobody owns comes to this app; a window with its pane runs it.
        let owns = |workspace: &Workspace| {
            workspace.active_host == host && workspace.blocks.browser(block).is_some_and(|pane| pane.read(cx).is_owner() || pane.read(cx).unowned())
        };
        if !owns(self) {
            return;
        }
        // Never read our own window through its handle (`is_own_window`).
        let windows = cx.window_stack().unwrap_or_else(|| cx.windows());
        let target = windows.iter().find(|handle| {
            self.is_own_window(**handle, window, cx) || handle.downcast::<Workspace>().and_then(|w| w.read(cx).ok()).is_some_and(owns)
        });
        if target.is_some_and(|handle| !self.is_own_window(*handle, window, cx)) {
            return;
        }
        let Some(control) = self.hosts.get(host).and_then(|host| host.control.clone()) else { return };
        let Some(pane) = self.blocks.browser(block) else { return };
        let executor = pane.update(cx, |pane, cx| {
            if pane.unowned() {
                pane.claim();
                return pane.executor(cx);
            }
            pane.agent_executor(cx)
        });
        let request_id = request_id.to_owned();
        let method = call.method.clone();
        let params = call.params.clone();
        let timeout = params.get("timeout_ms").and_then(Value::as_u64).map_or(30_000, |ms| ms.saturating_add(5_000)).min(120_000);
        let rt = self.rt.clone();
        cx.spawn(async move |_, cx| {
            let result = match executor {
                Ok(executor) => {
                    tokio::select! {
                        result = executor.run(&method, params) => result,
                        () = cx.background_executor().timer(std::time::Duration::from_millis(timeout)) => Err(anyhow::anyhow!("browser request timed out")),
                    }
                }
                Err(error) => Err(error),
            };
            let (result, error) = match result {
                Ok(value) => (Some(value), None),
                Err(error) => (None, Some(RpcError::bad_request(error.to_string()))),
            };
            rt.spawn(async move { let _ = control.call_value(Method::AppReply { request_id, result, error }).await; });
        }).detach();
    }

    /// Whether this window runs `call`: the frontmost window showing its
    /// `block_id`, else the frontmost window.
    fn runs_requests(&self, call: &AppCall, window: &Window, cx: &Context<Self>) -> bool {
        let block = call.params.get("block_id").and_then(Value::as_str);
        let windows: Vec<gpui::AnyWindowHandle> =
            cx.window_stack().unwrap_or_else(|| cx.windows()).into_iter().filter(|w| w.downcast::<Workspace>().is_some()).collect();
        // Never read our own window through its handle (`is_own_window`).
        let shows = |handle: &gpui::AnyWindowHandle| {
            let Some(block) = block else { return true };
            if self.is_own_window(*handle, window, cx) {
                return self.block_info(block).is_some();
            }
            handle.downcast::<Workspace>().and_then(|w| w.read(cx).ok()).is_some_and(|w| w.block_info(block).is_some())
        };
        let target = windows.iter().find(|w| shows(w)).or(windows.first());
        target.is_none_or(|w| self.is_own_window(*w, window, cx))
    }

    /// Run an `app.*` method in this window.
    fn run_app_method(&mut self, host: usize, method: Method, window: &mut Window, cx: &mut Context<Self>) -> Result<Value, RpcError> {
        let name = silo_proto::api::method_name(&method);
        // Commands without params run as from the palette.
        if let Some(&command) = Command::ALL.iter().find(|c| matches!(c.api(), Api::Method(m) if m == name))
            && matches!(
                method,
                Method::AppWindowCycle { .. }
                    | Method::AppSwitcher { .. }
                    | Method::AppSettingsReload { .. }
                    | Method::AppSettingsOpenFile { .. }
                    | Method::AppUpdateCheck { .. }
                    | Method::AppHide { .. }
                    | Method::AppHideOthers { .. }
                    | Method::AppShowAll { .. }
            )
        {
            self.execute(command, window, cx);
            return Ok(json!({}));
        }
        match method {
            Method::AppWindowNew { session_id, .. } => {
                let session = session_id.map(|id| (host, id));
                cx.defer(move |cx| {
                    if let Err(error) = crate::open_window(session, cx) {
                        tracing::warn!("open window: {error:#}");
                    }
                });
            }
            Method::AppSessionStep { delta, .. } => self.step_session(delta as isize, window, cx),
            Method::AppHostsAdd { ticket, .. } => {
                self.open_add_host(None, None, window, cx);
                if let (Some(ticket), Some(sheet)) = (ticket, &self.add_host) {
                    sheet.fill_ticket(&ticket, cx);
                }
            }
            Method::AppHostsRename { host, name, .. } => {
                let index = self.host_index(&host)?;
                self.rename_host(index, name, cx).map_err(RpcError::bad_request)?;
                self.refresh_settings(None, cx);
            }
            Method::AppHostsRemove { host, .. } => {
                let index = self.host_index(&host)?;
                self.remove_host(index, cx).map_err(RpcError::bad_request)?;
            }
            Method::AppHostsList { .. } => {
                let hosts: Vec<Value> = self
                    .hosts
                    .iter()
                    .filter(|h| !h.removed)
                    .map(|h| json!({"name": h.info.name, "id": host_id(h), "local": h.info.local, "connected": h.control.is_some()}))
                    .collect();
                return Ok(json!({ "hosts": hosts }));
            }
            Method::AppCopy { block_id, .. } => self.in_block(block_id, Command::Copy, window, cx),
            Method::AppPaste { block_id, .. } => self.in_block(block_id, Command::Paste, window, cx),
            Method::AppSelectAll { block_id, .. } => self.in_block(block_id, Command::SelectAll, window, cx),
            Method::AppAppearance { density, .. } => self.set_density(
                match density {
                    Density::Comfortable => crate::hosts::Density::Comfortable,
                    Density::Compact => crate::hosts::Density::Compact,
                },
                cx,
            ),
            Method::AppPaneHeaders { visible, .. } => self.set_pane_headers(crate::pane_header::PaneHeaders::all(visible), cx),
            Method::AppFullScreen { on, .. } => {
                let now = window.is_fullscreen();
                let want = on.unwrap_or(!now);
                if want != now {
                    window.toggle_fullscreen();
                }
                return Ok(json!({ "full_screen": want }));
            }
            Method::AppSettingsOpen { section: None, .. } => self.open_settings(window, cx),
            Method::AppSettingsOpen { section: Some(section), .. } => {
                let id = setting(&section).ok().or_else(|| SETTINGS.iter().find(|s| s.section.label().eq_ignore_ascii_case(&section)).map(|s| s.id));
                let id = id.ok_or_else(|| RpcError::not_found(format!("settings section {section}")))?;
                self.open_settings_at(id, window, cx);
            }
            Method::AppSettingsGet { id, .. } => {
                let snapshot = self.settings_snapshot(None, cx);
                let value = |id: SettingId| {
                    let value = match id.setting().kind {
                        Kind::Toggle => id.on(&snapshot).map(Value::from),
                        Kind::Number => id.number(&snapshot).and_then(|n| n.parse::<f64>().ok()).map(Value::from),
                        Kind::Choice => id.value(&snapshot).map(Value::from),
                        Kind::Open => None,
                    };
                    value.map(|value| json!({"id": setting_key(id), "title": id.setting().title, "value": value}))
                };
                return match id {
                    Some(key) => value(setting(&key)?).ok_or_else(|| RpcError::bad_request(format!("{key} is only in Settings"))),
                    None => Ok(json!({ "settings": SETTINGS.iter().filter_map(|s| value(s.id)).collect::<Vec<_>>() })),
                };
            }
            Method::AppSettingsSet { id, value, .. } => {
                let setting_id = setting(&id)?;
                let snapshot = self.settings_snapshot(None, cx);
                let event = match setting_id.setting().kind {
                    Kind::Toggle => value
                        .as_bool()
                        .and_then(|on| setting_id.toggled(on))
                        .ok_or_else(|| RpcError::bad_request(format!("{id} is true or false")))?,
                    Kind::Number => {
                        let text = value.as_str().map_or_else(|| value.to_string(), str::to_owned);
                        setting_id.parse(&text).map_err(RpcError::bad_request)?
                    }
                    Kind::Choice => {
                        let choices = setting_id.choices(&snapshot);
                        let wanted = value.as_str().unwrap_or_default();
                        match choices.iter().find(|c| c.label.eq_ignore_ascii_case(wanted)) {
                            Some(choice) => choice.event.clone(),
                            None => {
                                let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
                                return Err(RpcError::bad_request(format!("{id} is one of: {}", labels.join(", "))));
                            }
                        }
                    }
                    Kind::Open => return Err(RpcError::bad_request(format!("{id} is only in Settings"))),
                };
                let outcome = self.apply_settings_event(&event, window, cx).unwrap_or_default();
                self.refresh_settings(None, cx);
                if let Some(error) = outcome.error {
                    return Err(RpcError::bad_request(error));
                }
            }
            Method::AppPalette { query, .. } => self.open_palette_with(query.unwrap_or_default(), window, cx),
            Method::AppOpenShare { link, .. } => {
                let ticket = silo_client::transport::ShareTicket::parse(&link).map_err(|_| RpcError::bad_request("not a share link"))?;
                self.open_share(ticket, window, cx);
            }
            Method::AppJumpPrompt { delta, block_id, .. } => {
                self.terminal_pane(block_id.as_deref(), window, cx)?.update(cx, |pane, cx| pane.jump_prompt(delta, cx));
            }
            Method::AppSelectOutput { block_id, .. } => {
                if !self.terminal_pane(block_id.as_deref(), window, cx)?.update(cx, |pane, cx| pane.select_output(cx)) {
                    return Err(RpcError::not_found("command output"));
                }
            }
            Method::AppCopyOutput { block_id, .. } => {
                if !self.terminal_pane(block_id.as_deref(), window, cx)?.update(cx, |pane, cx| pane.copy_output(cx)) {
                    return Err(RpcError::not_found("command output"));
                }
            }
            _ => return Err(RpcError::bad_request(format!("{name} is not an app method"))),
        }
        Ok(json!({}))
    }

    /// The terminal pane of `block` (focusing it first), or the focused one.
    fn terminal_pane(
        &mut self,
        block: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<gpui::Entity<crate::pane::Pane>, RpcError> {
        let Some(block) = block else {
            return self.focused_pane(window, cx).ok_or_else(|| RpcError::not_found("a focused terminal"));
        };
        self.focus_block(block, window, cx);
        self.blocks.terminal(block).cloned().ok_or_else(|| RpcError::not_found(format!("terminal {block}")))
    }

    /// Run an edit command in `block` (focusing it first), or the focused one.
    fn in_block(&mut self, block: Option<String>, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(block) = block {
            self.focus_block(&block, window, cx);
        }
        self.execute(command, window, cx);
    }

    /// The host named `host` (or with that endpoint id prefix).
    fn host_index(&self, host: &str) -> Result<usize, RpcError> {
        self.hosts
            .iter()
            .position(|h| !h.removed && (h.info.name.eq_ignore_ascii_case(host) || (host.len() >= 4 && host_id(h).starts_with(host))))
            .ok_or_else(|| RpcError::not_found(format!("host {host}")))
    }
}

/// A host's endpoint id (`local` for this Mac).
fn host_id(h: &crate::hosts_state::HostEntry) -> String {
    match &h.info.transport {
        crate::hosts::Transport::Iroh { addr, .. } => addr.id.to_string(),
        crate::hosts::Transport::Shared { ticket, .. } => crate::share::key(ticket),
        crate::hosts::Transport::Local { .. } => "local".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_keys_are_snake_case_and_unique() {
        assert_eq!(setting_key(SettingId::FontSize), "font_size");
        assert_eq!(setting_key(SettingId::Color(crate::settings_panel::ColorSlot::Foreground)), "color_foreground");
        let mut keys: Vec<String> = SETTINGS.iter().map(|s| setting_key(s.id)).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), SETTINGS.len());
        assert_eq!(setting("font_size").unwrap(), SettingId::FontSize);
        assert_eq!(setting("nope").unwrap_err().code, "not_found");
    }
}
