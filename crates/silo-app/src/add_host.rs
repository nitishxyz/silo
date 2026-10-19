//! "Add Remote Host…" sheet: paste a `silo1…` pairing ticket (and an
//! optional name). The host is saved only once pairing succeeded.

use gpui::{AnyElement, Context, Entity, FontWeight, IntoElement, Subscription, Window, div, prelude::*, px, rgb};
use silo_client::transport::PairTicket;

use crate::theme::Continuous as _;
use crate::{
    conn::{Failure, classify},
    hosts::{self, HostInfo, RemoteHost},
    text_field::{FieldEvent, TextField},
    theme,
    workspace::Workspace,
};

pub struct AddHostSheet {
    name: Entity<TextField>,
    ticket: Entity<TextField>,
    /// Re-pairing this host (Pair again) instead of adding a new one.
    replace: Option<usize>,
    busy: bool,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl AddHostSheet {
    pub fn new(name: Option<String>, replace: Option<usize>, window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let name_field = cx.new(|cx| TextField::new("Optional", cx));
        if let Some(name) = name {
            name_field.update(cx, |f, cx| f.set_text(name, cx));
        }
        let ticket = cx.new(|cx| TextField::new("silo1…", cx));
        let subs = vec![
            cx.subscribe_in(&ticket, window, |this, _, event: &FieldEvent, window, cx| this.sheet_event(0, event, window, cx)),
            cx.subscribe_in(&name_field, window, |this, _, event: &FieldEvent, window, cx| this.sheet_event(1, event, window, cx)),
        ];
        let focus = ticket.read(cx).focus.clone();
        focus.focus(window, cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        Self { name: name_field, ticket, replace, busy: false, error: None, _subs: subs }
    }
}

/// Short message for a failed pairing.
pub(crate) fn pair_error(error: &anyhow::Error) -> &'static str {
    match classify(error) {
        Failure::NotPaired => "Ticket expired or already used.",
        Failure::DifferentBuild => "That machine runs a different Silo build.",
        Failure::Upgrading => "That machine is updating. Try again.",
        Failure::Unreachable => "Can't reach that machine.",
    }
}

/// The saved name: the typed one, else a short form of the id.
fn host_name(typed: &str, id: &str) -> String {
    let typed = typed.trim();
    if typed.is_empty() { id.get(..8).unwrap_or(id).to_owned() } else { typed.to_owned() }
}

impl Workspace {
    fn sheet_event(&mut self, field: usize, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = &self.add_host else { return };
        match event {
            FieldEvent::Cancel => self.close_add_host(window, cx),
            FieldEvent::Down | FieldEvent::Up => {
                let next = if field == 0 { &sheet.name } else { &sheet.ticket };
                let focus = next.read(cx).focus.clone();
                focus.focus(window, cx);
            }
            FieldEvent::Submit => self.save_add_host(window, cx),
            FieldEvent::Changed(_) => {
                if let Some(sheet) = &mut self.add_host {
                    sheet.error = None;
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn open_add_host(&mut self, name: Option<String>, replace: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.add_host = Some(AddHostSheet::new(name, replace, window, cx));
        cx.notify();
    }

    /// "Pair again" for a host the server no longer knows.
    pub(crate) fn pair_again(&mut self, host: usize, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.hosts.get(host).map(|h| h.info.name.clone());
        self.open_add_host(name, Some(host), window, cx);
    }

    fn close_add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_host = None;
        self.refocus_terminal(window, cx);
        cx.notify();
    }

    fn sheet_error(&mut self, error: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(sheet) = &mut self.add_host {
            sheet.busy = false;
            sheet.error = Some(error.into());
        }
        cx.notify();
    }

    /// Decode the ticket, pair in the background, then save and connect.
    fn save_add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = &self.add_host else { return };
        if sheet.busy {
            return;
        }
        let typed = sheet.name.read(cx).text.clone();
        let replace = sheet.replace;
        let Ok(ticket) = PairTicket::decode(&sheet.ticket.read(cx).text) else {
            return self.sheet_error("Invalid ticket.", cx);
        };
        let id = ticket.addr.id.to_string();
        let replacing = replace.and_then(|i| self.hosts.get(i)).and_then(|h| h.info.transport.remote_id());
        if replacing.as_deref() != Some(id.as_str()) && hosts::find(&hosts::load(&self.home), &id).is_some() {
            return self.sheet_error("This machine is already added.", cx);
        }
        if let Some(sheet) = &mut self.add_host {
            sheet.busy = true;
            sheet.error = None;
        }
        let home = self.home.clone();
        let client = crate::conn::client_name();
        let task = self.rt.spawn(async move {
            let net = crate::remote::net(&home).await?;
            silo_client::pair(net, &ticket, &client).await.map(|_| ticket.addr)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(addr) => {
                    let host = RemoteHost::new(addr, host_name(&typed, &id));
                    match this.save_host(host, replace, window, cx) {
                        Ok(()) => this.close_add_host(window, cx),
                        Err(error) => this.sheet_error(error, cx),
                    }
                }
                Err(error) => {
                    tracing::info!("pairing failed: {error:#}");
                    this.sheet_error(pair_error(&error), cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Forget a remote host (hosts.json and the switcher).
    pub(crate) fn remove_host(&mut self, index: usize, cx: &mut Context<Self>) -> Result<String, String> {
        let entry = self.hosts.get(index).ok_or("No such host.")?;
        let Some(id) = entry.info.transport.remote_id() else { return Err("This Mac cannot be removed.".into()) };
        let saved: Vec<RemoteHost> = hosts::load(&self.home).into_iter().filter(|h| h.id != id).collect();
        hosts::save(&self.home, &saved).map_err(|e| format!("Could not save: {e:#}"))?;
        self.shared.update(cx, |shared, cx| shared.forget(index, cx));
        self.sync_hosts(cx);
        Ok(self.hosts[index].info.name.clone())
    }

    /// Give a remote host a new name.
    pub(crate) fn rename_host(&mut self, index: usize, name: String, cx: &mut Context<Self>) -> Result<(), String> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err("Enter a name.".into());
        }
        let entry = self.hosts.get(index).ok_or("No such host.")?;
        let Some(id) = entry.info.transport.remote_id() else { return Err("This Mac cannot be renamed.".into()) };
        let mut saved = hosts::load(&self.home);
        saved.iter_mut().filter(|h| h.id == id).for_each(|h| h.name.clone_from(&name));
        hosts::save(&self.home, &saved).map_err(|e| format!("Could not save: {e:#}"))?;
        self.shared.update(cx, |shared, cx| shared.rename(index, name, cx));
        self.sync_hosts(cx);
        Ok(())
    }

    /// Persist a paired host (replacing its old entry) and connect to it.
    fn save_host(&mut self, host: RemoteHost, replace: Option<usize>, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        let mut saved: Vec<RemoteHost> = hosts::load(&self.home).into_iter().filter(|h| h.id != host.id).collect();
        saved.push(host.clone());
        hosts::save(&self.home, &saved).map_err(|e| format!("Could not save: {e:#}"))?;
        if let Some(index) = replace.filter(|i| *i < self.hosts.len()) {
            self.shared.update(cx, |shared, cx| shared.forget(index, cx));
        }
        self.add_host_entry(HostInfo::remote(self.home.clone(), &host), cx);
        if replace.is_some() {
            self.switch_host(self.hosts.len() - 1, window, cx);
        }
        Ok(())
    }

    pub(crate) fn render_add_host(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.add_host.as_ref()?;
        let field = |label: &'static str, input: &Entity<TextField>| {
            div().flex().flex_col().gap(px(6.0)).child(div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(label)).child(
                div()
                    .flex()
                    .items_center()
                    .h(px(34.0))
                    .px(px(10.0))
                    .continuous(8.0)
                    .bg(rgb(theme::background()))
                    .border_1()
                    .border_color(rgb(theme::border()))
                    .child(input.clone()),
            )
        };
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px(px(14.0))
                .py(px(6.0))
                .continuous(8.0)
                .text_size(px(13.0))
                .cursor_pointer()
                .when(primary, |el| el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())))
                .when(!primary, |el| el.bg(rgb(theme::tab_active())).hover(|s| s.bg(rgb(theme::row_active()))))
                .child(label)
        };
        let card = div()
            .id("add-host")
            .occlude()
            .w(px(420.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .p(px(20.0))
            .continuous(14.0)
            .bg(rgb(theme::popover()))
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_xl()
            .font_family(theme::ui_font())
            .text_size(px(13.0))
            .child(div().text_size(px(15.0)).font_weight(FontWeight::BOLD).child(if sheet.replace.is_some() {
                "Pair Again"
            } else {
                "Add Remote Host"
            }))
            .child(div().text_size(px(12.0)).text_color(rgb(theme::muted())).child("On the other Mac: Settings › Server › Pair a device…"))
            .child(field("Ticket", &sheet.ticket))
            .child(field("Name", &sheet.name))
            .children(sheet.error.clone().map(|e| div().text_size(px(12.0)).text_color(rgb(theme::danger())).child(e)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .when(sheet.busy, |el| el.child(div().flex_1().text_size(px(12.0)).text_color(rgb(theme::muted())).child("Pairing…")))
                    .child(button("add-host-cancel", "Cancel", false).on_click(cx.listener(|this, _, window, cx| this.close_add_host(window, cx))))
                    .child(
                        button("add-host-save", "Pair", true)
                            .when(sheet.busy, |el| el.opacity(0.5))
                            .on_click(cx.listener(|this, _, window, cx| this.save_add_host(window, cx))),
                    ),
            );
        Some(
            div()
                .id("add-host-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(90.0))
                .bg(gpui::rgba(0x0000_0088))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.close_add_host(window, cx)))
                .child(card)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_errors_are_short() {
        let message = String::from("why");
        assert_eq!(pair_error(&silo_client::Unauthorized { message: message.clone() }.into()), "Ticket expired or already used.");
        assert_eq!(pair_error(&silo_client::BuildMismatch { message }.into()), "That machine runs a different Silo build.");
        assert_eq!(pair_error(&anyhow::anyhow!("dial server")), "Can't reach that machine.");
        assert!(PairTicket::decode("silo1notbase32!").is_err());
    }

    #[test]
    fn default_name_is_short_id() {
        assert_eq!(host_name("  ", "abcdef0123456789"), "abcdef01");
        assert_eq!(host_name(" mini ", "abcdef0123456789"), "mini");
    }
}
