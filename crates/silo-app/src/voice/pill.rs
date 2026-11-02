//! The voice pill: a small frosted pill near the bottom of the focused pane,
//! with voice-driven bars, a lock while hands-free, live words, then
//! the bars easing into a wave that sweeps left to right and back while it
//! transcribes (or polishes), and a fade.

use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, FontWeight, IntoElement, TextRun, Window, div, prelude::*, px, rgb, rgba};
use silo_voice::bands::{self, BARS};

use super::{LEAVE, Stage};
use crate::{assets::icon, surface::Glassy as _, theme, workspace::Workspace};

const HEIGHT: f32 = 30.0;
const BAR_W: f32 = 2.5;
const BAR_GAP: f32 = 2.5;
const BAR_MIN: f32 = 2.5;
const BAR_MAX: f32 = 18.0;
/// Above the pane's bottom edge.
const LIFT: f32 = 18.0;
const APPEAR: Duration = Duration::from_millis(180);
const COLLAPSE: Duration = Duration::from_millis(320);
/// The pill's widest, and what it gives the bars, lock and padding.
const MAX_W: f32 = 460.0;
/// Between the bars and the words.
const GAP: f32 = 8.0;
const TEXT_SIZE: f32 = 12.0;
/// The words' width and scroll follow their targets as a critically damped spring of this smoothing time.
/// Live words arrive in bursts about a second apart (`silo_voice`'s `LIVE_EVERY`): a quick ease raced
/// to each burst and then sat still, a stepping motion. This spreads a burst over most of the wait, and
/// keeps its speed when the next comes, so the words flow.
const SMOOTH: f32 = 0.3;
/// A frame longer than this (a hitch) moves as if it were this long, rather than leaping.
const MAX_DT: f32 = 1.0 / 30.0;

/// One step of a critically damped spring from `x` (speed `v`) toward `to` over `dt` (the game-dev
/// `SmoothDamp`): the speed carries over when the target moves, so motion never restarts from rest.
fn follow(x: f32, v: &mut f32, to: f32, dt: f32) -> f32 {
    let omega = 2.0 / SMOOTH;
    let k = omega * dt;
    let decay = 1.0 / (1.0 + k + 0.48 * k * k + 0.235 * k * k * k);
    let change = x - to;
    let temp = (*v + omega * change) * dt;
    *v = (*v - omega * temp) * decay;
    let out = to + (change + temp) * decay;
    if (out - to).abs() < 0.25 && v.abs() < 2.0 {
        *v = 0.0;
        return to;
    }
    out
}

/// The live words as drawn: the box widens and the text slides left as they grow, rather than jumping.
#[derive(Default)]
pub(super) struct Words {
    text: String,
    /// The text's laid out width.
    width: f32,
    /// The box's width (the words' share, without the gap), eased.
    view: f32,
    /// How far the text is slid left in it, eased.
    scroll: f32,
    /// Their speeds (points a second).
    view_v: f32,
    scroll_v: f32,
    at: Option<Instant>,
}

impl Words {
    /// Takes `text` (measured by `measure` when it changes; none: the words are going) and steps the motion
    /// toward a box `cap` wide at most showing the text's end.
    fn step(&mut self, text: Option<&str>, measure: impl FnOnce(&str) -> f32, cap: f32, reduce: bool) {
        let now = Instant::now();
        match text {
            Some(text) if text != self.text => {
                let width = measure(text);
                // A revision (not more words after the old ones) keeps what shows of the end where it is.
                if !text.starts_with(self.text.as_str()) {
                    self.scroll = (self.scroll + width - self.width).max(0.0);
                }
                (self.text, self.width) = (text.to_owned(), width);
            }
            _ => {}
        }
        let view_to = if text.is_some() { self.width.min(cap.max(0.0)) } else { 0.0 };
        let scroll_to = if text.is_some() { (self.width - view_to).max(0.0) } else { self.scroll };
        let dt = self.at.map_or(0.0, |at| now.duration_since(at).as_secs_f32()).min(MAX_DT);
        self.at = Some(now);
        if reduce {
            (self.view, self.scroll, self.view_v, self.scroll_v) = (view_to, scroll_to, 0.0, 0.0);
        } else if dt > 0.0 {
            self.view = follow(self.view, &mut self.view_v, view_to, dt).max(0.0);
            self.scroll = follow(self.scroll, &mut self.scroll_v, scroll_to, dt);
        }
        let max = (self.width - self.view).max(0.0);
        if !(0.0..=max).contains(&self.scroll) {
            self.scroll = self.scroll.clamp(0.0, max);
            self.scroll_v = 0.0;
        }
        if text.is_none() && self.view <= 0.0 {
            *self = Self::default();
        }
    }

