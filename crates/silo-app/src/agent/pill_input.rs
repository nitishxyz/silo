//! The pill input (`docs/AGENT_BLOCK.md` §4c): one single-line field in a
//! full-radius pill with a soft halo, a leading icon and a trailing circle
//! that submits. The circle stays dark until the value is valid, then
//! animates to the foreground. ⏎ submits only when valid, esc cancels.
//!
//! ```ignore
//! let input = cx.new(|cx| PillInput::new("Paste an API key", "key", cx).masked(cx));
//! cx.subscribe(&input, |this, _, event: &PillInputEvent, cx| …);
//! input.update(cx, |i, cx| i.set_validate(Some(Rc::new(|s| s.starts_with("sk-"))), cx));
//! ```

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Context, Div, ElementId, Entity, EventEmitter, FocusHandle, Hsla, IntoElement, Render, SharedString, Stateful, Subscription,
    Window, div, prelude::*, px, rgb,
};

use super::editor::{Editor, EditorColors, EditorEvent};
use crate::{assets::icon, motion, theme};

/// Shared metrics of every pill (this input and the composer), from the pill's outer edges.
pub const HEIGHT: f32 = 46.0;
pub const HALO: f32 = 3.0;
pub const CIRCLE: f32 = 36.0;
pub const BORDER: f32 = 1.0;
/// Center of the leading icon: the circle's center, mirrored.
pub const LEAD: f32 = INSET + CIRCLE / 2.0;
/// Where the text starts: past the leading circle, mirroring the trailing one.
pub const TEXT_X: f32 = INSET + CIRCLE + 10.0;
/// The circle's inset on every side it touches.
pub const INSET: f32 = (HEIGHT - CIRCLE) / 2.0;
const VALID: Duration = Duration::from_millis(180);

pub type Validate = Rc<dyn Fn(&str) -> bool>;

pub enum PillInputEvent {
    /// ⏎ or the circle, with a valid value.
    Submit(String),
    /// Esc.
    Cancel,
    Changed,
}

pub struct PillInput {
    editor: Entity<Editor>,
    icon: &'static str,
    validate: Option<Validate>,
    masked: bool,
    revealed: bool,
    loading: bool,
    /// Off: read-only and the circle stays dark (nothing to submit).
    enabled: bool,
    error: Option<SharedString>,
    /// Validity, and when it last changed (the circle's fill animates).
    valid: bool,
    valid_at: Option<Instant>,
    /// Reserve the error line so an error never moves what follows.
    reserve_error: bool,
    /// Off: no trailing circle (a secondary field; ⏎ still submits).
    submit_circle: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PillInputEvent> for PillInput {}

fn hsla(c: u32, alpha: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a *= alpha;
    h
}

fn ink(amount: f32) -> u32 {
    theme::mix(theme::background(), theme::foreground(), amount)
}

/// The halo around a pill; `child` is the pill from [`pill`].
pub fn halo(error: bool, child: impl IntoElement) -> Div {
    // Opaque, so what tucks behind (the composer's tray) never shows through.
    let fill = if error { theme::mix(theme::background(), theme::danger(), 0.12) } else { ink(if theme::is_light() { 0.03 } else { 0.035 }) };
    div().w_full().p(px(HALO)).rounded(px(HEIGHT / 2.0 + HALO)).bg(rgb(fill)).child(child)
}

/// The pill itself: border, fill and full radius (kept when it grows).
pub fn pill(focused: bool, error: bool) -> Div {
    let border = if error { hsla(theme::danger(), 0.8) } else { rgb(ink(if focused { 0.3 } else { 0.2 })).into() };
    let fill = ink(if theme::is_light() { 0.065 } else { 0.08 });
    let mut el = div().relative().w_full().min_h(px(HEIGHT)).rounded(px(HEIGHT / 2.0)).border_color(border).bg(rgb(fill));
    let w: gpui::AbsoluteLength = px(BORDER).into();
    let b = &mut el.style().border_widths;
    (b.top, b.right, b.bottom, b.left) = (Some(w), Some(w), Some(w), Some(w));
    el
}

/// Fill and glyph color of the circle: `t` 0 = dark disc and muted glyph
/// (nothing to submit), 1 = foreground disc and background glyph.
pub fn circle_colors(t: f32) -> (Hsla, u32) {
    (hsla(theme::foreground(), 0.08 + 0.92 * t), theme::mix(theme::muted(), theme::background(), t))
}

/// The trailing circle; clickable when `enabled`.
pub fn circle(id: impl Into<ElementId>, t: f32, enabled: bool, glyph: AnyElement) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .size(px(CIRCLE))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(circle_colors(t).0)
        .child(glyph)
        .when(enabled, |el| el.cursor_pointer().hover(|s| s.opacity(0.88)).active(|s| s.scale(0.94)))
}

