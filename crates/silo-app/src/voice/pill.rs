//! The voice pill: a small frosted pill near the bottom of the focused pane,
//! with voice-driven bars, a lock while hands-free, live words, then
//! "Transcribing…" as the bars collapse into a shimmer, and a fade.

use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, FontWeight, IntoElement, Window, div, prelude::*, px, rgb, rgba};
use silo_voice::bands::{self, BARS};

use super::{LEAVE, Stage};
use crate::{assets::icon, surface::Glassy as _, theme, workspace::Workspace};

const HEIGHT: f32 = 34.0;
const BAR_W: f32 = 3.0;
const BAR_GAP: f32 = 2.5;
const BAR_MIN: f32 = 3.0;
const BAR_MAX: f32 = 22.0;
/// Above the pane's bottom edge.
const LIFT: f32 = 18.0;
const APPEAR: Duration = Duration::from_millis(180);
const COLLAPSE: Duration = Duration::from_millis(320);

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
            Stage::Recording => (bands::bar_heights(&live.levels, t, reduce), 1.0),
            Stage::Finishing(since) => {
                let frozen = *live.frozen.get_or_insert(live.heights);
                (bands::shimmer(&frozen, ease_out(progress(since, COLLAPSE)), t, reduce), 1.0)
            }
            Stage::Leaving(since) => (live.heights, 1.0 - ease_out(progress(since, LEAVE))),
        };
        live.heights = heights;
        let appear = if reduce { 1.0 } else { ease_out(progress(live.started, APPEAR)) };
        let bg = theme::overlay();
        let ink = theme::ink_on(bg);
        let muted = theme::muted_on(bg);
        let accent = theme::accent();
        let bars = div().flex().items_center().gap(px(BAR_GAP)).h(px(BAR_MAX)).children((0..BARS).map(|bar| {
            div().w(px(BAR_W)).h(px(BAR_MIN + heights[bar] * (BAR_MAX - BAR_MIN))).rounded_full().bg(tint(accent, bands::bar_opacity(bar)))
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
            Stage::Finishing(_) if live.polishing => Some(("Polishing…".into(), muted)),
            Stage::Finishing(_) => Some(("Transcribing…".into(), muted)),
            _ => None,
        };
        let hands_free = live.hands_free && live.stage == Stage::Recording;
        let pill = div()
            .id("voice-pill")
            .h(px(HEIGHT))
            .max_w(px(460.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(14.0))
            .glass(HEIGHT / 2.0)
            .rounded_full()
            .text_size(px(12.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(ink))
            .when(hands_free, |el| el.child(icon("lock", 11.0, muted)))
            .child(bars)
            .children(words.map(|(text, color)| {
                // The latest words stay in view: long text keeps its end.
                let shown: String = if text.chars().count() > 60 {
                    format!("…{}", text.chars().rev().take(58).collect::<Vec<_>>().into_iter().rev().collect::<String>())
                } else {
                    text
                };
                div().min_w_0().truncate().text_color(rgb(color)).child(shown)
            }))
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
