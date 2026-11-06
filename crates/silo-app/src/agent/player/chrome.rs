//! The player's chrome, torrz's look scaled to a pane: scrims, and a frosted dark bar (scrubber; play/pause,
//! ±5 s, time, volume, loop, Copy, Save…, fill), the same in light and dark themes.

use gpui::{ClickEvent, Div, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Stateful, div, prelude::*, px, relative};

use super::{Drag, Playback, Prefs, bubble_left, keys::SKIP, played, volume_glyph};
use crate::{
    agent::{AgentPane, audio, media},
    assets::icon,
    tooltip::Tooltip,
};

pub(in crate::agent) const BAR_INSET: f32 = 16.0;
const BAR_RADIUS: f32 = 22.0;
const BAR_PAD: f32 = 12.0;
const BAR_MAX_W: f32 = 760.0;
/// The scrubber's hit height, its track (hovered: thicker) and knob.
const HIT: f32 = 20.0;
const TRACK: f32 = 4.0;
const TRACK_HOVER: f32 = 6.0;
const THUMB: f32 = 12.0;
const ROW: f32 = 36.0;
/// The bar's height: border, padding, the scrubber, the gap, the transport row.
const BAR_H: f32 = 2.0 + (BAR_PAD - 2.0) + HIT + 2.0 + ROW + (BAR_PAD - 4.0);
/// Room under a fitted (not filling) video for the bar.
pub(in crate::agent) const BAR_ROOM: f32 = BAR_INSET + BAR_H + 8.0;
const SCRIM: f32 = 150.0;
const VOLUME_WIDTH: f32 = 96.0;
/// Narrower bars drop the volume slider, then the skips.
const ROOM_VOLUME: f32 = 560.0;
const ROOM_SKIP: f32 = 400.0;
const INK: u32 = 0x00f2_f2f2;

fn white(a: f32) -> Hsla {
    gpui::hsla(0.0, 0.0, 1.0, a)
}

/// An unframed round glyph button on the bar's glass.
pub(in crate::agent) fn bar_button(id: &'static str, glyph: &'static str, size: f32) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded_full()
        .cursor_pointer()
        .hover(|s| s.bg(white(0.12)))
        .active(|s| s.bg(white(0.2)))
        .child(icon(glyph, (size * 0.5).round(), INK).when(glyph == "play", |i| i.ml(px(1.5))))
}

/// A gradient from the edge (`top` or bottom) into the picture, under the chrome there.
pub(in crate::agent) fn scrim(top: bool) -> Div {
    let (angle, edge) = if top { (180.0, 0.5) } else { (0.0, 0.65) };
    div().absolute().left_0().right_0().h(px(SCRIM)).map(|el| if top { el.top_0() } else { el.bottom_0() }).bg(gpui::linear_gradient(
        angle,
        gpui::linear_color_stop(gpui::hsla(0.0, 0.0, 0.0, edge), 0.0),
        gpui::linear_color_stop(gpui::hsla(0.0, 0.0, 0.0, 0.0), 1.0),
    ))
}

/// Dark glass: blurred where the renderer can, else nearly opaque.
pub(in crate::agent) fn glass<E: Styled>(el: E) -> E {
    let el = el.border_1().border_color(white(0.1));
    if crate::surface::current().frosted() {
        el.bg(gpui::hsla(0.0, 0.0, 0.1, 0.58)).backdrop_blur(px(crate::surface::BLUR)).backdrop_saturation(crate::surface::SATURATION)
    } else {
        el.bg(gpui::hsla(0.0, 0.0, 0.1, 0.94))
    }
}

/// Where `x` falls on `bounds` (0–1).
#[allow(clippy::cast_possible_truncation)]
fn at(x: Pixels, bounds: gpui::Bounds<Pixels>) -> f32 {
    audio::fraction(f32::from(x), f32::from(bounds.origin.x), f32::from(bounds.size.width)) as f32
}

