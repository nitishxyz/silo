//! Settings controls modeled on the desktop-platform UI kit
//! (`native-ui` `Switch` and `SegmentedControl`): the same geometry, colors
//! roles and motion, without the kit's platform dependencies.
//!
//! - [`Toggle`]: capsule track that cross-fades to the accent, thumb that
//!   slides across with an ease-out-quint (180 ms).
//! - [`Segmented`]: recessed capsule track with an accent thumb that slides
//!   behind the selected segment on the platform spring (450 ms, slight
//!   overshoot absorbed as a squash at the track ends).
//!
//! Both animate only on a value change (a remembered previous value, kept
//! per control in window element state), never on first render, and GPUI's
//! `with_animation` renders the end state directly under Reduce Motion.

use std::{cell::Cell, rc::Rc, time::Duration};

use gpui::{
    Animation, AnimationExt as _, App, ElementId, FocusHandle, IntoElement, RenderOnce, Role, SharedString, Toggled,
    Window, div, ease_out_quint, prelude::*, px, relative, rgb,
};

use crate::theme::Continuous as _;
use crate::theme;

/// The kit's `SWITCH_ANIMATION_DURATION`.
const SWITCH_DURATION: Duration = Duration::from_millis(180);
/// The kit's `MotionDuration::Settle`, paired with the spring easing.
const SPRING_DURATION: Duration = Duration::from_millis(450);
/// Design-token spring (unit mass), underdamped: overshoots a few percent.
const SPRING_STIFFNESS: f32 = 520.0;
const SPRING_DAMPING: f32 = 32.0;

const TRACK_W: f32 = 38.0;
const TRACK_H: f32 = 22.0;
const THUMB: f32 = 14.0;
const THUMB_INSET: f32 = 3.0;

const SEGMENTED_H: f32 = 28.0;
const SEGMENTED_PADDING: f32 = 3.0;

type ToggleHandler = Rc<dyn Fn(bool, &mut Window, &mut App)>;
type SelectHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;

fn spring_position(t: f32) -> f32 {
    let decay = SPRING_DAMPING / 2.0;
    let omega = SPRING_STIFFNESS.sqrt();
    let damped = (omega * omega - decay * decay).sqrt();
    1.0 - (-decay * t).exp() * (damped * t).cos().mul_add(1.0, (decay / damped) * (damped * t).sin())
}

/// The token spring normalized to end exactly at 1.0.
fn spring(linear: f32) -> f32 {
    let window = SPRING_DURATION.as_secs_f32();
    spring_position(linear.clamp(0.0, 1.0) * window) / spring_position(window)
}

fn is_activation_key(key: &str) -> bool {
    matches!(key, "enter" | "space")
}

/// The previously rendered value of one control, so a change animates from
/// where the control was and a first render does not animate at all.
#[derive(Default)]
struct Slide {
    initialized: Cell<bool>,
    from: Cell<usize>,
    current: Cell<usize>,
    /// Bumped per change so each slide gets a fresh animation identity.
    generation: Cell<u64>,
}

impl Slide {
    fn origin(&self, value: usize) -> (usize, u64) {
        if !self.initialized.get() {
            self.initialized.set(true);
            self.from.set(value);
            self.current.set(value);
        } else if self.current.get() != value {
            self.from.set(self.current.get());
            self.current.set(value);
            self.generation.set(self.generation.get() + 1);
        }
        (self.from.get(), self.generation.get())
    }
}

fn slide_state(id: &ElementId, window: &mut Window, cx: &mut App) -> Rc<Slide> {
    window.use_keyed_state((id.clone(), "slide"), cx, |_, _| Rc::new(Slide::default())).read(cx).clone()
}

/// One focus handle per segment, so arrow keys move focus with the selection.
fn segment_focus(id: &ElementId, window: &mut Window, cx: &mut App) -> Rc<Vec<FocusHandle>> {
    window
        .use_keyed_state((id.clone(), "segment-focus"), cx, |_, cx| {
            Rc::new((0..3).map(|_| cx.focus_handle().tab_index(0)).collect::<Vec<_>>())
        })
        .read(cx)
        .clone()
}

/// An on/off switch.
#[derive(IntoElement)]
pub struct Toggle {
    id: ElementId,
    label: SharedString,
    on: bool,
    disabled: bool,
    on_toggle: Option<ToggleHandler>,
}

