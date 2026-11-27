//! A page's JavaScript dialog (`alert`, `confirm`, `prompt`, "Leave site?")
//! as a card over the page, which waits on it (clicks on the page do
//! nothing meanwhile). ↩ answers OK, esc Cancel; a prompt has a field with
//! the page's default text. An agent driving the page may answer the same
//! dialog (`browser_dialog`); the card then goes.

use gpui::{AppContext as _, div, prelude::*, px, rgb};

use super::BrowserPane;
use crate::{confirm_card, theme};

/// The dialog showing, and a prompt's field.
pub(super) struct DialogCard {
    dialog: silo_web::JsDialog,
    field: Option<gpui::Entity<crate::text_field::TextField>>,
    /// Focus the field (or keep the page's) on the next render.
    focus_pending: bool,
    _events: Option<gpui::Subscription>,
}

impl BrowserPane {
    /// `Event::JsDialog`.
    pub(super) fn js_dialog_opened(&mut self, dialog: silo_web::JsDialog, cx: &mut gpui::Context<Self>) {
        tracing::info!(block = %self.block_id, kind = ?dialog.kind, "page opened a dialog");
        let (field, events) = if dialog.kind == silo_web::JsDialogKind::Prompt {
            let field = cx.new(|cx| {
                let mut field = crate::text_field::TextField::new("", cx);
                field.set_text(dialog.default_text.clone(), cx);
                field.select_all(cx);
                field
            });
            let events = cx.subscribe(&field, |this, _, event: &crate::text_field::FieldEvent, cx| {
                use crate::text_field::FieldEvent;
                match event {
                    FieldEvent::Submit => this.answer_js_dialog(true, cx),
                    FieldEvent::Cancel => this.answer_js_dialog(false, cx),
                    _ => {}
                }
            });
            (Some(field), Some(events))
        } else {
            (None, None)
        };
        self.js_dialog = Some(DialogCard { dialog, field, focus_pending: true, _events: events });
        cx.notify();
    }

    /// `Event::JsDialogClosed`: answered (perhaps by an agent) or gone.
    pub(super) fn js_dialog_closed(&mut self, id: u32, cx: &mut gpui::Context<Self>) {
        if self.js_dialog.as_ref().is_some_and(|c| c.dialog.id == id) {
            self.close_js_dialog(cx);
        }
    }

    /// Drop the card; a prompt's focused field hands focus back to the page.
    fn close_js_dialog(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(card) = self.js_dialog.take()
            && card.field.is_some()
        {
            self.refocus_page = true;
        }
        cx.notify();
    }

    /// The user's answer to the showing dialog.
    pub(super) fn answer_js_dialog(&mut self, accept: bool, cx: &mut gpui::Context<Self>) {
        let Some(card) = self.js_dialog.as_ref() else { return };
        let text = card.field.as_ref().filter(|_| accept).map(|f| f.read(cx).text.clone());
        if let Some(page) = &self.page {
            page.answer_js_dialog(card.dialog.id, accept, text.as_deref());
        }
        self.close_js_dialog(cx);
    }

    /// ↩ / esc on the page while a dialog shows; true if taken.
    pub(super) fn js_dialog_key(&mut self, key: &str, cx: &mut gpui::Context<Self>) -> bool {
        if self.js_dialog.is_none() {
            return false;
        }
        match key {
            "enter" => self.answer_js_dialog(true, cx),
            "escape" => {
                let alert = self.js_dialog.as_ref().is_some_and(|c| c.dialog.dismiss_label().is_none());
                self.answer_js_dialog(alert, cx);
            }
            _ => return false,
        }
        true
    }

    /// Per render: focus a new prompt's field while the pane has focus, and
    /// give focus back to the page after a prompt went.
    pub(super) fn js_dialog_focus(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let page_focused = self.focus.is_focused(window);
        if let Some(card) = self.js_dialog.as_mut().filter(|c| c.focus_pending) {
            card.focus_pending = false;
            if let Some(field) = card.field.as_ref().filter(|_| page_focused) {
                let focus = field.read(cx).focus.clone();
                window.defer(cx, move |window, cx| focus.focus(window, cx));
            }
        }
        if std::mem::take(&mut self.refocus_page) && window.focused(cx).is_none() {
            let focus = self.focus.clone();
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        }
    }

    /// The card, over a veil that keeps clicks off the waiting page.
    pub(super) fn js_dialog_card(&self, cx: &mut gpui::Context<Self>) -> Option<gpui::AnyElement> {
        let card = self.js_dialog.as_ref()?;
        let dialog = &card.dialog;
        let body = dialog.body();
        let mut buttons = div().pt(px(4.0)).flex().justify_end().gap(px(8.0));
        if let Some(label) = dialog.dismiss_label() {
            buttons = buttons.child(
                confirm_card::button("js-dialog-dismiss", label, false, false)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.answer_js_dialog(false, cx))),
            );
        }
        buttons = buttons.child(
            confirm_card::button("js-dialog-accept", dialog.accept_label(), true, false)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.answer_js_dialog(true, cx))),
        );
        let panel = confirm_card::card("browser-js-dialog")
            .max_w_full()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(confirm_card::headline(dialog.headline()))
            .when(!body.trim().is_empty(), |el| {
                el.child(
                    div()
                        .id("js-dialog-message")
                        .max_h(px(240.0))
                        .overflow_y_scroll()
                        .text_size(px(13.0))
                        .text_color(rgb(theme::foreground()))
                        .whitespace_normal()
                        .child(body),
                )
            })
            .when_some(card.field.clone(), |el, field| {
                el.child(div().h(px(28.0)).px(px(8.0)).flex().items_center().rounded(px(6.0)).bg(theme::control_fill()).child(field))
            })
            .child(buttons);
        Some(
            div()
                .id("browser-js-dialog-veil")
                .absolute()
                .inset_0()
                .occlude()
                .bg(gpui::hsla(0.0, 0.0, 0.0, 0.12))
                .flex()
                .justify_center()
                .items_start()
                .pt(px(10.0))
                .px(px(10.0))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        // The pane takes focus (↩ and esc answer), a prompt's field first.
                        let field = this.js_dialog.as_ref().and_then(|c| c.field.as_ref()).map(|f| f.read(cx).focus.clone());
                        match field {
                            Some(field) => field.focus(window, cx),
                            None => this.focus.focus(window, cx),
                        }
                        cx.stop_propagation();
                    }),
                )
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(panel)
                .into_any_element(),
        )
    }
}
