//! Approval cards (Phase 7 §9): a server holds an owner action asked for by
//! something other than a Silo app (an agent in a pane, the CLI) until a
//! human allows or denies it here. One card per request, in the key
//! window: who asks, what, and on which host. Cards never show paths.

use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, FontWeight, IntoElement, Window, div, prelude::*, px, rgb};
use silo_proto::{
    control::{Caller, Method},
    text::{MAX_APP, MAX_MESSAGE, MAX_TITLE, untrusted},
};

use crate::theme::Continuous as _;
use crate::{
    hosts_state::HostsState,
    theme,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    pub host: usize,
    pub id: String,
    /// Who asks (the client's label), cleaned; the client chose it.
    pub caller: String,
    /// The caller's pane, `session › block`, if it runs in one; for a
    /// paired device's call, that device (`<name> (CLI)`).
    pub from: Option<String>,
    /// What allowing does, written by the server.
    pub summary: String,
    pub expires: Instant,
    /// Answered here; waiting for the server.
    pub answering: bool,
}

#[derive(Default)]
pub struct Approvals {
    pub cards: Vec<Approval>,
}

/// `text` cleaned as untrusted, without words that look like paths.
fn plain(text: &str, max: usize) -> String {
    untrusted(text, max).split(' ').filter(|w| !w.contains('/') && !w.contains('\\')).collect::<Vec<_>>().join(" ")
}

impl Approval {
    /// The card's first line. The name is the caller's own claim (anything
    /// may call itself "Silo"; the Silo app's own calls are never asked
    /// for), so it is quoted.
    pub fn asks(&self) -> String {
        if self.caller == UNNAMED { format!("{UNNAMED} wants to") } else { format!("\u{201c}{}\u{201d} wants to", self.caller) }
    }
}

const UNNAMED: &str = "A program";

/// How a host names the calling device on that device's own cards (the
/// server's `approval::THIS_DEVICE`). A remote host sends this app only
/// cards from this device, so only those are reworded.
const THIS_DEVICE: &str = "this device";

/// The card's last line: where the call comes from, on which host.
fn place(from: Option<&str>, host: &str, remote: bool) -> String {
    match from {
        Some(from) if remote && from.starts_with(THIS_DEVICE) => format!("From this Mac{} on {host}", &from[THIS_DEVICE.len()..]),
        Some(from) => format!("From {from} on {host}"),
        None => format!("On {host}"),
    }
}

impl Approvals {
    /// `approval.requested` from `host`. A repeated id replaces its card.
    pub fn requested(&mut self, host: usize, id: &str, summary: &str, caller: &Caller, expires_in_ms: u64, now: Instant) {
        self.cards.retain(|c| !(c.host == host && c.id == id));
        let label = plain(&caller.label, MAX_APP);
        self.cards.push(Approval {
            host,
            id: id.to_owned(),
            caller: if label.is_empty() { UNNAMED.to_owned() } else { label },
            from: caller.from.as_deref().map(|f| plain(f, MAX_TITLE)).filter(|f| !f.is_empty()),
            summary: plain(summary, MAX_MESSAGE),
            expires: now + Duration::from_millis(expires_in_ms),
            answering: false,
        });
    }

    /// The card answered, denied or timed out (anywhere): gone.
    pub fn resolved(&mut self, host: usize, id: &str) -> bool {
        let before = self.cards.len();
        self.cards.retain(|c| !(c.host == host && c.id == id));
        self.cards.len() != before
    }

    /// `host` disconnected: its requests failed with it.
    pub fn host_gone(&mut self, host: usize) -> bool {
        let before = self.cards.len();
        self.cards.retain(|c| c.host != host);
        self.cards.len() != before
    }

    /// Cards still answerable at `now`.
    pub fn live(&self, now: Instant) -> impl Iterator<Item = &Approval> {
        self.cards.iter().filter(move |c| c.expires > now)
    }
}

impl HostsState {
    /// Answer approval `id` of `host`.
    pub fn answer_approval(&mut self, host: usize, id: &str, allow: bool, cx: &mut Context<Self>) {
        let Some(control) = self.entries().get(host).and_then(|h| h.control.clone()) else { return };
        let Some(card) = self.approvals.cards.iter_mut().find(|c| c.host == host && c.id == id) else { return };
        card.answering = true;
        cx.notify();
        let id = id.to_owned();
        tracing::info!(host, id, allow, "approval answered");
        let task = self.rt.spawn({
            let id = id.clone();
            async move { control.call_value(Method::ApprovalAnswer { id, allow }).await }
        });
        cx.spawn(async move |this, cx| {
            let failed = !matches!(task.await, Ok(Ok(_)));
            let _ = this.update(cx, |this, cx| {
                if failed {
                    tracing::warn!(id, "approval answer failed");
                }
                // Answered, or no longer answerable: either way it is done.
                this.approvals.resolved(host, &id);
                crate::notifications::remove(vec![crate::notifications::approval_identifier(&this.entries()[host].info.key(), &id)]);
                cx.notify();
            });
        })
        .detach();
    }
}