impl Toggle {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, on: bool) -> Self {
        Self { id: id.into(), label: label.into(), on, disabled: false, on_toggle: None }
    }

    #[must_use]
    #[allow(dead_code, reason = "kit API parity; no setting is conditionally disabled yet")]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    #[must_use]
    pub fn on_toggle(mut self, handler: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_toggle = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Toggle {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let on = self.on;
        let (from, generation) = slide_state(&self.id, window, cx).origin(usize::from(on));
        let animate = from != usize::from(on);
        let accent = theme::accent();

        let active_track = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .capsule()
            .border_1()
            .border_color(rgb(accent))
            .bg(gpui::linear_gradient(
                180.0,
                gpui::linear_color_stop(rgb(theme::accent_highlight()), 0.0),
                gpui::linear_color_stop(rgb(accent), 1.0),
            ));
        let thumb_fill = div().size_full().capsule().bg(rgb(theme::on_accent()));
        let thumb = div().absolute().top(px(THUMB_INSET)).size(px(THUMB)).capsule().bg(rgb(theme::muted()));
        let travel = TRACK_W - 2.0 * THUMB_INSET - THUMB - 2.0;
        let fraction = move |progress: f32| if on { progress } else { 1.0 - progress };

        let (active_track, thumb) = if animate {
            let animation = || Animation::new(SWITCH_DURATION).with_easing(ease_out_quint());
            let key = |part: &str| ElementId::Name(format!("toggle-{part}-{generation}").into());
            (
                active_track.with_animation(key("track"), animation(), move |el, d| el.opacity(fraction(d))).into_any_element(),
                thumb
                    .child(thumb_fill.with_animation(key("fill"), animation(), move |el, d| el.opacity(fraction(d))))
                    .with_animation(key("thumb"), animation(), move |el, d| el.left(px(THUMB_INSET + travel * fraction(d))))
                    .into_any_element(),
            )
        } else {
            let level = if on { 1.0 } else { 0.0 };
            (
                active_track.opacity(level).into_any_element(),
                thumb.left(px(THUMB_INSET + travel * level)).child(thumb_fill.opacity(level)).into_any_element(),
            )
        };

        let track = div()
            .w(px(TRACK_W))
            .h(px(TRACK_H))
            .relative()
            .capsule()
            .border_1()
            .border_color(rgb(theme::border()))
            .bg(rgb(theme::tab_active()))
            .child(active_track)
            .child(thumb);

        let handler = self.on_toggle;
        div()
            .id(self.id)
            .role(Role::Switch)
            .aria_label(self.label)
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
            // Hit area larger than the track, focus ring around it.
            .p(px(2.0))
            .continuous(13.0)
            .border_2()
            .border_color(rgb(accent).opacity(0.0))
            .when(!self.disabled, |control| {
                let click = handler.clone();
                let a11y = handler.clone();
                control
                    .tab_index(0)
                    .focus_visible(move |s| s.border_color(rgb(accent)))
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.84))
                    .on_click(move |_, window, cx| {
                        if let Some(h) = &click {
                            h(!on, window, cx);
                        }
                    })
                    .on_a11y_action(gpui::AccessibleAction::Click, move |_, window, cx| {
                        if let Some(h) = &a11y {
                            h(!on, window, cx);
                        }
                    })
                    .on_key_down(move |event, window, cx| {
                        if is_activation_key(&event.keystroke.key) {
                            cx.stop_propagation();
                            if let Some(h) = &handler {
                                h(!on, window, cx);
                            }
                        }
                    })
            })
            .when(self.disabled, |control| control.opacity(0.38))
            .child(track)
    }
}

/// A choice between two or three options.
#[derive(IntoElement)]
pub struct Segmented {
    id: ElementId,
    segments: Vec<SharedString>,
    selected: usize,
    segment_width: f32,
    on_select: Option<SelectHandler>,
}

impl Segmented {
    pub fn new(id: impl Into<ElementId>, segments: impl IntoIterator<Item = impl Into<SharedString>>, selected: usize) -> Self {
        let segments: Vec<SharedString> = segments.into_iter().map(Into::into).collect();
        debug_assert!((2..=3).contains(&segments.len()), "a segmented control holds 2 to 3 segments");
        Self { id: id.into(), segments, selected, segment_width: 84.0, on_select: None }
    }

    #[must_use]
    pub fn segment_width(mut self, width: f32) -> Self {
        self.segment_width = width;
        self
    }

    #[must_use]
    pub fn on_select(mut self, handler: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }
}

/// Left edge and width (fractions of one segment) of the thumb at `delta`
/// of a slide: spring overshoot past either end becomes a squash, so the
/// thumb never slides under the track border.
fn thumb_geometry(from: usize, to: usize, count: usize, delta: f32) -> (f32, f32) {
    #[allow(clippy::cast_precision_loss)]
    let (from, to, count) = (from as f32, to as f32, count as f32);
    let position = (to - from).mul_add(delta, from);
    let overshoot = (position - position.clamp(0.0, count - 1.0)).abs();
    (position.max(0.0), 1.0 - overshoot)
}

impl RenderOnce for Segmented {
    #[allow(clippy::cast_precision_loss)]
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let mut segments = self.segments;
        segments.truncate(3);
        let count = segments.len().max(1);
        let selected = self.selected.min(count - 1);
        let (from, generation) = slide_state(&self.id, window, cx).origin(selected);
        let focus = segment_focus(&self.id, window, cx);
        let accent = theme::accent();
        let count_f = count as f32;

