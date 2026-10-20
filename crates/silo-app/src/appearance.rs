//! The appearance menu behind the titlebar's eye button: pane layout
//! density and pane headers, the same settings as the palette's
//! `Appearance: …` and `Show/Hide Pane Headers` commands.

use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, FocusHandle, IntoElement, Subscription, Window, div, prelude::*, px, rgb};

use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    hosts::Density,
    theme,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

pub struct AppearanceMenu {
    pub open: bool,
    /// When an outside click closed it: that click may be on the eye
    /// button, which must not reopen it.
    closed_at: Option<Instant>,
    /// Focused while open, so Escape reaches the menu, not the terminal.
    focus: FocusHandle,
    _blur: Subscription,
}

impl AppearanceMenu {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, |this, _, cx| this.close_appearance_menu(cx));
        Self { open: false, closed_at: None, focus, _blur: blur }
    }
}

impl Workspace {
    fn close_appearance_menu(&mut self, cx: &mut Context<Self>) {
        if self.appearance_menu.open {
            self.appearance_menu.open = false;
            self.appearance_menu.closed_at = Some(Instant::now());
            cx.notify();
        }
    }

    pub(crate) fn toggle_appearance_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let menu = &mut self.appearance_menu;
        if menu.closed_at.is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
            menu.closed_at = None;
            return;
        }
        menu.open = !menu.open;
        if menu.open {
            let focus = menu.focus.clone();
            focus.focus(window, cx);
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        } else {
            self.refocus_terminal(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn render_appearance_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.appearance_menu.open {
            return None;
        }
        let density = self.density();
        let headers = self.pane_headers();
        let row = |id: &'static str, label: &'static str, checked: bool| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .h(px(28.0))
                .px(px(8.0))
                .continuous(crate::radius::MENU_ROW)
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_fill()))
                .child(div().flex_1().child(label))
                .child(div().size(px(14.0)).flex_none().when(checked, |el| el.child(icon("check", 13.0, theme::accent()))))
        };
        Some(
            div()
                .id("appearance-menu")
                .track_focus(&self.appearance_menu.focus)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.close_appearance_menu(cx);
                        this.refocus_terminal(window, cx);
                    }
                }))
                .absolute()
                .top(px(TITLEBAR_HEIGHT - 4.0))
                .right(px(10.0))
                .w(px(220.0))
                .p(px(4.0))
                .flex()
                .flex_col()
                .continuous(10.0)
                .bg(theme::overlay_fill())
                .border_1()
                .border_color(rgb(theme::border()))
                .shadow_lg()
                .occlude()
                .font_family(theme::ui_font())
                .text_size(px(12.5))
                .text_color(rgb(theme::foreground()))
                .child(div().px(px(8.0)).pt(px(4.0)).pb(px(2.0)).text_size(px(11.0)).text_color(rgb(theme::muted())).child("Layout"))
                .child(
                    row("appearance-comfortable", "Comfortable", density == Density::Comfortable)
                        .on_click(cx.listener(|this, _, _, cx| this.set_density(Density::Comfortable, cx))),
                )
                .child(
                    row("appearance-compact", "Compact", density == Density::Compact)
                        .on_click(cx.listener(|this, _, _, cx| this.set_density(Density::Compact, cx))),
                )
                .child(div().h(px(1.0)).mx(px(4.0)).my(px(4.0)).bg(rgb(theme::border())))
                .child(
                    row("appearance-headers", "Show Pane Headers", headers)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_pane_headers(!headers, cx))),
                )
                .child(div().h(px(1.0)).mx(px(4.0)).my(px(4.0)).bg(rgb(theme::border())))
                .child(
                    row("appearance-settings", "All Settings…  ⌘,", false)
                        .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
                )
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.appearance_menu.open = false;
                    this.appearance_menu.closed_at = Some(Instant::now());
                    this.refocus_terminal(window, cx);
                    cx.notify();
                }))
                .into_any_element(),
        )
    }
}
