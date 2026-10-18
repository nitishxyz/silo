//! A small text tooltip for icon buttons.

use gpui::{AnyView, App, Context, IntoElement, Render, SharedString, Window, div, prelude::*, px, rgb};

use crate::theme::Continuous as _;
use crate::{radius, theme};

pub struct Tooltip(SharedString);

impl Tooltip {
    /// Builder for `.tooltip(...)`.
    pub fn text(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let text = text.into();
        move |_, cx| cx.new(|_| Tooltip(text.clone())).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .mt(px(6.0))
            .px(px(7.0))
            .py(px(3.0))
            .continuous(radius::TOOLTIP)
            .bg(rgb(theme::popover()))
            .border_1()
            .border_color(rgb(theme::border()))
            .font_family(theme::ui_font())
            .text_size(px(11.5))
            .text_color(rgb(theme::foreground()))
            .child(self.0.clone())
    }
}
