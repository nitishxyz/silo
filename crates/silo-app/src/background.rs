//! The titlebar's background button and its menu: the shown session's
//! parked blocks (a command an agent started, a hidden terminal), which
//! are in no tab. Each row shows the block's kind, title, state and who
//! started it; Show floats it into the shown tab, × closes it (asking
//! first when a program runs). Its rows are `block_list.rs`'s, as in ⌘P.

use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, FocusHandle, FontWeight, IntoElement, KeyDownEvent, Subscription, Window, div, prelude::*, px, rgb};

use crate::surface::Glassy as _;
use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    block_list::BlockRow,
    close_confirm::CloseTarget,
    theme,
    titlebar::PILL_HEIGHT,
    tooltip::Tooltip,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

/// From the window's right edge to the background button's: the titlebar's
/// padding, the "+" and eye buttons and their gaps (`titlebar.rs`).
const BUTTON_RIGHT: f32 = 10.0 + PILL_HEIGHT + 2.0 + PILL_HEIGHT + 6.0;
const MENU_WIDTH: f32 = 320.0;

pub struct BackgroundMenu {
    pub open: bool,
    selected: usize,
    /// When an outside click closed it: that click may be on the button,
    /// which must not reopen it.
    closed_at: Option<Instant>,
    /// Focused while open, so the keys reach the menu, not the terminal.
    focus: FocusHandle,
    _blur: Subscription,
}

impl BackgroundMenu {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, |this, _, cx| this.close_background_menu(cx));
        Self { open: false, selected: 0, closed_at: None, focus, _blur: blur }
    }
}

/// The `n`th row's selection after the list changed to `count` rows.
fn clamp(selected: usize, count: usize) -> usize {
    selected.min(count.saturating_sub(1))
}

impl Workspace {
    fn has_background(&self) -> bool {
        self.view.as_ref().is_some_and(|v| !v.session.parked.is_empty())
    }

    /// The menu is open and has something to show.
    pub(crate) fn background_shown(&self) -> bool {
        self.background.open && self.has_background()
    }

    /// An open menu whose last block went: closed, so a later one does not
    /// pop it open again.
    pub(crate) fn settle_background_menu(&mut self) {
        if self.background.open && !self.has_background() {
            self.background.open = false;
        }
    }

    fn close_background_menu(&mut self, cx: &mut Context<Self>) {
        if self.background.open {
            self.background.open = false;
            self.background.closed_at = Some(Instant::now());
            cx.notify();
        }
    }

