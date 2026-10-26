//! Renaming a session (`session.rename`): in place of the header's title
//! (double-click it, the `⋮` menu's Rename…, `/rename`) or of a switcher
//! row's (its pencil), and `/rename <title>` straight away.

use gpui::{AnyElement, ClickEvent, Context, Window, div, prelude::*, px, rgb};

use super::{AgentPane, Overlay};
use crate::{
    text_field::FieldEvent,
    theme::{self, Continuous as _},
};

/// A title being edited: whose, and where (the switcher's row, else the header).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Renaming {
    pub session: String,
    pub in_switcher: bool,
}

/// `/rename` with what follows it (trimmed; empty for a bare `/rename`), when the composer's text is that command.
pub(super) fn slash_rename(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix("/rename")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

/// The title to save from what was typed: trimmed, on one line; `None` (nothing to do) when it's empty or the
/// title it already has.
pub(super) fn rename_to(typed: &str, current: &str) -> Option<String> {
    let title = typed.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty() && title != current.trim()).then_some(title)
}

impl AgentPane {
    /// The title session `id` has now (as the switcher names it).
    fn title_of(&self, id: &str) -> String {
        if self.session_id.as_deref() == Some(id)
            && let Some(title) = self.header_title()
        {
            return title;
        }
        self.sessions.iter().find(|s| s.id == id).map(super::session_label).unwrap_or_default()
    }

    /// Edit session `id`'s title in place: its switcher row (`in_switcher`), else the header.
    pub(super) fn start_rename(&mut self, id: String, in_switcher: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !in_switcher && self.overlay.is_some() {
            self.close_overlay(window, cx);
        }
        let title = self.title_of(&id);
        self.renaming = Some(Renaming { session: id, in_switcher });
        let focus = self.rename_field.update(cx, |f, cx| {
            if in_switcher {
                f.set_palette(None, cx);
            }
            f.replace_text(title, cx);
            f.select_all(cx);
            f.focus.clone()
        });
        window.focus(&focus, cx);
        cx.emit(super::AgentPaneEvent::Header);
        cx.notify();
    }

    /// Rename the shown session in the header (double-click, Rename…, `/rename`).
    pub(super) fn rename_shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.session_id.clone() {
            Some(id) => self.start_rename(id, false, window, cx),
            None => {
                self.notice = Some("Nothing to rename yet: send a message first".into());
                cx.notify();
            }
        }
    }

    /// The title field's ⏎ saves, esc (or leaving it) cancels; an empty or unchanged title saves nothing.
    pub(super) fn rename_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            FieldEvent::Submit => {
                let Some(r) = self.renaming.clone() else { return };
                let typed = self.rename_field.read(cx).text.clone();
                if let Some(title) = rename_to(&typed, &self.title_of(&r.session)) {
                    self.rename(r.session, title, cx);
                }
                self.end_rename(window, cx);
            }
            FieldEvent::Cancel => self.end_rename(window, cx),
            FieldEvent::Changed(_) | FieldEvent::Up | FieldEvent::Down => {}
        }
    }

    /// Leave the title field: focus goes back to the switcher's filter, else the composer.
    fn end_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.renaming.take() else { return };
        if r.in_switcher && self.overlay == Some(Overlay::Sessions) {
            let focus = self.filter.read(cx).focus.clone();
            window.focus(&focus, cx);
        } else {
            self.focus_composer(window, cx);
        }
        cx.emit(super::AgentPaneEvent::Header);
        cx.notify();
    }

    /// Focus left the title field (a click elsewhere): nothing is saved.
    pub(super) fn rename_blurred(&mut self, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            cx.emit(super::AgentPaneEvent::Header);
            cx.notify();
        }
    }

    /// The switcher closed: a rename in its row ends.
    pub(super) fn switcher_closed(&mut self) {
        self.renaming.take_if(|r| r.in_switcher);
    }

    /// `session.rename`: shown at once (the header, the switcher's row); otto's `session.changed` confirms it, a
    /// refusal puts the old title back and says why.
    pub(super) fn rename(&mut self, id: String, title: String, cx: &mut Context<Self>) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
            s.title.clone_from(&title);
        }
        self.retitled = Some((id.clone(), title.clone()));
        cx.emit(super::AgentPaneEvent::Header);
        let asked = (id.clone(), title.clone());
        self.run(
            cx,
            move |c| async move { c.session_rename(&id, &title).await },
            move |this, result, cx| {
                if let Err(error) = result {
                    this.notice = Some(format!("Couldn't rename: {error}"));
                    this.retitled.take_if(|r| *r == asked);
                    cx.emit(super::AgentPaneEvent::Header);
                }
                this.refresh_sessions(cx);
            },
        );
        cx.notify();
    }

    /// The header's title field while the shown session is renamed there.
    pub(super) fn header_rename(&mut self, style: crate::pane_header::HeaderStyle, cx: &mut Context<Self>) -> Option<AnyElement> {
        let shown = self.session_id.as_deref()?;
        self.renaming.as_ref().filter(|r| !r.in_switcher && r.session == shown)?;
        self.rename_field.update(cx, |f, cx| f.set_palette(Some(style.palette), cx));
        let border = theme::mix(style.palette.surface, style.palette.ink, 0.35);
        Some(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .ml(px(-5.0))
                .child(crate::pane_header::keep_press(
                    div()
                        .id("agent-header-rename")
                        .flex()
                        .items_center()
                        .flex_1()
                        .min_w_0()
                        .max_w(px(420.0))
                        .h(px(crate::pane_header::BUTTON))
                        .px(px(5.0))
                        .continuous(style.control)
                        .border_1()
                        .border_color(rgb(border))
                        .text_color(rgb(style.ink()))
                        .child(self.rename_field.clone())
                        // A click in the field stays there (it doesn't reach the header behind).
                        .on_click(|_: &ClickEvent, _, cx| cx.stop_propagation()),
                ))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_rename_takes_the_rest_of_the_line() {
        assert_eq!(slash_rename("/rename Fix the login"), Some("Fix the login"));
        assert_eq!(slash_rename("  /rename   spaced out  "), Some("spaced out"));
        assert_eq!(slash_rename("/rename"), Some(""));
        assert_eq!(slash_rename("/renamed"), None, "another word");
        assert_eq!(slash_rename("rename it"), None);
        assert_eq!(slash_rename("/new"), None);
    }

    #[test]
    fn a_rename_saves_a_new_non_empty_title_on_one_line() {
        assert_eq!(rename_to("  New name ", "Old"), Some("New name".into()));
        assert_eq!(rename_to("two\nlines\tand  gaps", "Old"), Some("two lines and gaps".into()));
        // Empty: cancelled; unchanged: nothing to save.
        assert_eq!(rename_to("   ", "Old"), None);
        assert_eq!(rename_to("", "Old"), None);
        assert_eq!(rename_to("Old ", "Old"), None);
    }
}
