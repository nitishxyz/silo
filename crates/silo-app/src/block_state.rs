//! A block's state in the middle of its pane, the same for every kind:
//! an icon, a title and a muted detail ("Attaching\u{2026}", "Connecting to
//! otto\u{2026}", "Can't reach box"). Terminals show it while they have nothing
//! to show yet; agents while they reach otto.

use gpui::{FontWeight, ParentElement as _, SharedString, Styled as _, div, px, rgb};

use crate::{assets::icon, theme};

/// The centered state: `glyph`, `title` and an optional `detail`.
pub fn view(glyph: &'static str, title: impl Into<SharedString>, detail: Option<String>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .text_center()
        .child(icon(glyph, 22.0, theme::muted()))
        .child(div().mt(px(10.0)).text_size(px(15.0)).font_weight(FontWeight::MEDIUM).text_color(rgb(theme::foreground())).child(title.into()))
        .children(detail.map(|s| div().mt(px(6.0)).text_size(px(12.5)).text_color(rgb(theme::muted())).child(s)))
}