    pub(crate) fn toggle_background_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let menu = &mut self.background;
        if menu.closed_at.is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
            menu.closed_at = None;
            return;
        }
        menu.open = !menu.open;
        if menu.open {
            menu.selected = 0;
            let focus = menu.focus.clone();
            focus.focus(window, cx);
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        } else {
            self.refocus_block(window, cx);
        }
        cx.notify();
    }

    /// Show: float row `index`'s block into the shown tab and focus it.
    fn background_show(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.background_rows(cx).into_iter().nth(index) else { return };
        self.background.open = false;
        self.pick_block(row.target(), window, cx);
        cx.notify();
    }

    /// ×: close row `index`'s block, asking first if a program runs. The
    /// menu stays open on the rest.
    fn background_close(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.background_rows(cx).into_iter().nth(index) else { return };
        self.close(CloseTarget::Pane(row.block), window, cx);
        cx.notify();
    }

    fn background_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.view.as_ref().map_or(0, |v| v.session.parked.len());
        let selected = clamp(self.background.selected, count);
        match event.keystroke.key.as_str() {
            "up" if count > 0 => self.background.selected = (selected + count - 1) % count,
            "down" if count > 0 => self.background.selected = (selected + 1) % count,
            "enter" => self.background_show(selected, window, cx),
            "backspace" | "delete" => self.background_close(selected, window, cx),
            "escape" => {
                self.close_background_menu(cx);
                self.refocus_block(window, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The titlebar button, while the session has background blocks: a
    /// glyph and their count.
    pub(crate) fn render_background_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.view.as_ref().map_or(0, |v| v.session.parked.len());
        if count == 0 {
            return None;
        }
        let (chrome, open) = (self.chrome, self.background_shown());
        let hover = chrome.hover();
        let tip = if count == 1 { "1 background block".to_owned() } else { format!("{count} background blocks") };
        Some(
            div()
                .id("background-blocks")
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .flex_none()
                .ml(px(6.0))
                .h(px(PILL_HEIGHT))
                .min_w(px(PILL_HEIGHT))
                .px(px(8.0))
                .capsule()
                .hover(|s| s.bg(hover))
                .cursor_pointer()
                .when(open, |el| el.bg(hover))
                .when(!open, |el| el.tooltip(Tooltip::text(tip)))
                .child(icon("layers", 15.0, chrome.icon))
                .child(div().text_size(px(11.0)).font_weight(FontWeight::MEDIUM).text_color(rgb(chrome.muted)).child(count.to_string()))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, window, cx| this.toggle_background_menu(window, cx)))
                .into_any_element(),
        )
    }

    pub(crate) fn render_background_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let _glass = crate::surface::enter();
        if !self.background_shown() {
            return None;
        }
        let rows = self.background_rows(cx);
        let selected = clamp(self.background.selected, rows.len());
        let mut list = div().flex().flex_col().gap(px(1.0));
        for (index, row) in rows.into_iter().enumerate() {
            list = list.child(self.background_row(row, index, index == selected, cx));
        }
        Some(
            div()
                .id("background-menu")
                .track_focus(&self.background.focus)
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| this.background_key(event, window, cx)))
                .absolute()
                .top(px(TITLEBAR_HEIGHT - 4.0))
                .right(px(BUTTON_RIGHT))
                .w(px(MENU_WIDTH))
                .p(px(4.0))
                .flex()
                .flex_col()
                .glass(10.0)
                .occlude()
                .font_family(theme::ui_font())
                .text_size(px(12.5))
                .text_color(rgb(theme::foreground()))
                .child(div().px(px(8.0)).pt(px(4.0)).pb(px(2.0)).text_size(px(11.0)).text_color(rgb(theme::muted())).child("Background"))
                .child(list)
                .child(div().h(px(1.0)).mx(px(4.0)).my(px(4.0)).bg(rgb(theme::border())))
                .child(
                    div().px(px(8.0)).pb(px(2.0)).text_size(px(11.0)).text_color(rgb(theme::muted())).child(format!(
                        "↑↓ · ↩ show · ⌫ close · esc   {} all blocks",
                        crate::commands::Command::BrowseBlocks.hint().unwrap_or_default()
                    )),
                )
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.close_background_menu(cx);
                    this.refocus_block(window, cx);
                }))
                .into_any_element(),
        )
    }

    fn background_row(&self, row: BlockRow, index: usize, selected: bool, cx: &mut Context<Self>) -> AnyElement {
        let status = row.status();
        let tip = row.command.clone();
        let close_tip = if row.busy { "Close (asks first: a program is running)" } else { "Close" };
        let button = |id: &'static str| {
            div()
                .id((id, index))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .h(px(22.0))
                .continuous(crate::radius::MENU_ROW)
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_fill()))
        };
        div()
            .id(("background-row", index))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .py(px(5.0))
            .continuous(crate::radius::MENU_ROW)
            .when(selected, |el| el.bg(theme::row_fill()))
            .when(!selected, |el| el.hover(|s| s.bg(theme::hover_fill())))
            .cursor_pointer()
            .child(div().w(px(14.0)).flex_none().flex().justify_center().child(icon(row.icon(), 13.0, theme::muted())))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(div().text_ellipsis().child(row.title))
                    .child(div().text_ellipsis().text_size(px(11.0)).text_color(rgb(theme::muted())).child(status)),
            )
            .when_some(tip, |el, tip| el.tooltip(Tooltip::text(tip)))
            .child(
                button("background-show")
                    .px(px(7.0))
                    .text_size(px(11.5))
                    .text_color(rgb(theme::muted()))
                    .child("Show")
                    .tooltip(Tooltip::text("Float it into this tab  ↩"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.background_show(index, window, cx);
                    })),
            )
            .child(
                button("background-close")
                    .w(px(22.0))
                    .child(icon("close", 12.0, theme::muted()))
                    .tooltip(Tooltip::text(format!("{close_tip}  ⌫")))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.background_close(index, window, cx);
                    })),
            )
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.background.selected != index {
                    this.background.selected = index;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| this.background_show(index, window, cx)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selection_stays_on_the_list_as_it_shrinks() {
        assert_eq!(clamp(0, 3), 0);
        assert_eq!(clamp(2, 3), 2);
        assert_eq!(clamp(2, 2), 1, "the last row closed");
        assert_eq!(clamp(5, 0), 0);
    }
}