/// The leading circle (the composer's `⋮`): the trailing circle's size, filled only on hover/press.
pub fn lead_button(id: impl Into<ElementId>, name: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .size(px(CIRCLE))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .hover(|s| s.bg(theme::hover_fill()))
        .active(|s| s.bg(theme::hover_fill()).scale(0.94))
        .child(icon(name, 16.0, theme::muted()))
}

impl PillInput {
    pub fn new(placeholder: impl Into<SharedString>, icon: &'static str, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut e = Editor::new(placeholder, cx);
            e.single_line = true;
            e
        });
        let subscriptions =
            vec![cx.observe(&editor, |_, _, cx| cx.notify()), cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| this.editor_event(event, cx))];
        Self {
            editor,
            icon,
            validate: None,
            masked: false,
            revealed: false,
            loading: false,
            enabled: true,
            error: None,
            valid: false,
            valid_at: None,
            reserve_error: true,
            submit_circle: true,
            _subscriptions: subscriptions,
        }
    }

    /// Dots instead of the text, with an eye toggle to reveal it.
    #[must_use]
    pub fn masked(mut self, cx: &mut Context<Self>) -> Self {
        self.masked = true;
        self.editor.update(cx, |e, _| e.mask = true);
        self
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus.clone()
    }

    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).text().trim().to_owned()
    }

    /// Clears the value, error and reveal.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.loading = false;
        self.enabled = true;
        self.revealed = false;
        let mask = self.masked;
        self.editor.update(cx, |e, cx| {
            e.read_only = false;
            e.mask = mask;
            e.set_text("", cx);
        });
        self.refresh_valid(cx);
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| e.set_placeholder(placeholder, cx));
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| e.set_text(text, cx));
        self.refresh_valid(cx);
    }

    pub fn set_submit_circle(&mut self, on: bool, cx: &mut Context<Self>) {
        self.submit_circle = on;
        cx.notify();
    }

    /// `None`: any non-empty value is valid.
    pub fn set_validate(&mut self, validate: Option<Validate>, cx: &mut Context<Self>) {
        self.validate = validate;
        self.refresh_valid(cx);
    }

    /// Spinner in the circle; the field is read-only meanwhile.
    pub fn set_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        if self.loading != loading {
            self.loading = loading;
            let enabled = self.enabled;
            self.editor.update(cx, |e, _| e.read_only = loading || !enabled);
            cx.notify();
        }
    }

    pub fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.enabled != enabled {
            self.enabled = enabled;
            let loading = self.loading;
            self.editor.update(cx, |e, _| e.read_only = loading || !enabled);
            self.refresh_valid(cx);
            cx.notify();
        }
    }

    pub fn set_error(&mut self, error: Option<SharedString>, cx: &mut Context<Self>) {
        if self.error != error {
            self.error = error;
            cx.notify();
        }
    }

    pub fn is_valid(&self, cx: &App) -> bool {
        let text = self.text(cx);
        !text.is_empty() && self.validate.as_ref().is_none_or(|v| v(&text))
    }

    fn refresh_valid(&mut self, cx: &mut Context<Self>) {
        let valid = self.enabled && self.is_valid(cx);
        if valid != self.valid {
            self.valid = valid;
            self.valid_at = Some(Instant::now());
            cx.notify();
        }
    }

    fn editor_event(&mut self, event: &EditorEvent, cx: &mut Context<Self>) {
        match event {
            EditorEvent::Changed => {
                self.error = None;
                self.refresh_valid(cx);
                cx.emit(PillInputEvent::Changed);
            }
            EditorEvent::Submit => self.submit(cx),
            EditorEvent::Escape => cx.emit(PillInputEvent::Cancel),
            EditorEvent::Moved | EditorEvent::Nav(_) | EditorEvent::Files(_) | EditorEvent::Image(_) => {}
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.enabled && !self.loading && self.is_valid(cx) {
            cx.emit(PillInputEvent::Submit(self.text(cx)));
        }
    }

    fn toggle_reveal(&mut self, cx: &mut Context<Self>) {
        self.revealed = !self.revealed;
        let mask = !self.revealed;
        self.editor.update(cx, |e, cx| {
            e.mask = mask;
            cx.notify();
        });
    }
}

