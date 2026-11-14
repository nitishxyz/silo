//! A page asking for the camera, microphone, location or notifications:
//! a card under the page's top edge says who wants what, with Allow once,
//! Always allow for this site and Don't allow. The engine remembers
//! "always" and "never" per site; a request whose page navigated or
//! closed goes away unanswered (refused).

use gpui::{div, prelude::*, px};

use super::BrowserPane;
use crate::confirm_card;

impl BrowserPane {
    /// `Event::PermissionAsked`.
    pub(super) fn permission_asked(&mut self, request: silo_web::PermissionRequest, cx: &mut gpui::Context<Self>) {
        tracing::info!(block = %self.block_id, origin = %request.origin, permissions = ?request.permissions, "page asks for a permission");
        self.permissions.push(request);
        cx.notify();
    }

    /// `Event::PermissionDismissed`.
    pub(super) fn permission_dismissed(&mut self, id: u64, cx: &mut gpui::Context<Self>) {
        self.permissions.retain(|r| r.id != id);
        cx.notify();
    }

    fn answer_permission(&mut self, id: u64, answer: silo_web::Answer, cx: &mut gpui::Context<Self>) {
        self.permissions.retain(|r| r.id != id);
        if let Some(page) = &self.page {
            page.answer_permission(id, answer);
        }
        cx.notify();
    }

    /// The card for the oldest request, while there is one.
    pub(super) fn permission_card(&self, cx: &mut gpui::Context<Self>) -> Option<gpui::AnyElement> {
        let request = self.permissions.first()?;
        let id = request.id;
        let button = |key: &'static str, label: &'static str, primary: bool, answer: silo_web::Answer, cx: &mut gpui::Context<Self>| {
            confirm_card::button(key, label, primary, false)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.answer_permission(id, answer, cx)))
        };
        let more = self.permissions.len() - 1;
        let card = confirm_card::card("browser-permission")
            .w(px(380.0))
            .max_w_full()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(confirm_card::headline(request.text()))
            .when(more > 0, |card| card.child(confirm_card::detail(format!("{more} more waiting"))))
            .child(
                div()
                    .pt(px(4.0))
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap(px(8.0))
                    .child(button("permission-deny", "Don't allow", false, silo_web::Answer::Never, cx))
                    .child(button("permission-always", "Always allow for this site", false, silo_web::Answer::Always, cx))
                    .child(button("permission-once", "Allow once", true, silo_web::Answer::Once, cx)),
            );
        Some(div().absolute().top(px(10.0)).left_0().right_0().px(px(10.0)).flex().justify_center().child(card).into_any_element())
    }
}
