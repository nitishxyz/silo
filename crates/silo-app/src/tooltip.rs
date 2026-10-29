//! A small text tooltip for icon buttons.

use std::rc::Rc;

use gpui::{AnyView, App, Context, IntoElement, Render, SharedString, Window, div, prelude::*, px, rgb};

use crate::surface::Glassy as _;
use crate::{radius, theme};

/// Where a live tooltip reads its text.
type Source = Rc<dyn Fn(&App) -> SharedString>;

/// Text and its gap below the pointer; a live one re-reads its text whenever it is drawn.
pub struct Tooltip(SharedString, gpui::Pixels, Option<Source>);

impl Tooltip {
    /// Builder for `.tooltip(...)`.
    pub fn text(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let text = text.into();
        move |_, cx| cx.new(|_| Tooltip(text.clone(), px(6.0), None)).into()
    }

    /// [`Self::text`] whose text follows state that can change while it shows.
    pub fn live(text: impl Fn(&App) -> SharedString + 'static) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let text: Source = Rc::new(text);
        move |_, cx| {
            let text = text.clone();
            cx.new(|cx| Tooltip(text(cx), px(6.0), Some(text))).into()
        }
    }

    /// [`Self::text`], shown below window-relative `bottom` (e.g. the
    /// titlebar) instead of just below the pointer, so it never covers
    /// the control.
    pub fn below(text: impl Into<SharedString>, bottom: f32) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let text = text.into();
        move |window, cx| {
            let gap = px(bottom + 6.0) - window.mouse_position().y;
            cx.new(|_| Tooltip(text.clone(), gap.max(px(6.0)), None)).into()
        }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(text) = &self.2 {
            self.0 = text(cx);
        }
        // Padding on a wrapper: a root's own margin does not move it.
        div().pt(self.1 - px(6.0)).child(
            div()
                .mt(px(6.0))
                .px(px(7.0))
                .py(px(3.0))
                .glass(radius::TOOLTIP)
                .font_family(theme::ui_font())
                .text_size(px(11.5))
                .text_color(rgb(theme::foreground()))
                .child(self.0.clone()),
        )
    }
}