impl Render for PillInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        // 0 = invalid (dark circle), 1 = valid (foreground circle).
        let p = self.valid_at.filter(|_| !cx.reduce_motion()).and_then(|at| motion::progress(at, VALID, now)).map(motion::ease_out);
        if p.is_some() {
            window.request_animation_frame();
        }
        let t = match (self.valid, p) {
            (true, Some(p)) => p,
            (false, Some(p)) => 1.0 - p,
            (valid, None) => f32::from(u8::from(valid)),
        };
        let focus = self.editor.read(cx).focus.clone();
        let focused = focus.contains_focused(window, cx);
        let error = self.error.is_some();
        let muted = theme::muted();
        let arrow = circle_colors(t).1;
        let colors = EditorColors {
            text: rgb(theme::foreground()).into(),
            placeholder: rgb(muted).into(),
            caret: rgb(theme::accent()).into(),
            selection: hsla(theme::selection(), 0.6),
        };
        let enabled = self.valid && !self.loading;
        let glyph = if self.loading {
            crate::browser_pane::spinner("pill-input", 16.0, arrow, cx)
        } else {
            icon("arrow-right", 16.0, arrow).into_any_element()
        };
        let submit = circle("pill-submit", t, enabled, glyph).when(enabled, |el| el.on_click(cx.listener(|this, _, _, cx| this.submit(cx))));
        let reveal = self.masked.then(|| {
            div()
                .id("pill-reveal")
                .flex_none()
                .size(px(28.0))
                .mr(px(6.0))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_fill()))
                .child(icon(if self.revealed { "eye-off" } else { "eye" }, 16.0, muted))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_reveal(cx)))
        });
        let field_focus = focus.clone();
        let pill = pill(focused, error)
            .h(px(HEIGHT))
            .pl(px(LEAD - 8.0 - BORDER))
            .pr(px(INSET - BORDER))
            .flex()
            .items_center()
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| window.focus(&field_focus, cx))
            .child(div().flex_none().w(px(TEXT_X - LEAD + 8.0)).child(icon(self.icon, 16.0, muted)))
            .child(div().flex_1().min_w_0().mr(px(8.0)).text_size(px(15.0)).child(Editor::element(&self.editor, 1, colors, cx)))
            .children(reveal)
            .when(self.submit_circle, |el| el.child(submit))
            .when(!self.submit_circle, |el| el.child(div().flex_none().h(px(CIRCLE))));
        div().w_full().flex().flex_col().child(halo(error, pill)).when(self.reserve_error || error, |el| {
            el.child(
                div()
                    .h(px(16.0))
                    .mt(px(4.0))
                    .px(px(HALO + TEXT_X))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.0))
                    .text_color(rgb(theme::danger()))
                    .children(self.error.clone()),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn icons_exist() {
        for name in ["arrow-right", "eye", "eye-off", "key"] {
            assert!(crate::assets::has_icon(name), "{name}");
        }
    }
}
