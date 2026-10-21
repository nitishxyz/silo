//! "Add Machine…" sheet. Paste ticket: pair with a `silo1…` ticket (and an
//! optional name); the host is saved only once pairing succeeded. Run on
//! the other machine: a join invite of this Mac's server, as a command to
//! run there; its offer is linked without a prompt (see `offers.rs`).

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, FocusHandle, FontWeight, IntoElement, KeyDownEvent, Subscription, Task, Window, div, prelude::*, px,
    rgb,
};
use silo_client::transport::{EndpointAddr, PairTicket};
use silo_proto::control::{InvitePurpose, Method, PairInvite};

use crate::theme::Continuous as _;
use crate::{
    conn::{Failure, classify},
    hosts,
    text_field::{FieldEvent, TextField},
    theme,
    workspace::Workspace,
};

/// How long a join invite is valid.
const JOIN_TTL_SECS: u64 = 15 * 60;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ticket,
    Join,
}

pub struct AddHostSheet {
    name: Entity<TextField>,
    ticket: Entity<TextField>,
    /// Re-pairing this host (Pair again) instead of adding a new one.
    replace: Option<usize>,
    mode: Mode,
    /// The join invite shown in Join mode.
    invite: Option<PairInvite>,
    /// When `invite` expires; the sheet ticks until then.
    expires: Option<Instant>,
    _tick: Option<Task<()>>,
    /// Which command was copied last (0 install, 1 short).
    copied: Option<usize>,
    /// Keys (Escape) while no field has focus.
    focus: FocusHandle,
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
        let ticket = cx.new(|cx| TextField::new("silo1… or a shared link", cx));
        let subs = vec![
            cx.subscribe_in(&ticket, window, |this, _, event: &FieldEvent, window, cx| this.sheet_event(0, event, window, cx)),
            cx.subscribe_in(&name_field, window, |this, _, event: &FieldEvent, window, cx| this.sheet_event(1, event, window, cx)),
        ];
        let focus = ticket.read(cx).focus.clone();
        focus.focus(window, cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        Self {
            name: name_field,
            ticket,
            replace,
            mode: Mode::Ticket,
            invite: None,
            expires: None,
            _tick: None,
            copied: None,
            focus: cx.focus_handle(),
            busy: false,
            error: None,
            _subs: subs,
        }
    }
}

/// Short message for a failed pairing.
pub(crate) fn pair_error(error: &anyhow::Error) -> &'static str {
    match classify(error) {
        Failure::NotPaired => "Ticket expired or already used.",
        Failure::Incompatible => "That machine runs an incompatible Silo.",
        Failure::Upgrading => "That machine is updating. Try again.",
        Failure::Unreachable => "Can't reach that machine.",
    }
}

/// The saved name: the typed one, else the server's hostname.
pub(crate) fn host_name(typed: &str, hostname: &str) -> String {
    [typed.trim(), hostname.trim()].into_iter().find(|n| !n.is_empty()).unwrap_or("silo host").to_owned()
}

/// Pair this device with `ticket`'s server in the background; its address
/// and hostname.
pub(crate) fn pair(
    rt: &tokio::runtime::Handle,
    home: PathBuf,
    ticket: PairTicket,
) -> tokio::task::JoinHandle<anyhow::Result<(EndpointAddr, String)>> {
    let client = crate::conn::client_name();
    rt.spawn(async move {
        let net = crate::remote::net(&home).await?;
        let (control, _) = silo_client::pair(net, &ticket, &client).await?;
        Ok((ticket.addr, control.hello().server.hostname.clone()))
    })
}

/// The commands that join the other machine with `ticket`: installing
/// Silo first, or with Silo already there.
fn join_commands(ticket: &str) -> [String; 2] {
    [format!("curl -fsSL {} | sh -s -- join {ticket}", silo_proto::INSTALL_URL), format!("silo join {ticket}")]
}

/// The countdown of a join invite with `left` to go; `None` once expired.
fn expiry_text(left: Duration) -> Option<String> {
    match left.as_secs() {
        0 => None,
        secs @ 1..60 => Some(format!("Expires in {secs}s.")),
        secs => Some(format!("Expires in {} min.", secs.div_ceil(60))),
    }
}

impl AddHostSheet {
    /// Fill in the ticket field (a forwarded `app.hosts_add`).
    pub(crate) fn fill_ticket(&self, ticket: &str, cx: &mut gpui::App) {
        self.ticket.update(cx, |field, cx| field.set_text(ticket, cx));
    }
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

