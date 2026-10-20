//! The menu behind the tab strip's + button: a terminal or a browser tab
//! (⌘T and ⌥⌘T skip it).

use std::{cell::Cell, rc::Rc};

use gpui::{AnyElement, Bounds, Context, FocusHandle, FontWeight, IntoElement, Pixels, Subscription, Window, canvas, div, prelude::*, px, rgb};

use crate::theme::Continuous as _;
use crate::{assets::icon, commands::Command, theme, workspace::Workspace};

/// (command, label, icon) of each row.
const ITEMS: [(Command, &str, &str); 2] = [(Command::NewTab, "Terminal", "terminal"), (Command::NewBrowserTab, "Browser", "globe")];

pub struct NewTabMenu {
    pub open: bool,
    selected: usize,
    /// The + button's bounds, from its last paint: the menu hangs below
    /// it, and presses on it are the button's to toggle.
    button: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Focused while open, so keys reach the menu, not the terminal.
    focus: FocusHandle,
    _blur: Subscription,
}

impl NewTabMenu {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, |this, window, cx| {
            if !this.new_tab_menu.over_button(window) {
                this.close_new_tab_menu(cx);
            }
        });
        Self { open: false, selected: 0, button: Rc::default(), focus, _blur: blur }
    }

    fn over_button(&self, window: &Window) -> bool {
        self.button.get().is_some_and(|b| b.contains(&window.mouse_position()))
    }

    /// Records the + button's bounds; a child of the button.
    pub fn button_probe(&self) -> impl IntoElement {
        let button = self.button.clone();
        canvas(move |bounds, _, _| button.set(Some(bounds)), |_, (), _, _| {}).absolute().top_0().left_0().size_full()
    }
}

/// The row after `selected` moved by `step`, wrapping.
fn step(selected: usize, step: isize) -> usize {
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    let next = (selected as isize + step).rem_euclid(ITEMS.len() as isize) as usize;
    next
}

impl Workspace {
    fn close_new_tab_menu(&mut self, cx: &mut Context<Self>) {
        if self.new_tab_menu.open {
            self.new_tab_menu.open = false;
            cx.notify();
        }
    }

    pub(crate) fn toggle_new_tab_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let menu = &mut self.new_tab_menu;
        menu.open = !menu.open;
        if menu.open {
            menu.selected = 0;
            let focus = menu.focus.clone();
            focus.focus(window, cx);
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        } else {
            self.refocus_terminal(window, cx);
        }
        cx.notify();
    }

    fn choose_new_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_new_tab_menu(cx);
        self.refocus_terminal(window, cx);
        self.run(ITEMS[index].0, window, cx);
    }

    pub(crate) fn render_new_tab_menu(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = &self.new_tab_menu;
        // Hangs below the + button, right edges aligned.
        let button = menu.button.get().filter(|_| menu.open)?;
        let right = window.viewport_size().width - button.right();
        let mut list = div().flex().flex_col();
        for (index, (command, label, glyph)) in ITEMS.into_iter().enumerate() {
            let selected = index == menu.selected;
            // A host without the feature: muted, saying so instead of the shortcut.
            let supported = command.feature().is_none_or(|f| self.host_supports(f));
            let hint = if supported { command.hint() } else { Some(self.not_supported()) };
            list = list.child(
                div()
                    .id(("new-tab-item", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(28.0))
                    .px(px(8.0))
                    .continuous(crate::radius::MENU_ROW)
                    .when(supported, |el| el.cursor_pointer())
                    .when(selected, |el| el.bg(theme::row_fill()))
                    .child(icon(glyph, 13.0, theme::muted()))
                    .child(div().flex_1().when(!supported, |el| el.text_color(rgb(theme::muted()))).child(label))
                    .children(hint.map(|h| div().flex_none().text_size(px(11.0)).text_color(rgb(theme::muted())).child(h)))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.new_tab_menu.selected != index {
                            this.new_tab_menu.selected = index;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| this.choose_new_tab(index, window, cx))),
            );
        }
        Some(
            div()
                .id("new-tab-menu")
                .track_focus(&menu.focus)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    let menu = &mut this.new_tab_menu;
                    match ev.keystroke.key.as_str() {
                        "escape" => {
                            this.close_new_tab_menu(cx);
                            this.refocus_terminal(window, cx);
                        }
                        "up" => menu.selected = step(menu.selected, -1),
                        "down" => menu.selected = step(menu.selected, 1),
                        "enter" => {
                            let index = menu.selected;
                            this.choose_new_tab(index, window, cx);
                        }
                        _ => return,
                    }
                    cx.stop_propagation();
                    cx.notify();
                }))
                .absolute()
                .top(button.bottom() + px(4.0))
                .right(right)
                .min_w(px(200.0))
                .p(px(4.0))
                .continuous(10.0)
                .bg(theme::overlay_fill())
                .border_1()
                .border_color(rgb(theme::border()))
                .shadow_lg()
                .occlude()
                .font_family(theme::ui_font())
                .font_weight(FontWeight::NORMAL)
                .text_size(px(12.5))
                .text_color(rgb(theme::foreground()))
                .child(list)
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    if !this.new_tab_menu.over_button(window) {
                        this.close_new_tab_menu(cx);
                        this.refocus_terminal(window, cx);
                    }
                }))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_wraps() {
        assert_eq!(step(0, -1), 1);
        assert_eq!(step(1, 1), 0);
        assert_eq!(step(0, 1), 1);
    }
}
