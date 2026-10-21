//! Silo-to-Silo sharing (`docs/PHASE7.md` §8). "Share Session…" / "Share
//! Pane…" make a link (`grant.create {kind: link}`), show it with its QR
//! code and expiry, and stop links (`grant.revoke`). "Open Shared Link…"
//! (or a link pasted in Add Machine) adds the shared session or block as a
//! host marked shared, kept in `shares.json` until removed.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use gpui::{AnyElement, ClipboardItem, Context, Entity, FontWeight, IntoElement, KeyDownEvent, Subscription, Window, div, prelude::*, px, rgb};
use serde::{Deserialize, Serialize};
use silo_client::{Control, transport::ShareTicket};
use silo_proto::{
    access::{Grant, Rights, Scope, SecretKind, Who},
    control::{BlockView, GrantCreated, GrantKind, Grants, Method},
    model::{Layout, SessionInfo, SessionView, WindowInfo},
};

use crate::theme::Continuous as _;
use crate::{
    status::Status,
    text_field::{FieldEvent, TextField},
    theme,
    workspace::Workspace,
};

/// A shared host whose link was refused: revoked or expired.
pub const ENDED: &str = "Link revoked or expired";

/// Link lifetimes offered, the first the default (the server's maximum is
/// 30 days).
const TTLS: [(&str, u64); 3] = [("1 day", 24 * 3600), ("1 hour", 3600), ("7 days", 7 * 24 * 3600)];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SharesFile {
    #[serde(default)]
    shares: Vec<Saved>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Saved {
    name: String,
    /// The `silos1…` ticket (it holds the link's secret).
    ticket: String,
}

fn file(home: &Path) -> PathBuf {
    home.join("shares.json")
}

/// A shared host's key and cache identity: `share:` and a digest of the
/// link's secret, so two links of one server are two hosts.
#[must_use]
pub fn key(ticket: &ShareTicket) -> String {
    format!("share:{}", &silo_proto::access::secret_hash(&ticket.secret)[..16])
}

fn read(home: &Path) -> SharesFile {
    std::fs::read(file(home)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Written whole, owner-only: the tickets are bearer secrets.
fn write(home: &Path, shares: &SharesFile) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(home)?;
    let tmp = home.join(format!("shares.json.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    std::io::Write::write_all(&mut out, &serde_json::to_vec_pretty(shares)?)?;
    std::fs::rename(tmp, file(home))?;
    Ok(())
}

/// The kept shared links, with their names.
#[must_use]
pub fn load(home: &Path) -> Vec<(String, ShareTicket)> {
    read(home).shares.into_iter().filter_map(|s| Some((s.name, ShareTicket::decode(&s.ticket).ok()?))).collect()
}

/// Keep `ticket` as `name` (replacing the same link).
///
/// # Errors
/// The file can't be written.
pub fn save(home: &Path, name: &str, ticket: &ShareTicket) -> anyhow::Result<()> {
    let mut shares = read(home);
    let id = key(ticket);
    shares.shares.retain(|s| ShareTicket::decode(&s.ticket).is_ok_and(|t| key(&t) != id));
    shares.shares.push(Saved { name: name.to_owned(), ticket: ticket.encode() });
    write(home, &shares)
}

/// Forget the link with key `id`.
///
/// # Errors
/// The file can't be written.
pub fn remove(home: &Path, id: &str) -> anyhow::Result<()> {
    let mut shares = read(home);
    shares.shares.retain(|s| ShareTicket::decode(&s.ticket).is_ok_and(|t| key(&t) != id));
    write(home, &shares)
}

/// The block a block link shows, if `control` came in by one.
#[must_use]
pub fn shared_block(control: &Control) -> Option<String> {
    match &control.hello().granted.as_ref()?.scope {
        Scope::Block { block_id } => Some(block_id.clone()),
        _ => None,
    }
}

/// What a block link shows, as a session of one tab with that block: a
/// block link sees no session (`session.view` is not found for it).
///
/// # Errors
/// RPC failure.
pub async fn block_view(control: &Control, block: String) -> anyhow::Result<SessionView> {
    let view: BlockView = control.call(Method::BlockView { block_id: block.clone() }).await?;
    let window = WindowInfo {
        id: format!("share-{block}"),
        name: None,
        layout: Layout::block(block.clone()),
        focused: block.clone(),
        zoomed: false,
        floats: Vec::new(),
    };
    let session = SessionInfo {
        id: format!("share-{block}"),
        name: crate::pane::block_title(&view.block),
        active_window: window.id.clone(),
        windows: vec![window],
        revision: 0,
        created_at: 0,
    };
    Ok(SessionView { session, blocks: vec![view.block], status: view.status.into_iter().collect(), watchers: Vec::new() })
}

/// "Expires in 3 h" for a link expiring at unix second `at`.
#[must_use]
pub fn expires_text(at: Option<u64>, now: u64) -> String {
    let Some(at) = at else { return "Until stopped".into() };
    match at.saturating_sub(now) {
        0 => "Expired".into(),
        left @ ..3600 => format!("Expires in {} min", left.div_ceil(60)),
        left @ ..172_800 => format!("Expires in {} h", left.div_ceil(3600)),
        left => format!("Expires in {} days", left / 86_400),
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn rights_word(rights: Rights) -> &'static str {
    match rights {
        Rights::View => "view",
        _ => "interact",
    }
}

/// A link of `scope` (and only links).
fn is_link_of(grant: &Grant, scope: &Scope) -> bool {
    matches!(grant.who, Who::Secret { kind: SecretKind::Link, .. }) && grant.scope == *scope
}

/// The link just made, shown once.
struct Made {
    id: String,
    link: String,
    relay: bool,
}

pub struct ShareSheet {
    scope: Scope,
    title: &'static str,
    rights: Rights,
    ttl: usize,
    label: Entity<TextField>,
    made: Option<Made>,
    /// This scope's links (when the host lets this device list grants).
    links: Vec<Grant>,
    copied: bool,
    busy: bool,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl Workspace {
    /// Share the shown session, or (`pane`) the focused block.
    pub(crate) fn open_share_sheet(&mut self, pane: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.hosts.get(self.active_host).is_some_and(|h| h.info.transport.is_shared()) {
            return self.flash(Status::info("A shared link can't be shared again"), cx);
        }
        let scope = if pane {
            let Some(block_id) = self.focused_block(window, cx) else { return };
            Scope::Block { block_id }
        } else {
            let Some(view) = &self.view else { return };
            Scope::Session { session_id: view.session.id.clone() }
        };
        let label = cx.new(|cx| TextField::new("Who it's for (optional)", cx));
        let subs = vec![cx.subscribe_in(&label, window, |this, _, event: &FieldEvent, window, cx| match event {
            FieldEvent::Submit => this.create_link(cx),
            FieldEvent::Cancel => this.close_share_sheet(window, cx),
            _ => {}
        })];
        let focus = label.read(cx).focus.clone();
        focus.focus(window, cx);
        let title = if pane { "Share Pane" } else { "Share Session" };
        self.share = Some(ShareSheet {
            scope,
            title,
            rights: Rights::View,
            ttl: 0,
            label,
            made: None,
            links: Vec::new(),
            copied: false,
            busy: false,
            error: None,
            _subs: subs,
        });
        self.refresh_links(cx);
        cx.notify();
    }

    pub(crate) fn close_share_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.share = None;
        self.refocus_terminal(window, cx);
        cx.notify();
    }

    /// Re-read this scope's links; a host that lets this device make links
    /// but not list them (a paired device) shows the one made here.
    fn refresh_links(&mut self, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        let task = self.rt.spawn(async move { control.call::<Grants>(Method::GrantList {}).await });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(grants)) = task.await else { return };
            let _ = this.update(cx, |this, cx| {
                if let Some(sheet) = &mut this.share {
                    sheet.links = grants.grants.into_iter().filter(|g| is_link_of(g, &sheet.scope)).collect();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn create_link(&mut self, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        let Some(sheet) = self.share.as_mut() else { return };
        if sheet.busy {
            return;
        }
        (sheet.busy, sheet.error, sheet.copied) = (true, None, false);
        let label = Some(sheet.label.read(cx).text.trim().to_owned()).filter(|l| !l.is_empty());
        let method =
            Method::GrantCreate { scope: sheet.scope.clone(), rights: sheet.rights, kind: GrantKind::Link, ttl_secs: Some(TTLS[sheet.ttl].1), label };
        let task = self.rt.spawn(async move { control.call::<GrantCreated>(method).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                let Some(sheet) = &mut this.share else { return };
                sheet.busy = false;
                match result.map(|c| (c.ticket.as_deref().and_then(|t| ShareTicket::decode(t).ok()), c.grant)) {
                    Ok((Some(ticket), grant)) => {
                        sheet.made = Some(Made { id: grant.id.clone(), link: ticket.link(), relay: ticket.has_relay() });
                        sheet.links.retain(|g| g.id != grant.id);
                        sheet.links.push(grant);
                    }
                    Ok((None, _)) => sheet.error = Some("This host can't make links.".into()),
                    Err(error) => {
                        tracing::warn!("grant.create: {error:#}");
                        sheet.error = Some(if error.downcast_ref::<silo_proto::control::RpcError>().is_some_and(|e| e.code == "forbidden") {
                            "Not allowed on this host.".into()
                        } else {
                            "Could not create a link.".into()
                        });
                    }
                }
                this.refresh_links(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Stop Sharing: the link's viewers are disconnected at once.
    fn revoke_link(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        let task = self.rt.spawn({
            let id = id.clone();
            async move { control.call_value(Method::GrantRevoke { id }).await }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                let Some(sheet) = &mut this.share else { return };
                match result {
                    Ok(_) => {
                        sheet.links.retain(|g| g.id != id);
                        if sheet.made.as_ref().is_some_and(|m| m.id == id) {
                            sheet.made = None;
                        }
                    }
                    Err(error) => {
                        tracing::warn!("grant.revoke: {error:#}");
                        sheet.error = Some("Could not stop sharing.".into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Open a received link: check it, then add its host (marked shared)
    /// and show it.
    pub(crate) fn open_share(&mut self, ticket: ShareTicket, window: &mut Window, cx: &mut Context<Self>) {
        let home = self.home.clone();
        let client = crate::conn::client_name();
        let probe = ticket.clone();
        self.status = Some(Status::progress("Opening shared link…"));
        let task = self.rt.spawn(async move {
            let net = crate::remote::net(&home).await?;
            let (control, _) = Control::connect_shared(net, &probe, None, &client).await?;
            anyhow::Ok(control.hello().server.hostname.clone())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(hostname) => {
                        let name = crate::add_host::host_name("", &hostname);
                        match this.shared.update(cx, |shared, cx| shared.save_share(name, ticket, cx)) {
                            Ok(index) => {
                                this.sync_hosts(cx);
                                this.switch_host(index, window, cx);
                            }
                            Err(error) => this.status = Some(Status::error(error)),
                        }
                    }
                    Err(error) => {
                        tracing::info!("open shared link: {error:#}");
                        this.status = Some(Status::error(open_error(&error)));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn render_share(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.share.as_ref()?;
        let muted = |text: String| div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(text);
        let button = |id: gpui::ElementId, label: &'static str, primary: bool| {
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
        let segment = |id: &'static str, label: &'static str, selected: bool| {
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
        };
        let well = || div().flex().gap(px(2.0)).p(px(2.0)).continuous(8.0).bg(theme::well_fill());
        let rights = well()
            .child(segment("share-view", "View", sheet.rights == Rights::View).on_click(cx.listener(|this, _, _, cx| {
                if let Some(sheet) = &mut this.share {
                    sheet.rights = Rights::View;
                }
                cx.notify();
            })))
            .child(segment("share-interact", "Interact", sheet.rights == Rights::Interact).on_click(cx.listener(|this, _, _, cx| {
                if let Some(sheet) = &mut this.share {
                    sheet.rights = Rights::Interact;
                }
                cx.notify();
            })));
        let mut ttls = well();
        for (index, (label, _)) in TTLS.iter().enumerate() {
            ttls = ttls.child(segment(["share-ttl-0", "share-ttl-1", "share-ttl-2"][index], label, sheet.ttl == index).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(sheet) = &mut this.share {
                        sheet.ttl = index;
                    }
                    cx.notify();
                },
            )));
        }
        let field = div()
            .flex()
            .items_center()
            .h(px(34.0))
            .px(px(10.0))
            .continuous(8.0)
            .bg(theme::well_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .child(sheet.label.clone());
        let now = now_secs();
        let made = sheet.made.as_ref().map(|made| {
            let link = made.link.clone();
            let code = div()
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
                        .child(made.link.clone()),
                )
                .child(
                    div()
                        .id("share-copy")
                        .px(px(10.0))
                        .py(px(4.0))
                        .continuous(6.0)
                        .text_size(px(12.0))
                        .cursor_pointer()
                        .bg(theme::control_fill())
                        .hover(|s| s.bg(theme::row_fill()))
                        .child(if sheet.copied { "Copied" } else { "Copy" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                            if let Some(sheet) = &mut this.share {
                                sheet.copied = true;
                            }
                            cx.notify();
                        })),
                );
            div()
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(code)
                .when(!made.relay, |el| el.child(muted("Works only for Silo on the same network: this host has no relay.".into())))
                .children(crate::qr::render(&made.link).map(|qr| div().flex().justify_center().child(qr)))
        });
        let links = sheet.links.iter().map(|grant| {
            let id = grant.id.clone();
            let who = if grant.label.is_empty() { "Link".to_owned() } else { grant.label.clone() };
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).child(format!(
                    "{who} · {} · {}",
                    rights_word(grant.rights),
                    expires_text(grant.expires_at, now)
                )))
                .child(
                    button(gpui::ElementId::Name(format!("share-stop-{}", grant.id).into()), "Stop Sharing", false)
                        .on_click(cx.listener(move |this, _, _, cx| this.revoke_link(id.clone(), cx))),
                )
        });
        let error = sheet.error.clone().map(|e| div().text_size(px(12.0)).text_color(rgb(theme::danger())).child(e));
        let card = div()
            .id("share")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_share_sheet(window, cx);
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
            .child(div().text_size(px(15.0)).font_weight(FontWeight::BOLD).child(sheet.title))
            .child(rights)
            .when(sheet.rights == Rights::Interact, |el| {
                el.child(div().text_size(px(12.0)).text_color(rgb(theme::danger())).child("Interact on a shell is shell access as you."))
            })
            .child(ttls)
            .child(field)
            .children(made)
            .children(error)
            .when(!sheet.links.is_empty(), |el| el.child(muted("Links".into())).children(links))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .when(sheet.busy, |el| el.child(div().flex_1().text_size(px(12.0)).text_color(rgb(theme::muted())).child("Creating link…")))
                    .child(
                        button("share-close".into(), "Close", false).on_click(cx.listener(|this, _, window, cx| this.close_share_sheet(window, cx))),
                    )
                    .child(
                        button("share-create".into(), "Create Link", true)
                            .when(sheet.busy, |el| el.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| this.create_link(cx))),
                    ),
            );
        Some(
            div()
                .id("share-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(70.0))
                .pb(px(24.0))
                // The link, its QR code and the links list outgrow a small window.
                .overflow_y_scroll()
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.close_share_sheet(window, cx)))
                .child(card)
                .into_any_element(),
        )
    }
}

/// Short message for a link that did not open.
pub(crate) fn open_error(error: &anyhow::Error) -> &'static str {
    match crate::conn::classify(error) {
        crate::conn::Failure::NotPaired => "Link revoked or expired.",
        crate::conn::Failure::Incompatible => "That machine runs an incompatible Silo.",
        crate::conn::Failure::Upgrading => "That machine is updating. Try again.",
        crate::conn::Failure::Unreachable => "Can't reach that machine.",
    }
}

#[cfg(test)]
mod tests {
    use silo_client::transport::EndpointAddr;

    use super::*;

    fn ticket(dir: &Path, byte: u8) -> ShareTicket {
        let id = silo_client::client_identity(dir).unwrap().id();
        ShareTicket::new(&EndpointAddr::new(id), [byte; 32], silo_proto::VT)
    }

    #[test]
    fn shares_round_trip_privately_by_link() {
        let home = crate::hosts::tests::temp("shares");
        let (a, b) = (ticket(&home.join("a"), 1), ticket(&home.join("a"), 2));
        assert_ne!(key(&a), key(&b), "two links of one server are two hosts");
        save(&home, "mini", &a).unwrap();
        save(&home, "mini", &b).unwrap();
        save(&home, "mini again", &a).unwrap();
        let loaded = load(&home);
        assert_eq!(loaded.iter().map(|(n, t)| (n.as_str(), key(t))).collect::<Vec<_>>(), [("mini", key(&b)), ("mini again", key(&a))]);
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(file(&home)).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600, "tickets are secrets");
        remove(&home, &key(&b)).unwrap();
        assert_eq!(load(&home).len(), 1);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn expiry_reads_short() {
        assert_eq!(expires_text(None, 0), "Until stopped");
        assert_eq!(expires_text(Some(100), 100), "Expired");
        assert_eq!(expires_text(Some(1000 + 90), 1000), "Expires in 2 min");
        assert_eq!(expires_text(Some(1000 + 86_400), 1000), "Expires in 24 h");
        assert_eq!(expires_text(Some(1000 + 7 * 86_400), 1000), "Expires in 7 days");
    }
}
