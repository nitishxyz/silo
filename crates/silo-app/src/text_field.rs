//! A minimal single-line text field for the switcher and sheets: typing,
//! backspace, cmd-backspace, paste, and navigation keys reported as events.

use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, Render, SharedString, Window, div, prelude::*, px,
    rgb,
};

use crate::theme;

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
}

impl EventEmitter<FieldEvent> for TextField {}

impl TextField {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self { text: String::new(), placeholder: placeholder.into(), focus: cx.focus_handle() }
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        cx.emit(FieldEvent::Changed(self.text.clone()));
        cx.notify();
    }

    fn key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let k = &event.keystroke;
        let m = k.modifiers;
        match k.key.as_str() {
            "enter" => cx.emit(FieldEvent::Submit),
            "escape" => cx.emit(FieldEvent::Cancel),
            "up" => cx.emit(FieldEvent::Up),
            "down" => cx.emit(FieldEvent::Down),
            "tab" => cx.emit(if m.shift { FieldEvent::Up } else { FieldEvent::Down }),
            "backspace" => {
                if m.platform {
                    self.text.clear();
                } else if m.alt {
                    let trimmed = self.text.trim_end_matches(' ').len();
                    let cut = self.text[..trimmed].rfind(' ').map_or(0, |i| i + 1);
                    self.text.truncate(cut);
                } else {
                    self.text.pop();
                }
                cx.emit(FieldEvent::Changed(self.text.clone()));
            }
            _ if m.platform || m.control => {
                if m.platform && k.key == "v" {
                    if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                        self.text.extend(text.chars().filter(|c| !c.is_control()));
                        cx.emit(FieldEvent::Changed(self.text.clone()));
                    }
                    cx.notify();
                    return true;
                }
                return false;
            }
            _ => match k.key_char.as_deref() {
                Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => {
                    self.text.push_str(text);
                    cx.emit(FieldEvent::Changed(self.text.clone()));
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
            .on_action(cx.listener(|this, _: &crate::workspace::Paste, _, cx| {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    this.text.extend(text.chars().filter(|c| !c.is_control()));
                    cx.emit(FieldEvent::Changed(this.text.clone()));
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.key(event, cx) {
                    cx.stop_propagation();
                }
            }))
            .when(!empty, |el| el.child(div().text_color(rgb(theme::foreground())).child(self.text.clone())))
            .when(focused, |el| el.child(caret))
            .when(empty, |el| el.child(div().text_color(rgb(theme::muted())).child(self.placeholder.clone())))
    }
}
