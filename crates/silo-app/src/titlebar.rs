//! Titlebar: traffic-light gap, the session pill, and the window tabs.

use gpui::{AnyElement, Context, FontWeight, IntoElement, Window, div, prelude::*, px, rgb};
use silo_proto::control::Method;

use crate::{
    assets::icon,
    theme,
    workspace::{NewWindow, TITLEBAR_HEIGHT, Workspace},
};

/// Width reserved for the traffic lights.
pub const TRAFFIC_LIGHTS: f32 = 86.0;
/// Shared height of the session pill and tab pills.
pub const PILL_HEIGHT: f32 = 30.0;
const PILL_RADIUS: f32 = PILL_HEIGHT / 2.0;

impl Workspace {
    pub(crate) fn render_titlebar(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mut bar = div()
            .id("titlebar")
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(px(TITLEBAR_HEIGHT))
            .pl(px(TRAFFIC_LIGHTS))
            .pr(px(10.0))
            .gap(px(6.0))
            .bg(rgb(theme::chrome()))
            .font_family(theme::UI_FONT)
            .child(self.render_pill(cx))
            .child(div().w(px(4.0)).flex_none());
        if let Some(view) = &self.view {
            let active = view.session.active_window.clone();
            // A lone tab is not "selected" among anything: draw it plain.
            let several = view.session.windows.len() > 1;
            for (index, w) in view.session.windows.iter().enumerate() {
                let selected = several && w.id == active;
                let primary = selected || !several;
                let title = self.window_title(w, cx);
                let window_id = w.id.clone();
                let group = format!("tab-{}", w.id);
                bar = bar.child(
                    div()
                        .id(gpui::ElementId::Name(group.clone().into()))
                        .group(group.clone())
                        .flex()
                        .flex_row()
                        .items_center()
                        .flex_shrink(1.0)
                        .min_w(px(120.0))
                        .max_w(px(220.0))
                        .h(px(PILL_HEIGHT))
                        .pl(px(10.0))
                        .pr(px(6.0))
                        .gap(px(7.0))
                        .rounded(px(PILL_RADIUS))
                        .text_size(px(12.5))
                        .when(selected, |el| el.bg(rgb(theme::tab_active())))
                        .when(!selected, |el| el.hover(|s| s.bg(rgb(theme::tab_hover()))))
                        .text_color(rgb(if primary { theme::foreground() } else { theme::muted() }))
                        .cursor_pointer()
                        .child(icon("terminal", 14.0, if selected { theme::accent() } else { theme::muted() }))
                        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(title))
                        .child(
                            div()
                                .id(gpui::ElementId::Name(format!("close-{}", w.id).into()))
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(18.0))
                                .flex_none()
                                .rounded(px(9.0))
                                .opacity(0.0)
                                .group_hover(group, |s| s.opacity(1.0))
                                .hover(|s| s.bg(rgb(theme::border())))
                                .child(icon("close", 11.0, theme::muted()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.call(Method::WindowClose { window_id: window_id.clone() });
                                })),
                        )
                        .on_click(cx.listener(move |this, _, _, _| this.select_window(index))),
                );
            }
            bar = bar.child(div().flex_1()).child(
                div()
                    .id("new-tab")
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .size(px(PILL_HEIGHT))
                    .rounded(px(PILL_RADIUS))
                    .hover(|s| s.bg(rgb(theme::tab_hover())))
                    .cursor_pointer()
                    .child(icon("plus", 15.0, theme::muted()))
                    .on_click(cx.listener(|this, _, window, cx| this.new_window(&NewWindow, window, cx))),
            );
        }
        bar.into_any_element()
    }

    fn render_pill(&self, cx: &mut Context<Self>) -> AnyElement {
        let host = &self.hosts[self.active_host];
        let name = self.view.as_ref().map_or_else(|| "—".to_owned(), |v| v.session.name.clone());
        let open = self.switcher.is_some();
        div()
            .id("session-pill")
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .h(px(PILL_HEIGHT))
            .pl(px(10.0))
            .pr(px(14.0))
            .max_w(px(240.0))
            .rounded(px(PILL_RADIUS))
            // Plain until the switcher is open; a faint wash on hover.
            .when(open, |el| el.bg(rgb(theme::tab_active())))
            .when(!open, |el| el.hover(|s| s.bg(rgb(theme::tab_hover()))))
            .cursor_pointer()
            .child(icon(if host.info.local { "laptop" } else { "globe" }, 15.0, theme::foreground()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .justify_center()
                    .child(div().text_size(px(12.0)).font_weight(FontWeight::BOLD).line_height(px(13.0)).text_ellipsis().child(name))
                    .child(div().text_size(px(9.5)).line_height(px(11.0)).text_color(rgb(theme::muted())).child(host.info.name.clone())),
            )
            .on_click(cx.listener(|this, _, window, cx| this.toggle_switcher(window, cx)))
            // Right-click renames the current session.
            .on_mouse_down(gpui::MouseButton::Right, cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.open_switcher_rename(window, cx);
            }))
            .into_any_element()
    }
}