    pub(crate) fn close_add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_host = None;
        self.refocus_terminal(window, cx);
        cx.notify();
    }

    pub(crate) fn sheet_error(&mut self, error: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(sheet) = &mut self.add_host {
            sheet.busy = false;
            sheet.error = Some(error.into());
        }
        cx.notify();
    }

    /// The join invite this window's sheet shows.
    pub(crate) fn join_invite(&self) -> Option<&str> {
        self.add_host.as_ref()?.invite.as_ref().map(|i| i.invite_id.as_str())
    }

    fn set_add_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = &mut self.add_host else { return };
        if sheet.mode == mode {
            return;
        }
        (sheet.mode, sheet.error) = (mode, None);
        match mode {
            Mode::Ticket => {
                let focus = sheet.ticket.read(cx).focus.clone();
                focus.focus(window, cx);
            }
            Mode::Join => {
                sheet.focus.focus(window, cx);
                if sheet.invite.is_none() && !sheet.busy {
                    self.create_join_invite(cx);
                }
            }
        }
        cx.notify();
    }

    /// Ask this Mac's server for a join invite.
    fn create_join_invite(&mut self, cx: &mut Context<Self>) {
        let Some(control) = self.hosts.first().and_then(|h| h.control.clone()) else {
            return self.sheet_error("This Mac's server isn't running.", cx);
        };
        if let Some(sheet) = &mut self.add_host {
            sheet.busy = true;
        }
        let method = Method::PairCreate { ttl_secs: JOIN_TTL_SECS, purpose: InvitePurpose::Join };
        let task = self.rt.spawn(async move { control.call::<PairInvite>(method).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| match result {
                Ok(invite) => {
                    let Some(sheet) = &mut this.add_host else { return };
                    let id = invite.invite_id.clone();
                    sheet.expires = Some(Instant::now() + Duration::from_secs(invite.expires_in_secs));
                    sheet._tick = Some(cx.spawn(async move |this, cx| {
                        loop {
                            cx.background_executor().timer(Duration::from_secs(1)).await;
                            let Ok(more) = this.update(cx, |this, cx| {
                                cx.notify();
                                this.add_host.as_ref().and_then(|s| s.expires).is_some_and(|at| at > Instant::now())
                            }) else {
                                return;
                            };
                            if !more {
                                return;
                            }
                        }
                    }));
                    (sheet.busy, sheet.invite) = (false, Some(invite));
                    this.shared.update(cx, |shared, _| shared.offers.invites.insert(id));
                    cx.notify();
                }
                Err(error) if error.downcast_ref::<silo_client::Offline>().is_some() => this.sheet_error("Not online yet. Try again shortly.", cx),
                Err(error) => {
                    tracing::warn!("pair.create: {error:#}");
                    this.sheet_error("Could not create an invite.", cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// The shown invite expired: replace it.
    fn new_join_invite(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = &mut self.add_host else { return };
        if sheet.busy {
            return;
        }
        let old = sheet.invite.take().map(|i| i.invite_id);
        (sheet.expires, sheet._tick, sheet.copied, sheet.error) = (None, None, None, None);
        if let Some(old) = old {
            self.shared.update(cx, |shared, _| shared.offers.invites.remove(&old));
        }
        self.create_join_invite(cx);
    }

    /// Decode the ticket, pair in the background, then save and connect.
    fn save_add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = &self.add_host else { return };
        if sheet.busy || sheet.mode != Mode::Ticket {
            return;
        }
        let typed = sheet.name.read(cx).text.clone();
        let replace = sheet.replace;
        let text = sheet.ticket.read(cx).text.clone();
        // A shared link (`silos1…`) opens what was shared; it pairs nothing.
        if replace.is_none()
            && let Ok(share) = silo_client::transport::ShareTicket::parse(&text)
        {
            self.close_add_host(window, cx);
            return self.open_share(share, window, cx);
        }
        let Ok(ticket) = PairTicket::decode(&text) else {
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
        let task = pair(&self.rt, self.home.clone(), ticket);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok((addr, hostname)) => {
                    let typed = Some(typed.trim().to_owned()).filter(|t| !t.is_empty());
                    match this.save_host(addr, typed, &host_name("", &hostname), replace, window, cx) {
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
        if entry.info.transport.is_shared() {
            crate::share::remove(&self.home, &entry.info.key()).map_err(|e| format!("Could not save: {e:#}"))?;
            self.shared.update(cx, |shared, cx| shared.forget(index, cx));
            self.sync_hosts(cx);
            return Ok(self.hosts[index].info.name.clone());
        }
        let Some(id) = entry.info.transport.remote_id() else { return Err("This Mac cannot be removed.".into()) };
        hosts::update(&self.home, |saved| saved.retain(|h| h.id != id)).map_err(|e| format!("Could not save: {e:#}"))?;
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
        if let hosts::Transport::Shared { ticket, .. } = &entry.info.transport {
            crate::share::save(&self.home, &name, ticket).map_err(|e| format!("Could not save: {e:#}"))?;
            self.shared.update(cx, |shared, cx| shared.rename(index, name, cx));
            self.sync_hosts(cx);
            return Ok(());
        }
        let Some(id) = entry.info.transport.remote_id() else { return Err("This Mac cannot be renamed.".into()) };
        hosts::update(&self.home, |saved| saved.iter_mut().filter(|h| h.id == id).for_each(|h| h.name.clone_from(&name)))
            .map_err(|e| format!("Could not save: {e:#}"))?;
        self.shared.update(cx, |shared, cx| shared.rename(index, name, cx));
        self.sync_hosts(cx);
        Ok(())
    }

    /// Persist a paired host (named `typed`, else as before, else
    /// `suggested`), connect to it and show it.
    fn save_host(
        &mut self,
        addr: EndpointAddr,
        typed: Option<String>,
        suggested: &str,
        replace: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let (index, _) = self.shared.update(cx, |shared, cx| shared.save_remote(addr, typed, suggested, replace, cx))?;
        self.sync_hosts(cx);
        self.switch_host(index, window, cx);
        Ok(())
    }

    pub(crate) fn render_add_host(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.add_host.as_ref()?;
        let muted = |text: &'static str| div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(text);
        let field = |label: &'static str, input: &Entity<TextField>| {
            div().flex().flex_col().gap(px(6.0)).child(muted(label)).child(
                div()
                    .flex()
                    .items_center()
                    .h(px(34.0))
                    .px(px(10.0))
                    .continuous(8.0)
                    .bg(theme::well_fill())
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
                .when(!primary, |el| el.bg(theme::control_fill()).hover(|s| s.bg(theme::row_fill())))
                .child(label)
        };
        let tab = |id: &'static str, label: &'static str, mode: Mode| {
            let selected = sheet.mode == mode;
            div()
                .id(id)
                .flex_1()
                .h(px(26.0))
                .flex()
                .items_center()
                .justify_center()
                .continuous(6.0)
                .text_size(px(12.5))
                .cursor_pointer()
                .when(selected, |el| el.bg(theme::control_fill()).font_weight(FontWeight::MEDIUM))
                .when(!selected, |el| el.text_color(rgb(theme::muted())).hover(|s| s.text_color(rgb(theme::foreground()))))
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| this.set_add_mode(mode, window, cx)))
        };
        let code = |index: usize, text: String| {
            let copied = sheet.copied == Some(index);
            let copy = text.clone();
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .h(px(34.0))
                .pl(px(10.0))
                .pr(px(4.0))
                .continuous(8.0)
                .bg(theme::well_fill())
                .border_1()
                .border_color(rgb(theme::border()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family(crate::fonts::FALLBACK_FONT)
                        .text_size(px(11.5))
                        .child(text),
                )
                .child(
                    div()
                        .id(("add-host-copy", index))
                        .px(px(10.0))
                        .py(px(4.0))
                        .continuous(6.0)
                        .text_size(px(12.0))
                        .cursor_pointer()
                        .bg(theme::control_fill())
                        .hover(|s| s.bg(theme::row_fill()))
                        .child(if copied { "Copied" } else { "Copy" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            if let Some(sheet) = &mut this.add_host {
                                sheet.copied = Some(index);
                            }
                            cx.notify();
                        })),
                )
        };
        let title = if sheet.replace.is_some() { "Pair Again" } else { "Add Machine" };
        let mut card = div()
            .id("add-host")
            .track_focus(&sheet.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_add_host(window, cx);
                }
            }))
            .occlude()
            .w(px(460.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .p(px(20.0))
            .continuous(14.0)
            .bg(theme::overlay_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_xl()
            .font_family(theme::ui_font())
            .text_size(px(13.0))
            .child(div().text_size(px(15.0)).font_weight(FontWeight::BOLD).child(title));
        if sheet.replace.is_none() {
            card = card.child(
                div()
                    .flex()
                    .gap(px(2.0))
                    .p(px(2.0))
                    .continuous(8.0)
                    .bg(theme::well_fill())
                    .child(tab("add-host-mode-ticket", "Paste ticket", Mode::Ticket))
                    .child(tab("add-host-mode-join", "Run on the other machine", Mode::Join)),
            );
        }
        let error = sheet.error.clone().map(|e| div().text_size(px(12.0)).text_color(rgb(theme::danger())).child(e));
        let footer = div().flex().flex_row().items_center().justify_end().gap(px(8.0));
        let card = match sheet.mode {
            Mode::Ticket => card
                .child(muted("On the other machine: Settings › Server › Pair a device…"))
                .child(field("Ticket", &sheet.ticket))
                .child(field("Name", &sheet.name))
                .children(error)
                .child(
                    footer
                        .when(sheet.busy, |el| el.child(div().flex_1().text_size(px(12.0)).text_color(rgb(theme::muted())).child("Pairing…")))
                        .child(
                            button("add-host-cancel", "Cancel", false).on_click(cx.listener(|this, _, window, cx| this.close_add_host(window, cx))),
                        )
                        .child(
                            button("add-host-save", "Pair", true)
                                .when(sheet.busy, |el| el.opacity(0.5))
                                .on_click(cx.listener(|this, _, window, cx| this.save_add_host(window, cx))),
                        ),
                ),
            Mode::Join => {
                let left = sheet.expires.map(|at| at.saturating_duration_since(Instant::now()));
                let countdown = left.and_then(expiry_text);
                let card = match &sheet.invite {
                    Some(_) if countdown.is_none() => card.child(muted("Invite expired.")),
                    Some(invite) => {
                        let [install, short] = join_commands(&invite.ticket);
                        card.child(muted("On the other machine, run:"))
                            .child(code(0, install))
                            .child(muted("Already has Silo:"))
                            .child(code(1, short))
                    }
                    None => card.when(sheet.busy, |el| el.child(muted("Creating invite…"))),
                };
                let expired = sheet.invite.is_some() && countdown.is_none();
                let status = countdown.map(|c| format!("Waiting for it to join… {c}"));
                card.children(error).child(
                    footer
                        .children(status.map(|s| div().flex_1().text_size(px(12.0)).text_color(rgb(theme::muted())).child(s)))
                        .child(button("add-host-close", "Close", false).on_click(cx.listener(|this, _, window, cx| this.close_add_host(window, cx))))
                        .when(expired, |el| {
                            el.child(
                                button("add-host-new-invite", "New invite", true).on_click(cx.listener(|this, _, _, cx| this.new_join_invite(cx))),
                            )
                        }),
                )
            }
        };
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
        assert_eq!(pair_error(&silo_client::IncompatibleProtocol { message }.into()), "That machine runs an incompatible Silo.");
        assert_eq!(pair_error(&anyhow::anyhow!("dial server")), "Can't reach that machine.");
        assert!(PairTicket::decode("silo1notbase32!").is_err());
    }

    #[test]
    fn default_name_is_hostname() {
        assert_eq!(host_name("  ", "mini.local"), "mini.local");
        assert_eq!(host_name(" mini ", "mini.local"), "mini");
        assert_eq!(host_name("", ""), "silo host");
    }

    #[test]
    fn invite_counts_down() {
        assert_eq!(expiry_text(Duration::from_secs(900)).as_deref(), Some("Expires in 15 min."));
        assert_eq!(expiry_text(Duration::from_secs(61)).as_deref(), Some("Expires in 2 min."));
        assert_eq!(expiry_text(Duration::from_secs(59)).as_deref(), Some("Expires in 59s."));
        assert_eq!(expiry_text(Duration::from_millis(400)), None, "expired");
    }

    #[test]
    fn join_commands_carry_the_ticket() {
        let [install, short] = join_commands("silo1abc");
        assert_eq!(install, "curl -fsSL https://nitishxyz.github.io/silo/install.sh | sh -s -- join silo1abc");
        assert_eq!(short, "silo join silo1abc");
    }
}
