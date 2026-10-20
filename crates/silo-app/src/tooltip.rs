//! A small text tooltip for icon buttons.

use gpui::{AnyView, App, Context, IntoElement, Render, SharedString, Window, div, prelude::*, px, rgb};

use crate::theme::Continuous as _;
use crate::{radius, theme};

/// Text and its gap below the pointer.
pub struct Tooltip(SharedString, gpui::Pixels);

impl Tooltip {
    /// Builder for `.tooltip(...)`.
    pub fn text(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let text = text.into();
        move |_, cx| cx.new(|_| Tooltip(text.clone(), px(6.0))).into()
    }

    /// [`Self::text`], shown below window-relative `bottom` (e.g. the
    /// titlebar) instead of just below the pointer, so it never covers
    /// the control.
    pub fn below(text: impl Into<SharedString>, bottom: f32) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let text = text.into();
        move |window, cx| {
            let gap = px(bottom + 6.0) - window.mouse_position().y;
            cx.new(|_| Tooltip(text.clone(), gap.max(px(6.0)))).into()
        }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // Padding on a wrapper: a root's own margin does not move it.
        div().pt(self.1 - px(6.0)).child(
            div()
                .mt(px(6.0))
                .px(px(7.0))
                .py(px(3.0))
                .continuous(radius::TOOLTIP)
                .bg(theme::overlay_fill())
                .border_1()
                .border_color(rgb(theme::border()))
                .font_family(theme::ui_font())
                .text_size(px(11.5))
                .text_color(rgb(theme::foreground()))
                .child(self.0.clone()),
        )
    }
}
