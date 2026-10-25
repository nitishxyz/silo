//! File tabs in a block's header, shared by Files and Changes: the name
//! (and folder for twins), a dot when changed, × to close (a dot in its
//! place while unsaved); middle-click closes too. Scrolls sideways.

use std::rc::Rc;

use gpui::{AnyElement, App, ClickEvent, MouseButton, MouseDownEvent, SharedString, Window, div, prelude::*, px, rgb};

use crate::{
    pane_header::{BUTTON, HeaderStyle, keep_press},
    theme::Continuous as _,
};

/// Element ids of one block type's strip.
#[derive(Clone, Copy)]
pub struct Ids {
    pub strip: &'static str,
    pub tab: &'static str,
    pub close: &'static str,
}

/// One tab.
pub struct Tab {
    pub name: String,
    /// The folder telling twins apart.
    pub dir: Option<String>,
    pub tooltip: String,
    pub shown: bool,
    /// Unsaved: a dot in the ×'s place.
    pub dirty: bool,
    /// Changed elsewhere: an accent dot.
    pub dot: bool,
}

type OnTab = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// The strip of `tabs`; `open` and `close` get a tab's index.
pub fn strip(
    ids: Ids,
    tabs: Vec<Tab>,
    style: HeaderStyle,
    open: impl Fn(usize, &mut Window, &mut App) + 'static,
    close: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let palette = style.palette;
    let (open, close): (OnTab, OnTab) = (Rc::new(open), Rc::new(close));
    let tabs = tabs.into_iter().enumerate().map(|(i, tab)| {
        let Tab { name, dir, tooltip, shown, dirty, dot } = tab;
        let group: SharedString = format!("{}-{i}", ids.tab).into();
        let ink = if shown && style.active { palette.ink } else { palette.secondary };
        let (on_close, on_middle, on_open) = (close.clone(), close.clone(), open.clone());
        let close_button = div()
            .id((ids.close, i))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(16.0))
            .continuous(4.0)
            .hover(|s| s.bg(palette.hover()))
            .relative()
            .when(!shown && !dirty, |el| el.opacity(0.0).group_hover(group.clone(), |s| s.opacity(1.0)))
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                on_close(i, window, cx);
            })
            // Unsaved: a dot in the ×'s place, the × on hover.
            .when(dirty, |el| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .group_hover(group.clone(), |s| s.opacity(0.0))
                        .child(div().size(px(7.0)).rounded_full().bg(rgb(ink))),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(dirty, |el| el.opacity(0.0).group_hover(group.clone(), |s| s.opacity(1.0)))
                    .child(crate::assets::icon("close", 10.0, palette.icon)),
            );
        let dot = dot.then(|| div().flex_none().size(px(6.0)).rounded_full().bg(rgb(palette.accent)));
        keep_press(
            div()
                .id((ids.tab, i))
                .group(group)
                .flex()
                .flex_none()
                .items_center()
                .gap(px(5.0))
                .h(px(BUTTON))
                .pl(px(8.0))
                .pr(px(3.0))
                .continuous(style.control)
                .when(shown, |el| el.bg(palette.selected()))
                .when(!shown, |el| el.hover(|s| s.bg(palette.hover())))
                .cursor_pointer()
                .on_click(move |_: &ClickEvent, window, cx| on_open(i, window, cx))
                .on_mouse_down(MouseButton::Middle, move |_: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    on_middle(i, window, cx);
                })
                .child(div().text_color(rgb(ink)).child(name))
                .children(dir.map(|d| div().text_color(rgb(palette.muted)).child(d)))
                .children(dot)
                .child(close_button),
        )
        .tooltip(crate::tooltip::Tooltip::text(SharedString::from(tooltip)))
    });
    div()
        .id(ids.strip)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .flex_1()
        .min_w_0()
        .overflow_x_scroll()
        .whitespace_nowrap()
        .children(tabs)
        .into_any_element()
}
