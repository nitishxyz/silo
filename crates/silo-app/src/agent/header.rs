//! The agent's pane header parts: on the chat screen the session title opens
//! the switcher and History / New Session sit before the pane buttons (not on
//! a sub-agent: its strip leads back to the main agent instead).

use gpui::{AnyElement, Context, div, prelude::*, px, rgb};
use silo_proto::model::BlockInfo;

use super::AgentPane;
use crate::{
    pane_header::{BUTTON, BlockHeader, HeaderParts, HeaderStyle, button, host_label, keep_press, labels},
    theme::Continuous as _,
};

impl BlockHeader for AgentPane {
    fn header(&mut self, style: HeaderStyle, info: Option<&BlockInfo>, cx: &mut Context<Self>) -> HeaderParts {
        let host = self.host.clone();
        let Some(title) = self.header_title() else {
            // Sign-in and Providers: the block's name.
            let name = info.map(|i| i.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Agent".into());
            return named(name, host.as_deref(), style);
        };
        if self.is_subagent() {
            return named(title, host.as_deref(), style);
        }
        let mut parts = HeaderParts::new(session_title(&title, host.as_deref(), style, cx), title);
        parts.actions = vec![history(style, cx), new_session(style, cx)];
        parts
    }
}

/// A plain name; a remote host's agent names its host after it.
fn named(name: String, host: Option<&str>, style: HeaderStyle) -> HeaderParts {
    let Some(host) = host else { return labels(name, None, style) };
    let middle = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .child(div().min_w_0().overflow_hidden().text_ellipsis().text_color(rgb(style.ink())).child(name.clone()))
        .child(host_label(host, style));
    HeaderParts::new(middle.into_any_element(), name)
}

/// The session's title; opens the switcher. Its hover fill reaches past the
/// text, which keeps its place.
fn session_title(title: &str, host: Option<&str>, style: HeaderStyle, cx: &mut Context<AgentPane>) -> AnyElement {
    let palette = style.palette;
    let button = div()
        .id("agent-header-title")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .min_w_0()
        .pl(px(5.0))
        .pr(px(3.0))
        .h(px(BUTTON))
        .continuous(style.control)
        .cursor_pointer()
        .hover(move |s| s.bg(palette.hover()))
        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(rgb(style.ink())).child(title.to_owned()))
        .child(crate::assets::icon("chevron-down", 11.0, palette.icon))
        .on_click(cx.listener(|this, _, window, cx| this.open_sessions(window, cx)));
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(3.0))
        .flex_1()
        .min_w_0()
        .ml(px(-5.0))
        .child(keep_press(button))
        .children(host.map(|h| host_label(h, style)))
        .into_any_element()
}

fn history(style: HeaderStyle, cx: &mut Context<AgentPane>) -> AnyElement {
    keep_press(button("agent-history", "history", "Sessions  ⌘J", style.palette.icon, true, style))
        .on_click(cx.listener(|this, _, window, cx| this.open_sessions(window, cx)))
        .into_any_element()
}

fn new_session(style: HeaderStyle, cx: &mut Context<AgentPane>) -> AnyElement {
    keep_press(button("agent-new", "new-session", "New Session  /new", style.palette.icon, true, style))
        .on_click(cx.listener(|this, _, window, cx| this.start_new_session(window, cx)))
        .into_any_element()
}
