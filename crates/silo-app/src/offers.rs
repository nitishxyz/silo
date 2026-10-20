//! Machines offering to link: a `host.offered` event from any connected
//! host. A terminal offer (`silo` printed it, e.g. over SSH) becomes a
//! card in the active window, saying where it came from; a join offer for
//! an invite this app created (Add Machine… › Run on the other machine),
//! sent to this Mac's server, is linked at once.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use gpui::{AnyElement, Context, FontWeight, IntoElement, Window, div, prelude::*, px, rgb};
use silo_client::transport::PairTicket;
use silo_proto::{
    control::{HostOffer, Method, OfferSource},
    model::{SessionSummary, SessionView},
};

use crate::{
    hosts_state::HostsState,
    theme::{self, Continuous as _},
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

/// How long a terminal offer's ticket (and so its card) lasts.
pub const TERMINAL_TTL: Duration = Duration::from_secs(120);

/// An offer waiting for Connect or Dismiss.
pub struct OfferCard {
    pub offer: HostOffer,
    pub arrived: Instant,
    /// Where it was printed: "session › tab", plus the host if remote.
    pub from: Option<String>,
    pub busy: bool,
    pub error: Option<&'static str>,
}

#[derive(Default)]
pub struct Offers {
    pub cards: Vec<OfferCard>,
    /// Join invites this app created, not yet redeemed.
    pub invites: HashSet<String>,
    /// Tickets dismissed: not offered again (a newer ticket is).
    dismissed: HashSet<String>,
    /// Endpoints being paired.
    linking: HashSet<String>,
}

/// What to do with an offer.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Ignore,
    Show,
    /// A newer ticket for the machine of this card.
    Replace(usize),
    Accept,
}

impl Offers {
    /// `known`: EndpointIds of this Mac's server and of saved hosts that
    /// are still paired. `from_local`: sent by this Mac's server.
    pub fn verdict(&self, offer: &HostOffer, known: &[String], from_local: bool, now: Instant) -> Verdict {
        let id = &offer.endpoint_id;
        if known.contains(id) || self.linking.contains(id) {
            return Verdict::Ignore;
        }
        match &offer.source {
            OfferSource::Join { invite_id } if from_local && self.invites.contains(invite_id) => Verdict::Accept,
            // Someone else's invite (or server): theirs to accept. A newer
            // server's sources are not understood.
            OfferSource::Join { .. } | OfferSource::Unknown => Verdict::Ignore,
            OfferSource::Terminal { .. } if self.dismissed.contains(&offer.ticket) => Verdict::Ignore,
            OfferSource::Terminal { .. } => match self.cards.iter().position(|c| c.offer.endpoint_id == *id) {
                Some(i) if self.cards[i].offer.ticket == offer.ticket && !expired(&self.cards[i], now) => Verdict::Ignore,
                Some(i) if self.cards[i].busy => Verdict::Ignore,
                Some(i) => Verdict::Replace(i),
                None => Verdict::Show,
            },
        }
    }

    /// Drop cards whose ticket has expired; whether any were.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.cards.len();
        self.cards.retain(|c| c.busy || !expired(c, now));
        self.cards.len() != before
    }
}

fn expired(card: &OfferCard, now: Instant) -> bool {
    now.duration_since(card.arrived) >= TERMINAL_TTL
}

/// "session › tab" of the window holding `block` in `view`: the tab's name,
/// else the block's title.
pub fn source_label(view: &SessionView, block: &str) -> Option<String> {
    let window = view.session.windows.iter().find(|w| w.layout.contains(block) || w.floats.iter().any(|f| f.id == block))?;
    let tab = window.name.clone().filter(|n| !n.is_empty()).or_else(|| view.blocks.iter().find(|b| b.id == block).map(|b| b.title.clone()));
    Some(match tab.filter(|t| !t.is_empty()) {
        Some(tab) => format!("{} › {tab}", view.session.name),
        None => view.session.name.clone(),
    })
}