        let thumb = div()
            .absolute()
            .top_0()
            .bottom_0()
            .w(relative(1.0 / count_f))
            .capsule()
            .border_1()
            .border_color(rgb(accent))
            .bg(gpui::linear_gradient(
                180.0,
                gpui::linear_color_stop(rgb(theme::accent_highlight()), 0.0),
                gpui::linear_color_stop(rgb(accent), 1.0),
            ));
        let thumb = if from == selected {
            thumb.left(relative(selected as f32 / count_f)).into_any_element()
        } else {
            thumb
                .with_animation(
                    ElementId::Name(format!("segmented-thumb-{generation}").into()),
                    Animation::new(SPRING_DURATION).with_easing(spring),
                    move |thumb, delta| {
                        let (left, width) = thumb_geometry(from, selected, count, delta);
                        thumb.left(relative(left / count_f)).w(relative(width / count_f))
                    },
                )
                .into_any_element()
        };

        let id = self.id.clone();
        let handler = self.on_select;
        let mut row = div().relative().size_full().flex().child(thumb);
        for (index, label) in segments.into_iter().enumerate() {
            let is_selected = index == selected;
            let click = handler.clone();
            let a11y = handler.clone();
            let keys = handler.clone();
            let focus_keys = focus.clone();
            // Roving tab stop: Tab reaches the selected segment, arrows the rest.
            let handle = focus[index].clone().tab_stop(is_selected);
            row = row.child(
                div()
                    .id((id.clone(), format!("segment-{index}")))
                    .role(Role::RadioButton)
                    .aria_label(label.clone())
                    .aria_toggled(if is_selected { Toggled::True } else { Toggled::False })
                    .flex_1()
                    .min_w_0()
                    .capsule()
                    .border_1()
                    .border_color(rgb(accent).opacity(0.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(px(6.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(12.5))
                    .text_color(rgb(if is_selected { theme::on_accent() } else { theme::muted() }))
                    .track_focus(&handle)
                    .focus_visible(move |s| s.border_color(rgb(theme::on_accent())))
                    .cursor_pointer()
                    .when(!is_selected, |s| s.hover(|s| s.text_color(rgb(theme::foreground()))))
                    .on_click(move |_, window, cx| {
                        if let (false, Some(h)) = (is_selected, &click) {
                            h(index, window, cx);
                        }
                    })
                    .on_a11y_action(gpui::AccessibleAction::Click, move |_, window, cx| {
                        if let (false, Some(h)) = (is_selected, &a11y) {
                            h(index, window, cx);
                        }
                    })
                    .on_key_down(move |event, window, cx| {
                        let key = event.keystroke.key.as_str();
                        let target = match key {
                            "left" => index.checked_sub(1),
                            "right" => (index + 1 < count).then_some(index + 1),
                            "home" => Some(0),
                            "end" => Some(count - 1),
                            _ if is_activation_key(key) => Some(index),
                            _ => None,
                        };
                        if let Some(target) = target {
                            cx.stop_propagation();
                            focus_keys[target].focus(window, cx);
                            if let (true, Some(h)) = (target != selected, &keys) {
                                h(target, window, cx);
                            }
                        }
                    })
                    .child(label),
            );
        }
        div()
            .id(self.id)
            .role(Role::RadioGroup)
            .flex_none()
            .h(px(SEGMENTED_H))
            .w(px(self.segment_width * count_f + (SEGMENTED_PADDING + 1.0) * 2.0))
            .p(px(SEGMENTED_PADDING))
            .capsule()
            .border_1()
            .border_color(rgb(theme::border()))
            .bg(rgb(theme::tab_active()))
            .child(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_render_does_not_animate_and_changes_slide_from_the_previous_value() {
        let slide = Slide::default();
        assert_eq!(slide.origin(1), (1, 0), "no slide on mount");
        assert_eq!(slide.origin(1), (1, 0));
        assert_eq!(slide.origin(0), (1, 1), "slides from the previous value");
        assert_eq!(slide.origin(0), (1, 1), "stable across the frames of one slide");
        let (_, first) = slide.origin(1);
        slide.origin(0);
        let (_, second) = slide.origin(1);
        assert_ne!(first, second, "a repeated transition gets a fresh animation");
    }

    #[test]
    fn spring_ends_at_rest_and_overshoots_once() {
        assert!(spring(0.0).abs() < 1e-6);
        assert!((spring(1.0) - 1.0).abs() < 1e-6);
        let peak = (0..=100).map(|i| spring(i as f32 / 100.0)).fold(0.0_f32, f32::max);
        assert!(peak > 1.0 && peak < 1.1, "slight overshoot: {peak}");
    }

    #[test]
    fn segmented_overshoot_squashes_instead_of_leaving_the_track() {
        // Past the last segment: pinned to the end, narrower by the excess.
        let (left, width) = thumb_geometry(0, 1, 2, 1.05);
        assert!((left - 1.05).abs() < 1e-6 && (width - 0.95).abs() < 1e-6);
        let (left, width) = thumb_geometry(1, 0, 2, 1.05);
        assert!(left.abs() < 1e-6 && (width - 0.95).abs() < 1e-6);
        assert_eq!(thumb_geometry(0, 2, 3, 0.5), (1.0, 1.0));
    }
}