    fn shown(&self) -> bool {
        self.view > 0.0
    }
}

fn ease_out(x: f32) -> f32 {
    1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3)
}

fn progress(since: Instant, over: Duration) -> f32 {
    since.elapsed().as_secs_f32() / over.as_secs_f32()
}

/// `color` at `alpha` (0..=1).
fn tint(color: u32, alpha: f32) -> gpui::Rgba {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    rgba((color << 8) | a)
}

impl Workspace {
    /// The pill while a dictation lasts; asks for frames meanwhile.
    pub(crate) fn render_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.voice_step(window, cx);
        let reduce = cx.reduce_motion();
        let t = self.voice.epoch.elapsed().as_secs_f32();
        let area = self.focused_block(window, cx).and_then(|b| self.block_bounds(&b, cx));
        let fetching = self.voice.live.as_ref().and_then(|l| super::fetch::fetch(l.engine));
        let live = self.voice.live.as_mut()?;
        window.request_animation_frame();
        let (heights, fade) = match live.stage {
            Stage::Recording => (live.history.heights(t, reduce), 1.0),
            Stage::Finishing(since) => {
                let frozen = *live.frozen.get_or_insert(live.heights);
                (bands::sweep(&frozen, ease_out(progress(since, COLLAPSE)), since.elapsed().as_secs_f32(), reduce), 1.0)
            }
            Stage::Leaving(since) => (live.heights, 1.0 - ease_out(progress(since, LEAVE))),
        };
        live.heights = heights;
        let appear = if reduce { 1.0 } else { ease_out(progress(live.started, APPEAR)) };
        let bg = theme::overlay();
        let ink = theme::ink_on(bg);
        let muted = theme::muted_on(bg);
        let accent = theme::accent();
        let finishing = matches!(live.stage, Stage::Finishing(_));
        let bars = div().flex().items_center().gap(px(BAR_GAP)).h(px(BAR_MAX)).children((0..BARS).map(|bar| {
            // The sweeping crest is the brightest part of the wave.
            // The timeline fades in from its oldest (left) end; the sweeping crest is the brightest.
            let alpha = if finishing { 0.3 + 0.7 * heights[bar].clamp(0.0, 1.0) } else { bands::age_opacity(bar) };
            div().w(px(BAR_W)).h(px(BAR_MIN + heights[bar] * (BAR_MAX - BAR_MIN))).rounded_full().bg(tint(accent, alpha))
        }));
        let words: Option<(String, u32)> = match live.stage {
            _ if live.waiting => Some((
                match fetching {
                    Some(super::fetch::Fetch::Running { done, total }) => match super::fetch::percent(done, total) {
                        Some(p) => format!("Downloading the voice model… {p}%"),
                        None => "Downloading the voice model…".into(),
                    },
                    _ => "Getting the voice model…".into(),
                },
                muted,
            )),
            Stage::Recording if !live.partial.is_empty() => Some((live.partial.clone(), muted)),
            _ => None,
        };
        let hands_free = live.hands_free && live.stage == Stage::Recording;
        // The words' room: the pill's widest less its padding, bars, lock and gap (and within the pane).
        #[allow(clippy::cast_precision_loss)]
        let bars_w = BARS as f32 * BAR_W + (BARS as f32 - 1.0) * BAR_GAP;
        let fixed = 28.0 + bars_w + GAP + if hands_free { 11.0 + GAP } else { 0.0 };
        let room = area.map_or(MAX_W, |b| f32::from(b.size.width) - 2.0 * LIFT).min(MAX_W);
        let mut font = window.text_style().font();
        font.weight = FontWeight::MEDIUM;
        let color = words.as_ref().map_or(muted, |(_, c)| *c);
        let measure = |text: &str| {
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: rgb(color).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            f32::from(window.text_system().shape_line(text.to_owned().into(), px(TEXT_SIZE), &[run], None).width()).ceil()
        };
        live.words.step(words.as_ref().map(|(t, _)| t.as_str()), measure, room - fixed, reduce);
        let w = &live.words;
        let words_el = w.shown().then(|| {
            let text = div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(-w.scroll))
                .flex()
                .items_center()
                .whitespace_nowrap()
                .font(font.clone())
                .text_color(rgb(color))
                .child(w.text.clone());
            // The gap rides with the words, so the pill closes up evenly when they go.
            div()
                .flex_none()
                .h_full()
                .w(px(w.view + GAP))
                .pl(px(GAP))
                .overflow_hidden()
                .child(div().relative().size_full().overflow_hidden().child(text))
        });
        let pill = div()
            .id("voice-pill")
            .h(px(HEIGHT))
            .flex()
            .items_center()
            .px(px(14.0))
            .glass(HEIGHT / 2.0)
            .rounded_full()
            .text_size(px(TEXT_SIZE))
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(ink))
            .when(hands_free, |el| el.child(div().mr(px(GAP)).child(icon("lock", 11.0, muted))))
            .child(bars)
            .children(words_el)
            .when(hands_free, |el| el.cursor_pointer().on_click(cx.listener(|this, _, _, cx| this.voice_stop(false, cx))));
        let rise = 6.0 * (1.0 - appear);
        let place = div().absolute().flex().justify_center().items_end().opacity(appear * fade);
        let place = match area {
            Some(b) => place.left(b.left()).top(b.top()).w(b.size.width).h(b.size.height - px(LIFT - rise)),
            None => place.inset_0().pb(px(LIFT - rise)),
        };
        Some(place.child(pill).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::Words;

    #[test]
    fn words_ease_toward_the_text_and_keep_its_end() {
        let mut w = Words::default();
        let measure = |t: &str| t.len() as f32 * 10.0;
        // Reduced motion snaps: fits, so no scroll.
        w.step(Some("hello"), measure, 100.0, true);
        assert_eq!((w.view, w.scroll), (50.0, 0.0));
        // Past the cap: the box stops at it and the text slides to its end.
        w.step(Some("hello there world"), measure, 100.0, true);
        assert_eq!((w.view, w.scroll), (100.0, 70.0));
        // Animated: the first step (no time yet) holds, then it moves part way, never past the target.
        let mut w = Words::default();
        w.step(Some("hello"), measure, 100.0, false);
        assert_eq!(w.view, 0.0);
        std::thread::sleep(std::time::Duration::from_millis(30));
        w.step(Some("hello"), measure, 100.0, false);
        assert!(w.view > 0.0 && w.view < 50.0, "{}", w.view);
        // Gone: it closes to nothing, then resets.
        w.step(None, measure, 100.0, true);
        assert!(!w.shown() && w.text.is_empty());
    }

    #[test]
    fn the_follow_keeps_its_speed_when_the_target_moves_on() {
        let (mut x, mut v) = (0.0, 0.0);
        for _ in 0..20 {
            x = super::follow(x, &mut v, 100.0, 1.0 / 60.0);
        }
        assert!(x > 0.0 && x < 100.0 && v > 0.0, "under way: {x} at {v}");
        // More words: it doesn't stop and restart, it carries on at about its speed.
        let before = v;
        x = super::follow(x, &mut v, 200.0, 1.0 / 60.0);
        assert!(v >= before * 0.9, "{before} → {v}");
        // It settles exactly, without overshooting from rest.
        for _ in 0..600 {
            let last = x;
            x = super::follow(x, &mut v, 200.0, 1.0 / 60.0);
            assert!(x >= last && x <= 200.0, "{last} → {x}");
        }
        assert_eq!((x, v), (200.0, 0.0));
    }
}
