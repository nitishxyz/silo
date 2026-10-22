//! The one list row of the agent block (`docs/AGENT_BLOCK.md` §4c
//! "Alignment"): switcher, model and agent pickers, the ⋮ and slash menus,
//! and the provider rows share its height, icon column, two-line text block
//! and trailing column, and the small search pill above a list.

use gpui::{AnyElement, Div, ElementId, FontWeight, Hsla, IntoElement, SharedString, Stateful, div, prelude::*, px, rgb};

use crate::{
    assets::icon,
    theme::{self, Continuous as _},
};

pub const ROW_H: f32 = 44.0;
/// Horizontal padding inside a row (and of the search pill's icon).
pub const PAD: f32 = 8.0;
pub const ICON_COL: f32 = 28.0;
pub const GAP: f32 = 8.0;
pub const SEARCH_H: f32 = 36.0;

fn hsla(c: u32, alpha: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a *= alpha;
    h
}

/// A row: `lead` in the icon column, `title` over an optional muted
/// `subtitle`, then `trailing` (check, shortcut, buttons) right-aligned.
/// `selected` is the keyboard selection; hover and press fill too.
pub fn row(
    id: impl Into<ElementId>,
    selected: bool,
    lead: Option<AnyElement>,
    title: impl IntoElement,
    subtitle: Option<AnyElement>,
    trailing: Option<AnyElement>,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .w_full()
        .h(px(ROW_H))
        .px(px(PAD))
        .flex()
        .items_center()
        .gap(px(GAP))
        .continuous(8.0)
        .when(selected, |el| el.bg(theme::row_fill()))
        .child(div().flex_none().w(px(ICON_COL)).flex().items_center().justify_center().children(lead))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(13.0)).child(title))
                .children(subtitle.map(|s| {
                    div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.5)).text_color(rgb(theme::muted())).child(s)
                })),
        )
        .children(trailing.map(|t| div().flex_none().flex().items_center().justify_end().child(t)))
}

/// Hover and press fills for a clickable row.
pub fn clickable(row: Stateful<Div>, selected: bool) -> Stateful<Div> {
    row.cursor_pointer().when(!selected, |el| el.hover(|s| s.bg(theme::hover_fill()))).active(|s| s.bg(theme::row_fill()).opacity(0.85))
}

/// The check of the current choice, in the trailing column.
pub fn check() -> AnyElement {
    div().w(px(20.0)).flex().justify_center().child(icon("check", 13.0, theme::foreground())).into_any_element()
}

/// A muted shortcut or note in the trailing column.
pub fn note(text: impl Into<SharedString>) -> AnyElement {
    div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(text.into()).into_any_element()
}

/// A row icon (muted unless `strong`).
pub fn lead(name: &'static str, strong: bool) -> AnyElement {
    icon(name, 16.0, if strong { theme::foreground() } else { theme::muted() }).into_any_element()
}

/// A section label between groups of rows, aligned with the titles.
pub fn section(label: impl Into<SharedString>, lead: Option<AnyElement>) -> Div {
    div()
        .h(px(26.0))
        .px(px(PAD))
        .flex()
        .items_center()
        .gap(px(GAP))
        .text_size(px(11.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::muted()))
        .child(div().flex_none().w(px(ICON_COL)).flex().justify_center().children(lead))
        .child(label.into())
}

/// The small pill input above a list: the search glyph in the icon column,
/// the field where row titles start.
pub fn search(focused: bool, field: impl IntoElement) -> Div {
    let border = theme::mix(theme::background(), theme::foreground(), if focused { 0.4 } else { 0.2 });
    div()
        .flex_none()
        .h(px(SEARCH_H))
        .mb(px(6.0))
        .flex()
        .items_center()
        .gap(px(GAP))
        .px(px(PAD - 1.0))
        .capsule()
        .border_1()
        .border_color(rgb(border))
        .bg(hsla(theme::foreground(), 0.03))
        .child(div().flex_none().w(px(ICON_COL)).flex().justify_center().child(icon("search", 14.0, theme::muted())))
        .child(div().flex_1().min_w_0().text_size(px(13.0)).child(field))
}

/// A thin rule between row groups.
pub fn divider() -> Div {
    div().h(px(1.0)).mx(px(PAD)).my(px(4.0)).bg(rgb(theme::mix(theme::background(), theme::foreground(), 0.1)))
}