impl AgentPane {
    /// The bar, centred at the pane's bottom; `width` is the viewer's.
    pub(in crate::agent) fn player_bar(p: &Playback, prefs: Prefs, width: f32, cx: &mut gpui::Context<Self>) -> Div {
        use crate::theme::Continuous as _;
        let room = (width - 2.0 * BAR_INSET).min(BAR_MAX_W);
        let bar = glass(div())
            .id("player-bar")
            .occlude()
            .w_full()
            .max_w(px(BAR_MAX_W))
            .continuous(BAR_RADIUS)
            .px(px(BAR_PAD + 4.0))
            .pt(px(BAR_PAD - 2.0))
            .pb(px(BAR_PAD - 4.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if let Some(p) = this.playback() {
                    p.over_bar = *hovered;
                }
                cx.notify();
            }))
            .child(Self::scrubber(p, cx))
            .child(Self::transport(p, prefs, room, cx));
        div().absolute().left_0().right_0().bottom(px(BAR_INSET)).px(px(BAR_INSET)).flex().justify_center().child(bar)
    }

    /// The timeline: played fill, a knob and a time bubble while hovered or dragged; press or drag seeks.
    fn scrubber(p: &Playback, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let player = &p.video.player;
        let duration = player.duration();
        let done = played(p.drag, player.time(), duration);
        let dragging = matches!(p.drag, Some(Drag::Scrub { .. }));
        let active = p.hover.is_some() || dragging;
        let cell = p.track.clone();
        let track_w = f32::from(p.track.get().size.width);
        let bubble = (if dragging { Some(done) } else { p.hover }).zip(duration);
        let track = div()
            .relative()
            .w_full()
            .h(px(if active { TRACK_HOVER } else { TRACK }))
            .rounded_full()
            .bg(white(0.22))
            .child(gpui::canvas(move |b, _, _| cell.set(b), |_, _, _, _| {}).absolute().size_full())
            .child(div().absolute().top_0().left_0().h_full().w(relative(done)).rounded_full().bg(white(0.95)));
        let thumb = if active { THUMB } else { 0.0 };
        let knob =
            div().absolute().top(px((HIT - thumb) / 2.0)).left(relative(done)).ml(px(-thumb / 2.0)).size(px(thumb)).rounded_full().bg(white(1.0));
        div()
            .id("player-scrubber")
            .relative()
            .w_full()
            .h(px(HIT))
            .flex()
            .items_center()
            .cursor_pointer()
            .child(track)
            .child(knob)
            .children(bubble.map(|(at, d)| time_bubble(at, d, track_w)))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if let Some(p) = this.playback().filter(|_| !*hovered) {
                    p.hover = None;
                    cx.notify();
                }
            }))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if let Some(p) = this.playback() {
                    p.hover = Some(at(e.position.x, p.track.get()));
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.scrub_to(e.position.x, cx);
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }
}

/// The time under the pointer, over the track.
fn time_bubble(at: f32, duration: f64, track: f32) -> Div {
    const W: f32 = 52.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = (f64::from(at) * duration * 1000.0) as u64;
    let label = div()
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(6.0))
        .bg(gpui::hsla(0.0, 0.0, 0.0, 0.72))
        .border_1()
        .border_color(white(0.1))
        .text_size(px(11.5))
        .text_color(gpui::rgb(INK))
        .font_features(crate::agent::view::tabular())
        .child(media::clock(ms));
    div().absolute().bottom(px(HIT + 2.0)).left(px(bubble_left(at, W, track))).w(px(W)).flex().justify_center().child(label)
}