impl Workspace {
    /// Approval cards, in the key window only.
    pub(crate) fn render_approvals(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !window.is_window_active() {
            return None;
        }
        let shared = self.shared.read(cx);
        let now = Instant::now();
        let cards: Vec<Approval> = shared.approvals.live(now).cloned().collect();
        if cards.is_empty() {
            return None;
        }
        let button = |id: (&'static str, usize), label: &'static str, primary: bool| {
            div()
                .id(id)
                .h(px(26.0))
                .px(px(14.0))
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
        let cards = cards.into_iter().enumerate().map(|(i, card)| {
            let host = self.hosts.get(card.host);
            let remote = host.is_some_and(|h| !h.info.local);
            let place = place(card.from.as_deref(), &host.map(|h| h.info.name.clone()).unwrap_or_default(), remote);
            let (deny, allow) = ((card.host, card.id.clone()), (card.host, card.id.clone()));
            div()
                .id(("approval", i))
                .occlude()
                .w(px(340.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .p(px(14.0))
                .continuous(12.0)
                .bg(theme::overlay_fill())
                .border_1()
                .border_color(rgb(theme::border()))
                .shadow_xl()
                .text_size(px(13.0))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(card.asks()))
                .child(div().child(card.summary.clone()))
                .child(div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(place))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap(px(8.0))
                        .pt(px(4.0))
                        .child(button(("approval-deny", i), "Deny", false).when(card.answering, |el| el.opacity(0.5)).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.shared.update(cx, |shared, cx| shared.answer_approval(deny.0, &deny.1, false, cx));
                            },
                        )))
                        .child(button(("approval-allow", i), "Allow", true).when(card.answering, |el| el.opacity(0.5)).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.shared.update(cx, |shared, cx| shared.answer_approval(allow.0, &allow.1, true, cx));
                            },
                        ))),
                )
        });
        let cards: Vec<_> = cards.collect();
        Some(
            div()
                .absolute()
                .top(px(TITLEBAR_HEIGHT + 8.0))
                .left_0()
                .right_0()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .children(cards)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(label: &str, from: Option<&str>) -> Caller {
        Caller { client_id: 3, label: label.into(), from: from.map(Into::into) }
    }

    #[test]
    fn cards_come_and_go_by_host_and_id() {
        let now = Instant::now();
        let mut approvals = Approvals::default();
        approvals.requested(0, "a", "Create a device invite valid for 10 min", &caller("silo cli · claude", Some("work › api")), 120_000, now);
        approvals.requested(1, "a", "Stop the server and end every session", &caller("silo cli", None), 120_000, now);
        approvals.requested(0, "a", "Create a device invite valid for 10 min", &caller("silo cli · claude", None), 120_000, now);
        assert_eq!(approvals.cards.len(), 2, "a repeated id replaces its card");
        assert!(!approvals.resolved(0, "zz"));
        assert!(approvals.resolved(1, "a"));
        assert_eq!(approvals.cards.iter().map(|c| (c.host, c.id.as_str())).collect::<Vec<_>>(), [(0, "a")]);
        assert_eq!(approvals.live(now + Duration::from_secs(121)).count(), 0, "expired");
        assert!(approvals.host_gone(0));
        assert!(approvals.cards.is_empty());
    }

    #[test]
    fn cards_show_no_paths_or_control_characters() {
        let mut approvals = Approvals::default();
        let who = caller("agent\u{202e} /usr/local/bin/silo", Some("work › ~/dev/api build"));
        approvals.requested(0, "a", "Remove the host /tmp/x mini", &who, 1, Instant::now());
        let card = &approvals.cards[0];
        assert_eq!(card.caller, "agent");
        assert_eq!(card.asks(), "\u{201c}agent\u{201d} wants to", "a name the caller chose is quoted");
        assert_eq!(card.from.as_deref(), Some("work › build"));
        assert_eq!(card.summary, "Remove the host mini");
        approvals.requested(0, "b", "x", &caller("/bin/sh", None), 1, Instant::now());
        assert_eq!(approvals.cards[1].asks(), "A program wants to");
    }

    #[test]
    fn a_call_from_this_device_says_so() {
        assert_eq!(place(Some("this device (CLI)"), "mini", true), "From this Mac (CLI) on mini");
        assert_eq!(place(Some("this device (CLI)"), "This Mac", false), "From this device (CLI) on This Mac", "only a remote host's cards");
        assert_eq!(place(Some("Ana's Mac (CLI)"), "This Mac", false), "From Ana's Mac (CLI) on This Mac");
        assert_eq!(place(Some("work › api"), "mini", true), "From work › api on mini");
        assert_eq!(place(None, "mini", true), "On mini");
    }
}
