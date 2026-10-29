//! The confirm card the close guard asks with (busy programs, unsaved
//! files) and a Changes block's revert: a card over a dimless backdrop,
//! below the titlebar, with a headline, details and buttons. Each dialog
//! keeps its own state and keys; this is the chrome they share.

use gpui::{AnyElement, App, FontWeight, IntoElement, MouseDownEvent, Stateful, Window, div, prelude::*, px, rgb};

use crate::surface::Glassy as _;
use crate::{
    theme::{self, Continuous as _},
    workspace::TITLEBAR_HEIGHT,
};

/// The card: a column with room for a headline, details and a button row.
pub(crate) fn card(id: &'static str) -> Stateful<gpui::Div> {
    div().id(id).occlude().w(px(400.0)).flex().flex_col().gap(px(8.0)).p(px(18.0)).glass_sheet(14.0).font_family(theme::ui_font()).text_size(px(13.0))
}

/// A card's button: `primary` filled with the accent, else a control with
/// `danger`'s red or the foreground.
pub(crate) fn button(id: &'static str, label: &'static str, primary: bool, danger: bool) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px(px(14.0))
        .flex()
        .items_center()
        .continuous(8.0)
        .text_size(px(13.0))
        .font_weight(FontWeight::MEDIUM)
        .when(primary, |el| el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())).hover(|s| s.opacity(0.88)))
        .when(!primary, |el| {
            el.bg(theme::control_fill())
                .text_color(rgb(if danger { theme::danger() } else { theme::foreground() }))
                .hover(|s| s.bg(theme::row_fill()))
        })
        .child(label)
}

/// The headline.
pub(crate) fn headline(text: impl Into<gpui::SharedString>) -> gpui::Div {
    div().font_weight(FontWeight::SEMIBOLD).text_size(px(14.0)).child(text.into())
}

/// Muted detail under the headline.
pub(crate) fn detail(text: impl Into<gpui::SharedString>) -> gpui::Div {
    div().text_size(px(12.5)).text_color(rgb(theme::muted())).child(text.into())
}

/// The card over the window; a press beside it calls `dismiss`.
pub(crate) fn backdrop(id: &'static str, card: impl IntoElement, dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static) -> AnyElement {
    div()
        .id(id)
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(TITLEBAR_HEIGHT + 60.0))
        .on_mouse_down(gpui::MouseButton::Left, dismiss)
        .child(card)
        .into_any_element()
}
