//! A minimal single-line text field for the switcher and sheets: typing,
//! backspace (cmd: line, option/ctrl: word), select all / copy / paste, and
//! navigation keys reported as events. ⌘A selects the whole text (there is
//! no partial selection); typing, Backspace or Paste then replace it.

use gpui::{Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, Render, SharedString, Window, div, prelude::*, px, rgb};

use crate::theme;
use crate::theme::Continuous as _;

pub enum FieldEvent {
    Changed(String),
    Submit,
    Cancel,
    Up,
    Down,
}

pub struct TextField {
    pub text: String,
    placeholder: SharedString,
    pub focus: FocusHandle,
    /// The whole text is selected (⌘A).
    all: bool,
}

/// One edit of the field's text.
#[derive(Debug, PartialEq, Eq)]
enum Edit<'a> {
    Insert(&'a str),
    DeleteChar,
    DeleteWord,
    DeleteLine,
}

/// Apply `edit`; with everything selected, any edit first clears the text.
fn apply(text: &mut String, all: &mut bool, edit: Edit<'_>) {
    if std::mem::take(all) {
        text.clear();
        if !matches!(edit, Edit::Insert(_)) {
            return;
        }
    }
    match edit {
        Edit::Insert(s) => text.extend(s.chars().filter(|c| !c.is_control())),
        Edit::DeleteChar => {
            text.pop();
        }
        Edit::DeleteWord => {
            let trimmed = text.trim_end_matches(' ').len();
            let cut = text[..trimmed].rfind(' ').map_or(0, |i| i + 1);
            text.truncate(cut);
        }
        Edit::DeleteLine => text.clear(),
    }
}

impl EventEmitter<FieldEvent> for TextField {}

impl TextField {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self { text: String::new(), placeholder: placeholder.into(), focus: cx.focus_handle(), all: false }
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    /// Replace the text without emitting `Changed`.
    pub fn replace_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        self.all = false;
        cx.notify();
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        self.all = false;
        cx.emit(FieldEvent::Changed(self.text.clone()));
        cx.notify();
    }

    /// Select the whole text (as ⌘A does).
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.all = !self.text.is_empty();
        cx.notify();
    }

    fn edit(&mut self, edit: Edit<'_>, cx: &mut Context<Self>) {
        apply(&mut self.text, &mut self.all, edit);
        cx.emit(FieldEvent::Changed(self.text.clone()));
        cx.notify();
    }

    fn key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let k = &event.keystroke;
        let m = k.modifiers;
        // Any key other than a bare modifier or a ⌘/⌃ chord drops the
        // select-all highlight; edits below consume it first.
        let selected = self.all;
        if !(m.platform || m.control) {
            self.all = false;
        }
        match k.key.as_str() {
            "enter" => cx.emit(FieldEvent::Submit),
            "escape" => cx.emit(FieldEvent::Cancel),
            "up" => cx.emit(FieldEvent::Up),
            "down" => cx.emit(FieldEvent::Down),
            "tab" => cx.emit(if m.shift { FieldEvent::Up } else { FieldEvent::Down }),
            "backspace" => {
                self.all = selected;
                let edit = if m.platform {
                    Edit::DeleteLine
                } else if m.alt || m.control {
                    Edit::DeleteWord
                } else {
                    Edit::DeleteChar
                };
                self.edit(edit, cx);
                return true;
            }
            _ if m.platform || m.control => {
                if m.platform && k.key == "v" {
                    if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                        self.edit(Edit::Insert(&text), cx);
                    }
                    return true;
                }
                return false;
            }
            _ => match k.key_char.as_deref() {
                Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => {
                    self.all = selected;
                    let text = text.to_owned();
                    self.edit(Edit::Insert(&text), cx);
                    return true;
                }
                _ => return false,
            },
        }
        cx.notify();
        true
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus.is_focused(window);
        let empty = self.text.is_empty();
        let caret = div().w(px(1.5)).h(px(15.0)).bg(rgb(theme::accent())).flex_none();
        div()
            .id("text-field")
            .key_context("TextField")
            .track_focus(&self.focus)
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .flex_1()
            .overflow_hidden()
            .whitespace_nowrap()
            .on_action(cx.listener(|this, _: &crate::commands::act::Paste, _, cx| {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    this.edit(Edit::Insert(&text), cx);
                }
            }))
            // Handled here, the chords never reach the terminal behind.
            .on_action(cx.listener(|this, _: &crate::commands::act::SelectAll, _, cx| {
                this.all = !this.text.is_empty();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &crate::commands::act::Copy, _, cx| {
                if this.all {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(this.text.clone()));
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.key(event, cx) {
                    cx.stop_propagation();
                }
            }))
            .when(!empty, |el| {
                el.child(
                    div()
                        .text_color(rgb(theme::foreground()))
                        .when(self.all, |el| el.continuous(2.0).bg(rgb(theme::selection()).opacity(0.55)))
                        .child(self.text.clone()),
                )
            })
            .when(focused && !self.all, |el| el.child(caret))
            .when(empty, |el| el.child(div().text_color(rgb(theme::muted())).child(self.placeholder.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_replace_a_select_all() {
        let (mut text, mut all) = ("spl right".to_owned(), false);
        apply(&mut text, &mut all, Edit::DeleteWord);
        assert_eq!(text, "spl ");
        apply(&mut text, &mut all, Edit::Insert("ri\u{7}g"));
        assert_eq!(text, "spl rig", "control characters are dropped");
        all = true;
        apply(&mut text, &mut all, Edit::Insert("x"));
        assert_eq!((text.as_str(), all), ("x", false), "typing replaces the selection");
        all = true;
        apply(&mut text, &mut all, Edit::DeleteChar);
        assert_eq!((text.as_str(), all), ("", false), "backspace deletes the selection, nothing more");
        text.push_str("ab");
        apply(&mut text, &mut all, Edit::DeleteChar);
        assert_eq!(text, "a");
        apply(&mut text, &mut all, Edit::DeleteLine);
        assert_eq!(text, "");
    }
}