/// The first 8 characters of an EndpointId.
pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// A linked (or failed) offer, or a host added outside the app, for every
/// window.
pub enum OfferEvent {
    Added { host: usize, name: String, invite: Option<String> },
    Failed { error: &'static str, invite: Option<String> },
}

fn invite_of(offer: &HostOffer) -> Option<String> {
    match &offer.source {
        OfferSource::Join { invite_id } => Some(invite_id.clone()),
        OfferSource::Terminal { .. } | OfferSource::Unknown => None,
    }
}

impl HostsState {
    /// A `host.offered` event from `host`.
    pub(crate) fn host_offered(&mut self, host: usize, offer: &HostOffer, cx: &mut Context<Self>) {
        let local = self.entries().first().and_then(|h| h.control.as_ref()).map(|c| c.hello().server.endpoint_id.clone());
        let known: Vec<String> = self
            .entries()
            .iter()
            .filter(|h| !h.removed && h.state != crate::hosts::HostState::NotPaired)
            .filter_map(|h| h.info.transport.remote_id())
            .chain(local.filter(|id| !id.is_empty()))
            .collect();
        let now = Instant::now();
        let card = || OfferCard { offer: offer.clone(), arrived: now, from: None, busy: false, error: None };
        match self.offers.verdict(offer, &known, host == 0, now) {
            Verdict::Ignore => return,
            Verdict::Show => self.offers.cards.push(card()),
            Verdict::Replace(i) => self.offers.cards[i] = card(),
            Verdict::Accept => {
                if let Some(invite) = invite_of(offer) {
                    self.offers.invites.remove(&invite);
                }
                return self.link(offer.clone(), cx);
            }
        }
        if let OfferSource::Terminal { block_id } = &offer.source {
            self.resolve_source(host, block_id.clone(), offer.ticket.clone(), cx);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TERMINAL_TTL).await;
            let _ = this.update(cx, |this, cx| {
                if this.offers.expire(Instant::now()) {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Find the session and tab of the block that printed an offer.
    fn resolve_source(&mut self, host: usize, block: String, ticket: String, cx: &mut Context<Self>) {
        let Some(control) = self.entries()[host].control.clone() else { return };
        let on = (host != 0).then(|| self.entries()[host].info.name.clone());
        let task = self.rt.spawn(async move {
            let sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await?;
            for s in sessions {
                let view: SessionView = control.call(Method::SessionView { session_id: s.id }).await?;
                if let Some(label) = source_label(&view, &block) {
                    return anyhow::Ok(Some(label));
                }
            }
            Ok(None)
        });
        cx.spawn(async move |this, cx| {
            let label = match task.await.map_err(anyhow::Error::from).and_then(|r| r) {
                Ok(label) => label,
                Err(error) => return tracing::debug!("offer source: {error:#}"),
            };
            let from = match (label, on) {
                (Some(label), Some(host)) => format!("{label} on {host}"),
                (Some(label), None) => label,
                (None, Some(host)) => host,
                (None, None) => return,
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(card) = this.offers.cards.iter_mut().find(|c| c.offer.ticket == ticket) {
                    card.from = Some(from);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn connect_offer(&mut self, endpoint: &str, cx: &mut Context<Self>) {
        let Some(card) = self.offers.cards.iter_mut().find(|c| c.offer.endpoint_id == endpoint) else { return };
        if card.busy {
            return;
        }
        (card.busy, card.error) = (true, None);
        let offer = card.offer.clone();
        self.link(offer, cx);
        cx.notify();
    }

    pub(crate) fn dismiss_offer(&mut self, endpoint: &str, cx: &mut Context<Self>) {
        let tickets: Vec<String> = self.offers.cards.iter().filter(|c| c.offer.endpoint_id == endpoint).map(|c| c.offer.ticket.clone()).collect();
        self.offers.cards.retain(|c| c.offer.endpoint_id != endpoint);
        self.offers.dismissed.extend(tickets);
        cx.notify();
    }

    /// Pair with the offered server, then save and connect it.
    fn link(&mut self, offer: HostOffer, cx: &mut Context<Self>) {
        let Ok(ticket) = PairTicket::decode(&offer.ticket) else {
            return self.link_failed(&offer, "Invalid ticket.", cx);
        };
        self.offers.linking.insert(offer.endpoint_id.clone());
        let task = crate::add_host::pair(&self.rt, self.home.clone(), ticket);
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                this.offers.linking.remove(&offer.endpoint_id);
                let (addr, hostname) = match result {
                    Ok(paired) => paired,
                    Err(error) => {
                        tracing::info!("linking {} failed: {error:#}", offer.name);
                        return this.link_failed(&offer, crate::add_host::pair_error(&error), cx);
                    }
                };
                let suggested = crate::add_host::host_name(&offer.name, &hostname);
                let id = addr.id.to_string();
                let replace = this.entries().iter().position(|h| !h.removed && h.info.transport.remote_id().as_deref() == Some(id.as_str()));
                match this.save_remote(addr, None, &suggested, replace, cx) {
                    Ok((host, name)) => {
                        this.offers.cards.retain(|c| c.offer.endpoint_id != offer.endpoint_id);
                        cx.emit(OfferEvent::Added { host, name, invite: invite_of(&offer) });
                        cx.notify();
                    }
                    Err(error) => {
                        tracing::warn!("save host: {error}");
                        this.link_failed(&offer, "Could not save the machine.", cx);
                    }
                }
            });
        })
        .detach();
    }

    fn link_failed(&mut self, offer: &HostOffer, error: &'static str, cx: &mut Context<Self>) {
        if let Some(card) = self.offers.cards.iter_mut().find(|c| c.offer.endpoint_id == offer.endpoint_id) {
            (card.busy, card.error) = (false, Some(error));
        }
        if let Some(invite) = invite_of(offer) {
            cx.emit(OfferEvent::Failed { error, invite: Some(invite) });
        }
        cx.notify();
    }
}

/// Whether `window` is the frontmost Silo window.
fn frontmost(window: &Window, cx: &gpui::App) -> bool {
    let silo = |w: &gpui::AnyWindowHandle| w.downcast::<Workspace>().is_some();
    let front = cx.window_stack().and_then(|stack| stack.into_iter().find(silo)).or_else(|| cx.windows().into_iter().find(silo));
    front.is_some_and(|w| w.window_id() == window.window_handle().window_id())
}

impl Workspace {
    /// A linked or failed offer: the window waiting on its invite (or
    /// else the frontmost one, even while another app is active) says
    /// so, and shows the added host.
    pub(crate) fn offer_event(&mut self, event: &OfferEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_hosts(cx);
        let (invite, message) = match event {
            OfferEvent::Added { name, invite, .. } => (invite, format!("{name} added")),
            OfferEvent::Failed { error, invite } => (invite, (*error).to_owned()),
        };
        let announce = if invite.is_some() && self.join_invite() == invite.as_deref() {
            match event {
                OfferEvent::Added { .. } => self.close_add_host(window, cx),
                OfferEvent::Failed { error, .. } => return self.sheet_error(*error, cx),
            }
            true
        } else {
            frontmost(window, cx)
        };
        if announce {
            if let OfferEvent::Added { host, .. } = event {
                self.switch_host(*host, window, cx);
            }
            self.flash(message, cx);
        }
        cx.notify();
    }

    /// Offer cards, in the active window only.
    pub(crate) fn render_offers(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !window.is_window_active() {
            return None;
        }
        let shared = self.shared.read(cx);
        if shared.offers.cards.is_empty() {
            return None;
        }
        let button = |id: (&'static str, usize), label: &'static str, primary: bool| {
            div()
                .id(id)
                .h(px(26.0))
                .px(px(12.0))
                .flex()
                .items_center()
                .continuous(13.0)
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .when(primary, |el| el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())).hover(|s| s.opacity(0.88)))
                .when(!primary, |el| el.bg(theme::control_fill()).hover(|s| s.bg(theme::row_fill())))
                .child(label)
        };
        let cards = shared.offers.cards.iter().enumerate().map(|(i, card)| {
            let (connect, dismiss) = (card.offer.endpoint_id.clone(), card.offer.endpoint_id.clone());
            div()
                .id(("offer", i))
                .occlude()
                .w(px(300.0))
                .flex()
                .flex_col()
                .gap(px(10.0))
                .p(px(14.0))
                .continuous(12.0)
                .bg(theme::overlay_fill())
                .border_1()
                .border_color(rgb(theme::border()))
                .shadow_xl()
                .text_size(px(13.0))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("{} is running Silo", card.offer.name)))
                .child(div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(match &card.from {
                    Some(from) => format!("From {from} · ID {}", short_id(&card.offer.endpoint_id)),
                    None => format!("From a terminal · ID {}", short_id(&card.offer.endpoint_id)),
                }))
                .children(card.error.map(|e| div().text_size(px(12.0)).text_color(rgb(theme::danger())).child(e)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap(px(8.0))
                        .when(card.busy, |el| el.child(div().flex_1().text_size(px(12.0)).text_color(rgb(theme::muted())).child("Connecting…")))
                        .child(button(("offer-dismiss", i), "Dismiss", false).on_click(cx.listener(move |this, _, _, cx| {
                            this.shared.update(cx, |shared, cx| shared.dismiss_offer(&dismiss, cx));
                        })))
                        .child(button(("offer-connect", i), "Connect", true).when(card.busy, |el| el.opacity(0.5)).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.shared.update(cx, |shared, cx| shared.connect_offer(&connect, cx));
                            },
                        ))),
                )
        });
        let cards: Vec<_> = cards.collect();
        Some(div().absolute().top(px(TITLEBAR_HEIGHT + 8.0)).right(px(12.0)).flex().flex_col().gap(px(8.0)).children(cards).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(id: &str, ticket: &str, source: OfferSource) -> HostOffer {
        HostOffer { ticket: ticket.into(), name: "mini".into(), endpoint_id: id.into(), source }
    }

    fn terminal(id: &str, ticket: &str) -> HostOffer {
        offer(id, ticket, OfferSource::Terminal { block_id: "b1".into() })
    }

    fn join(id: &str, invite: &str) -> HostOffer {
        offer(id, "silo1j", OfferSource::Join { invite_id: invite.into() })
    }

    fn card(offer: HostOffer, arrived: Instant) -> OfferCard {
        OfferCard { offer, arrived, from: None, busy: false, error: None }
    }

    #[test]
    fn terminal_offers_show_once_per_ticket_and_never_auto_accept() {
        let now = Instant::now();
        let mut offers = Offers::default();
        assert_eq!(offers.verdict(&terminal("e1", "t1"), &[], true, now), Verdict::Show);
        offers.cards.push(card(terminal("e1", "t1"), now));
        assert_eq!(offers.verdict(&terminal("e1", "t1"), &[], true, now), Verdict::Ignore, "already shown");
        assert_eq!(offers.verdict(&terminal("e1", "t2"), &[], true, now), Verdict::Replace(0), "a newer ticket replaces it");
        // Dismissing hides that ticket only.
        offers.dismissed.insert("t3".into());
        assert_eq!(offers.verdict(&terminal("e2", "t3"), &[], true, now), Verdict::Ignore, "dismissed");
        assert_eq!(offers.verdict(&terminal("e2", "t4"), &[], true, now), Verdict::Show, "newer ticket after dismiss");
        // Even with a known invite id lookalike, a terminal offer only shows.
        offers.invites.insert("b1".into());
        assert_eq!(offers.verdict(&terminal("e3", "t5"), &[], true, now), Verdict::Show);
    }

    #[test]
    fn cards_expire_with_their_ticket() {
        let start = Instant::now();
        let mut offers = Offers::default();
        offers.cards.push(card(terminal("e1", "t1"), start));
        let later = start + TERMINAL_TTL;
        // The same ticket again, once expired, shows afresh.
        assert_eq!(offers.verdict(&terminal("e1", "t1"), &[], true, later), Verdict::Replace(0));
        assert!(!offers.expire(start + Duration::from_secs(60)));
        assert!(offers.expire(later));
        assert!(offers.cards.is_empty());
        // A card being connected stays until linking ends.
        offers.cards.push(OfferCard { busy: true, ..card(terminal("e1", "t1"), start) });
        assert!(!offers.expire(later));
        assert_eq!(offers.verdict(&terminal("e1", "t2"), &[], true, later), Verdict::Ignore, "busy");
    }

    #[test]
    fn this_mac_and_paired_hosts_are_ignored() {
        let (offers, now) = (Offers::default(), Instant::now());
        let known = vec!["local".to_owned(), "e1".to_owned()];
        assert_eq!(offers.verdict(&terminal("local", "t"), &known, true, now), Verdict::Ignore, "this Mac's own server");
        assert_eq!(offers.verdict(&terminal("e1", "t"), &known, true, now), Verdict::Ignore);
        assert_eq!(offers.verdict(&terminal("e2", "t"), &known, false, now), Verdict::Show, "from any host");
    }

    #[test]
    fn only_our_join_invites_on_this_mac_auto_accept() {
        let now = Instant::now();
        let mut offers = Offers::default();
        offers.invites.insert("inv1".into());
        assert_eq!(offers.verdict(&join("e1", "inv1"), &[], true, now), Verdict::Accept);
        assert_eq!(offers.verdict(&join("e1", "inv1"), &[], false, now), Verdict::Ignore, "sent to another host");
        assert_eq!(offers.verdict(&join("e1", "other"), &[], true, now), Verdict::Ignore, "the CLI's invite");
        assert_eq!(offers.verdict(&join("e1", "inv1"), &["e1".into()], true, now), Verdict::Ignore, "already paired");
        offers.linking.insert("e1".into());
        assert_eq!(offers.verdict(&join("e1", "inv1"), &[], true, now), Verdict::Ignore, "already linking");
    }

    #[test]
    fn source_is_session_and_tab() {
        let mut view: SessionView = serde_json::from_value(serde_json::json!({
            "session": {"id": "s1", "name": "work", "windows": [
                {"id": "w1", "name": null, "layout": silo_proto::model::Layout::block("b1"), "focused": "b1", "floats": [{"id": "b2", "rect": {"x": 0.0, "y": 0.0, "w": 0.5, "h": 0.5}}]},
            ], "active_window": "w1", "revision": 1, "created_at": 0},
            "blocks": [{"id": "b1", "session_id": "s1", "kind": "terminal", "title": "ssh mini", "size": {"rows": 24, "columns": 80}}],
        }))
        .unwrap();
        assert_eq!(source_label(&view, "b1").as_deref(), Some("work › ssh mini"));
        assert_eq!(source_label(&view, "b2").as_deref(), Some("work"), "floating, no title");
        assert_eq!(source_label(&view, "b9"), None);
        view.session.windows[0].name = Some("build".into());
        assert_eq!(source_label(&view, "b1").as_deref(), Some("work › build"));
        assert_eq!(short_id("b470dc365e04b1a5"), "b470dc36");
    }
}