impl AgentPane {
    /// Play/pause (replay at the end), ±5 s, the time; volume, loop, Copy, Save…, fill. `room` is the bar's
    /// width: narrow bars drop the volume slider, then the skips.
    fn transport(p: &Playback, prefs: Prefs, room: f32, cx: &mut gpui::Context<Self>) -> Div {
        let player = &p.video.player;
        let (duration, playing, ended) = (player.duration(), player.playing(), player.ended());
        let shown = match (p.drag, duration) {
            (Some(Drag::Scrub { at, .. }), Some(d)) => f64::from(at) * d,
            _ => player.time(),
        };
        let (glyph, tip) = match (ended, playing) {
            (true, _) => ("rotate-ccw", "Replay  space"),
            (_, true) => ("pause", "Pause  space"),
            _ => ("play", "Play  space"),
        };
        let skip = |id: &'static str, glyph: &'static str, by: f64, cx: &mut gpui::Context<Self>| {
            bar_button(id, glyph, 32.0)
                .tooltip(Tooltip::text(if by < 0.0 { "Back 5 s  ←" } else { "Forward 5 s  →" }))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.player_skip(by, cx)))
        };
        let left = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(
                bar_button("player-play", glyph, 36.0)
                    .tooltip(Tooltip::text(tip))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.player_toggle(cx))),
            )
            .when(room >= ROOM_SKIP, |el| el.child(skip("player-back", "rotate-ccw", -SKIP, cx)).child(skip("player-forward", "rotate-cw", SKIP, cx)))
            .child(
                div()
                    .ml(px(6.0))
                    .text_size(px(12.0))
                    .text_color(white(0.78))
                    .font_features(crate::agent::view::tabular())
                    .whitespace_nowrap()
                    .child(audio::times(shown, duration)),
            );
        let level = if prefs.muted { 0.0 } else { prefs.volume };
        let right = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(
                bar_button("player-mute", volume_glyph(prefs.volume, prefs.muted), 32.0)
                    .tooltip(Tooltip::text(if prefs.muted { "Unmute  m" } else { "Mute  m" }))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.player_mute(cx))),
            )
            .when(room >= ROOM_VOLUME, |el| el.child(Self::volume_slider(p, level, cx)).child(div().w(px(6.0))))
            .child(
                bar_button("player-loop", "repeat", 32.0)
                    .when(prefs.looping, |b| b.bg(white(0.18)))
                    .tooltip(Tooltip::text(if prefs.looping { "Loop on  l" } else { "Loop  l" }))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.player_loop(cx))),
            )
            .child(div().flex_none().w(px(1.0)).h(px(16.0)).mx(px(4.0)).bg(white(0.16)))
            .child(
                bar_button("player-copy", "copy", 32.0)
                    .tooltip(Tooltip::text("Copy video  ⌘C"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy_viewed(cx))),
            )
            .child(
                div()
                    .id("player-save")
                    .flex_none()
                    .h(px(32.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .text_color(gpui::rgb(INK))
                    .hover(|s| s.bg(white(0.12)))
                    .active(|s| s.bg(white(0.2)))
                    .child("Save…")
                    .tooltip(Tooltip::text("Save  ⌘S"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_viewed(cx))),
            )
            .child(
                bar_button("player-fill", if prefs.fill { "shrink" } else { "expand" }, 32.0)
                    .tooltip(Tooltip::text(if prefs.fill { "Fit  f" } else { "Fill the pane  f" }))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.player_fill(cx))),
            );
        div().h(px(ROW)).flex().items_center().justify_between().gap(px(8.0)).child(left).child(right)
    }

    /// The volume slider: a press sets it and a drag follows (`viewer_drag`).
    fn volume_slider(p: &Playback, level: f32, cx: &mut gpui::Context<Self>) -> Stateful<Div> {
        let cell = p.level.clone();
        let dragging = p.drag == Some(Drag::Volume);
        div()
            .id("player-volume")
            .flex_none()
            .w(px(VOLUME_WIDTH))
            .h(px(24.0))
            .flex()
            .items_center()
            .cursor_pointer()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(if dragging { 6.0 } else { 4.0 }))
                    .rounded_full()
                    .bg(white(0.22))
                    .child(gpui::canvas(move |b, _, _| cell.set(b), |_, _, _, _| {}).absolute().size_full())
                    .child(div().absolute().top_0().left_0().h_full().w(relative(level)).rounded_full().bg(white(0.95))),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.volume_to(e.position.x, cx);
                    cx.stop_propagation();
                }),
            )
    }

    /// A round frosted replay button over the last frame.
    pub(in crate::agent) fn replay_disc(cx: &mut gpui::Context<Self>) -> Div {
        div().absolute().size_full().flex().items_center().justify_center().child(
            glass(div())
                .id("player-replay")
                .occlude()
                .size(px(60.0))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(gpui::hsla(0.0, 0.0, 0.2, 0.7)))
                .child(icon("rotate-ccw", 24.0, INK))
                .tooltip(Tooltip::text("Replay  space"))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.player_toggle(cx))),
        )
    }
}

impl AgentPane {
    /// Scrub to the track's `x`: pauses for the drag (and plays on release when it played).
    pub(in crate::agent) fn scrub_to(&mut self, x: Pixels, cx: &mut gpui::Context<Self>) {
        let now = std::time::Instant::now();
        let Some(p) = self.playback() else { return };
        let to = at(x, p.track.get());
        let resume = match p.drag {
            Some(Drag::Scrub { resume, .. }) => resume,
            _ => {
                let playing = p.video.player.playing();
                p.video.player.pause();
                playing
            }
        };
        p.drag = Some(Drag::Scrub { at: to, resume });
        if let Some(d) = p.video.player.duration() {
            p.seek(f64::from(to) * d, now);
        }
        cx.notify();
    }

    /// Volume at the slider's `x`.
    pub(in crate::agent) fn volume_to(&mut self, x: Pixels, cx: &mut gpui::Context<Self>) {
        let Some(p) = self.playback() else { return };
        p.drag = Some(Drag::Volume);
        let level = at(x, p.level.get());
        self.player_volume(level, cx);
    }

    /// A drag ends: a scrub plays on when it played.
    pub(in crate::agent) fn player_release(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(p) = self.playback() else { return };
        if let Some(Drag::Scrub { resume: true, .. }) = p.drag.take() {
            p.play(std::time::Instant::now());
        }
        cx.notify();
    }
}
