//! The pane's side: the transcript wrapper (pointer, keys, ⌘C/⌘A), each row's drawing scope, autoscroll.

use std::time::Duration;

use gpui::{
    AnyElement, ClipboardItem, Context, DispatchPhase, EntityInputHandler as _, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Window, div, prelude::*, px,
};

use super::{
    copy_text, everything,
    pane::{Rows, edge_speed},
    ranges,
    view::{self, Guard},
};
use crate::{agent::AgentPane, commands::act, theme};

/// The selection's colour (the composer's).
pub(super) fn colour() -> Hsla {
    let mut c: Hsla = gpui::rgb(theme::selection()).into();
    c.a = 0.6;
    c
}

/// The pane's rows and parsed texts (a macro, so `self.select` stays free to borrow mutably).
macro_rules! rows {
    ($this:expr) => {
        Rows { rows: &$this.rows, parsed: &$this.parsed }
    };
}

impl AgentPane {
    /// The transcript `list`, selectable: a press starts a selection, a drag extends it (scrolling past an
    /// edge), and while one stays the transcript has focus for ⌘C and ⌘A (typing goes on to the composer).
    pub(in crate::agent) fn selectable(&mut self, list: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        if self.select.session.as_ref() != Some(&self.session_id) {
            self.select.session = Some(self.session_id.clone());
            self.select.clear();
        }
        self.select.registry.borrow_mut().entries.clear();
        let frame = self.select.sel.as_ref().and_then(|s| s.bounds(&rows!(self)));
        // Its text went away (rows rebuilt): nothing selected any more.
        if frame.is_none() && !self.select.dragging() {
            self.select.sel = None;
        }
        self.select.frame = frame;
        let (pane, viewport) = (cx.entity().downgrade(), self.select.viewport.clone());
        // Pointer moves and the release anywhere in the window, so a drag past the transcript keeps going.
        let events = gpui::canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                viewport.set(Some(bounds));
                let moves = pane.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble {
                        let _ = moves.update(cx, |this, cx| this.select_move(e, window, cx));
                    }
                });
                let ups = pane.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                        let _ = ups.update(cx, |this, cx| this.select_up(window, cx));
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .size_full()
            .relative()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.select.down(&rows!(this), e.position, e.click_count, e.modifiers.shift);
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(Self::select_key))
            .on_action(cx.listener(|this, _: &act::Copy, _, cx| match this.select.sel.as_ref().and_then(|s| copy_text(&rows!(this), s)) {
                Some(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                None => cx.propagate(),
            }))
            .on_action(cx.listener(|this, _: &act::SelectAll, _, cx| {
                this.select.sel = everything(&rows!(this));
                cx.notify();
            }))
            .child(div().absolute().size_0().track_focus(&self.select.focus))
            .child(list)
            .child(events)
            .into_any_element()
    }

    /// While alive, the texts drawn are row `ix`'s (key `key`): they register and paint their selected part.
    pub(in crate::agent) fn select_scope(&self, ix: usize, key: &str) -> Guard {
        let ranges = self.select.frame.map(|f| ranges(&rows!(self), f, ix)).unwrap_or_default();
        view::scope(self.select.registry.clone(), key.to_owned(), ranges, colour())
    }

    fn select_move(&mut self, e: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.select.dragging() {
            return;
        }
        if e.pressed_button != Some(MouseButton::Left) {
            self.select_up(window, cx);
            return;
        }
        // Past the top, or into the composer floating over the bottom: scroll that way.
        self.select.speed = self.select.viewport.get().map_or(0.0, |b| {
            let bottom = f32::from(b.bottom()) - self.composer_h.get();
            edge_speed(f32::from(e.position.y), f32::from(b.top()), bottom)
        });
        if self.select.drag_to(&rows!(self), e.position) {
            cx.notify();
        }
        if self.select.speed != 0.0 {
            self.select_autoscroll(cx);
        }
    }

    fn select_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.select.dragging() {
            return;
        }
        if self.select.up(&rows!(self)) {
            window.focus(&self.select.focus, cx);
        } else if self.select.focus.is_focused(window) {
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    /// A key while the transcript has focus: shortcuts go on as usual; Esc drops the selection; anything else
    /// (typing) goes to the composer.
    fn select_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        if k.modifiers.platform || k.modifiers.control || k.modifiers.function {
            return;
        }
        if k.key == "escape" {
            self.select.clear();
            cx.notify();
        }
        self.focus_composer(window, cx);
        if let Some(text) = k.key_char.as_deref().filter(|t| !t.is_empty() && !t.chars().any(char::is_control)) {
            self.editor.update(cx, |ed, cx| ed.replace_text_in_range(None, text, window, cx));
        }
        cx.stop_propagation();
    }

    /// While a drag is past an edge: scroll each frame and extend the selection to what comes into view.
    fn select_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.select.autoscroll.is_some() {
            return;
        }
        self.select.autoscroll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                let keep = this
                    .update(cx, |this, cx| {
                        if !this.select.dragging() || this.select.speed == 0.0 {
                            this.select.autoscroll = None;
                            return false;
                        }
                        this.list.scroll_by(px(this.select.speed));
                        let p = this.select.pointer;
                        this.select.drag_to(&rows!(this), p);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    return;
                }
            }
        }));
    }
}
