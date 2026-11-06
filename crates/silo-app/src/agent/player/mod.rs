//! The viewer's video player (`docs/MEDIA.md` §6.4), drawn by gpui: frames come from `av::Video` as
//! IOSurface-backed pixel buffers (a `surface` element), the controls are gpui (a frosted bar: scrubber,
//! play/pause, ±5 s, time, volume, loop, Copy, Save…, fill). A poll on the main thread wakes the pane only
//! when AVFoundation has a newer frame (so a 24 fps clip redraws 24 times a second), catches the end
//! (stop, or loop), hides the chrome and pointer when idle, and pauses once the viewer isn't drawn.

mod autohide;
mod chrome;
mod keys;

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{Bounds, Pixels};

pub(super) use autohide::AutoHide;
pub(super) use chrome::{BAR_ROOM, scrim};
pub(super) use keys::{Key, map};

use super::{AgentPane, av, media::Clip};

/// How often the poll looks: playing or settling a seek, and otherwise.
const POLL: Duration = Duration::from_millis(8);
const POLL_IDLE: Duration = Duration::from_millis(100);
/// A seek's frame is waited for this fast.
const SETTLE: Duration = Duration::from_millis(600);
/// The longest a playing video goes undrawn while shown.
const TICK: Duration = Duration::from_millis(100);
/// Not drawn for this long (another tab, the pane hidden or gone): paused.
const STALE: Duration = Duration::from_millis(250);

/// What carries from one video to the next while the viewer is open.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Prefs {
    pub volume: f32,
    pub muted: bool,
    pub looping: bool,
    /// The picture fills the pane, the chrome over it.
    pub fill: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { volume: 1.0, muted: false, looping: false, fill: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Drag {
    /// Scrubbing at a fraction; play again on release.
    Scrub {
        at: f32,
        resume: bool,
    },
    Volume,
}

/// The shown video's player and its chrome's state.
pub(super) struct Playback {
    pub id: String,
    pub video: av::Video,
    pub hide: AutoHide,
    /// The scrubber's hovered fraction.
    pub hover: Option<f32>,
    pub drag: Option<Drag>,
    pub over_bar: bool,
    pub pointer_in: bool,
    /// The scrubber's track and the volume slider, as last laid out.
    pub track: Rc<Cell<Bounds<Pixels>>>,
    pub level: Rc<Cell<Bounds<Pixels>>>,
    /// Played once the viewer finished opening (the poster stands in until then).
    pub started: bool,
    /// Last drawn, and until when a seek is settling.
    pub painted: Instant,
    settle: Instant,
    /// Playing at the last poll (catches the end).
    was: bool,
    /// The poll runs while this lives.
    _live: Rc<()>,
}

impl Playback {
    pub fn play(&mut self, now: Instant) {
        self.painted = now;
        self.video.player.play();
    }

    pub fn toggle(&mut self, now: Instant) {
        if self.video.player.playing() { self.video.player.pause() } else { self.play(now) }
    }

    /// Seek to `secs`, waiting for its frame.
    pub fn seek(&mut self, secs: f64, now: Instant) {
        self.settle = now + SETTLE;
        self.video.player.seek(secs);
    }
}

/// A frame as gpui draws it: the pixel buffer's IOSurface, sampled by the renderer, fit inside.
#[cfg(target_os = "macos")]
pub(super) fn surface(frame: av::Frame) -> gpui::AnyElement {
    use gpui::{IntoElement as _, Styled as _};
    gpui::surface(frame).absolute().size_full().object_fit(gpui::ObjectFit::Contain).into_any_element()
}

#[cfg(not(target_os = "macos"))]
pub(super) fn surface(frame: av::Frame) -> gpui::AnyElement {
    match frame {}
}

/// Where a skip of `by` from `time` lands, inside the clip.
#[must_use]
pub fn skipped(time: f64, by: f64, duration: Option<f64>) -> f64 {
    let t = (time + by).max(0.0);
    duration.map_or(t, |d| t.min(d))
}

/// The fraction the knob and clock show: a scrub's, else the player's.
#[must_use]
pub fn played(drag: Option<Drag>, time: f64, duration: Option<f64>) -> f32 {
    match (drag, duration) {
        (Some(Drag::Scrub { at, .. }), _) => at,
        #[allow(clippy::cast_possible_truncation)]
        (_, Some(d)) if d > 0.0 && time.is_finite() => (time / d).clamp(0.0, 1.0) as f32,
        _ => 0.0,
    }
}

/// The left edge of a `width` wide bubble over the fraction `at` of a `track` wide scrubber, kept on it.
#[must_use]
pub fn bubble_left(at: f32, width: f32, track: f32) -> f32 {
    (track * at.clamp(0.0, 1.0) - width / 2.0).clamp(0.0, (track - width).max(0.0))
}

/// The speaker for a level (0–1).
#[must_use]
pub fn volume_glyph(volume: f32, muted: bool) -> &'static str {
    if muted || volume <= 0.0 { "volume-off" } else { "volume" }
}

impl AgentPane {
    /// The shown video's player, made once its file is ready (`id` attachment): the viewer's prefs applied,
    /// its poll started. It plays once the viewer finished opening.
    pub(super) fn ensure_playback(&mut self, id: &str, clip: &Clip, cx: &mut gpui::Context<Self>) {
        let Some(v) = self.viewer.as_mut().filter(|v| v.closing.is_none()) else { return };
        if v.video.as_ref().is_some_and(|p| p.id == id) {
            return;
        }
        let Some(video) = av::Video::new(&clip.file) else { return };
        video.player.set_volume(v.prefs.volume);
        video.player.set_muted(v.prefs.muted);
        let now = Instant::now();
        let live = Rc::new(());
        let poll = Rc::downgrade(&live);
        v.video = Some(Playback {
            id: id.to_owned(),
            video,
            hide: AutoHide::new(now),
            hover: None,
            drag: None,
            over_bar: false,
            pointer_in: false,
            track: Rc::default(),
            level: Rc::default(),
            started: false,
            painted: now,
            settle: now,
            was: false,
            _live: live,
        });
        cx.spawn(async move |this, cx| {
            let mut wait = POLL;
            while poll.strong_count() > 0 {
                cx.background_executor().timer(wait).await;
                let Ok(next) = this.update(cx, |this, cx| this.poll_video(cx)) else { break };
                wait = next;
            }
        })
        .detach();
    }

    /// One poll: a newer frame or a change redraws; the end stops (or loops); not drawn lately pauses; idle
    /// hides the chrome. The time to the next.
    fn poll_video(&mut self, cx: &mut gpui::Context<Self>) -> Duration {
        let reduce = cx.reduce_motion();
        let Some(v) = self.viewer.as_mut() else { return POLL_IDLE };
        let (looping, closing) = (v.prefs.looping, v.closing.is_some());
        let Some(p) = v.video.as_mut() else { return POLL_IDLE };
        let now = Instant::now();
        let player = &p.video.player;
        if player.playing() && now.duration_since(p.painted) > STALE {
            player.pause();
        }
        let playing = player.playing();
        // Playing: at least every `TICK` too (the clock; a clip that holds a frame still stays drawn).
        let mut redraw = playing != p.was || p.video.fresh() || (playing && now.duration_since(p.painted) > TICK);
        if p.was && !playing && looping && player.ended() && p.drag.is_none() {
            p.seek(0.0, now);
            p.play(now);
        }
        p.was = p.video.player.playing();
        let hold = !p.was || p.over_bar || p.drag.is_some() || closing;
        if p.hide.tick(now, hold, reduce) {
            if p.pointer_in {
                av::hide_cursor();
            }
            redraw = true;
        }
        let fast = p.was || p.drag.is_some() || now < p.settle;
        if redraw {
            cx.notify();
        }
        if fast { POLL } else { POLL_IDLE }
    }

    /// The shown video's player.
    pub(super) fn playback(&mut self) -> Option<&mut Playback> {
        self.viewer.as_mut()?.video.as_mut()
    }

    /// The pointer moved over the viewer, a click or a key: the chrome shows.
    pub(super) fn player_activity(&mut self, cx: &mut gpui::Context<Self>) {
        let reduce = cx.reduce_motion();
        let Some(p) = self.playback() else { return };
        let hidden = !p.hide.shown();
        p.hide.activity(Instant::now(), reduce);
        if hidden {
            cx.notify();
        }
    }

    pub(super) fn player_toggle(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(p) = self.playback() {
            p.toggle(Instant::now());
        }
        cx.notify();
    }

    pub(super) fn player_skip(&mut self, by: f64, cx: &mut gpui::Context<Self>) {
        if let Some(p) = self.playback() {
            let player = &p.video.player;
            let to = skipped(player.time(), by, player.duration());
            p.seek(to, Instant::now());
        }
        cx.notify();
    }

    /// Volume to `level` (0–1), unmuting; kept for the next video.
    pub(super) fn player_volume(&mut self, level: f32, cx: &mut gpui::Context<Self>) {
        let Some(v) = self.viewer.as_mut() else { return };
        v.prefs.volume = level.clamp(0.0, 1.0);
        v.prefs.muted = false;
        if let Some(p) = v.video.as_ref() {
            p.video.player.set_volume(v.prefs.volume);
            p.video.player.set_muted(false);
        }
        cx.notify();
    }

    pub(super) fn player_mute(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(v) = self.viewer.as_mut() else { return };
        v.prefs.muted = !v.prefs.muted;
        if let Some(p) = v.video.as_ref() {
            p.video.player.set_muted(v.prefs.muted);
        }
        cx.notify();
    }

    pub(super) fn player_loop(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(v) = self.viewer.as_mut() {
            v.prefs.looping = !v.prefs.looping;
        }
        cx.notify();
    }

    pub(super) fn player_fill(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(v) = self.viewer.as_mut() {
            v.prefs.fill = !v.prefs.fill;
        }
        cx.notify();
    }

    /// A player key (`keys::map`), the chrome shown.
    pub(super) fn player_key(&mut self, key: Key, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        self.player_activity(cx);
        match key {
            Key::Toggle => self.player_toggle(cx),
            Key::Skip(by) => self.player_skip(by, cx),
            Key::Volume(by) => {
                let level = self.viewer.as_ref().map_or(1.0, |v| if v.prefs.muted { 0.0 } else { v.prefs.volume });
                self.player_volume(level + by, cx);
            }
            Key::Mute => self.player_mute(cx),
            Key::Loop => self.player_loop(cx),
            Key::Fill => self.player_fill(cx),
            Key::Step(dir) => self.step_viewer(dir, cx),
            Key::Escape if self.viewer.as_ref().is_some_and(|v| v.prefs.fill) => self.player_fill(cx),
            Key::Escape => self.close_viewer(window, cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_stay_inside_the_clip() {
        assert!((skipped(3.0, 5.0, Some(5.0)) - 5.0).abs() < 1e-9);
        assert!(skipped(3.0, -5.0, Some(5.0)).abs() < 1e-9);
        assert!((skipped(1.0, 5.0, None) - 6.0).abs() < 1e-9, "no length yet: only the start bounds it");
    }

    #[test]
    fn the_knob_follows_a_scrub_else_the_player() {
        assert!((played(None, 2.5, Some(10.0)) - 0.25).abs() < 1e-6);
        assert!((played(Some(Drag::Scrub { at: 0.8, resume: true }), 2.5, Some(10.0)) - 0.8).abs() < 1e-6);
        assert!((played(Some(Drag::Volume), 2.5, Some(10.0)) - 0.25).abs() < 1e-6);
        assert!((played(None, 20.0, Some(10.0)) - 1.0).abs() < 1e-6, "clamped");
        assert!(played(None, 2.0, None).abs() < 1e-6 && played(None, f64::NAN, Some(1.0)).abs() < 1e-6);
    }

    #[test]
    fn the_time_bubble_stays_on_the_track() {
        assert!((bubble_left(0.5, 52.0, 400.0) - 174.0).abs() < 1e-4);
        assert!(bubble_left(0.0, 52.0, 400.0).abs() < 1e-4);
        assert!((bubble_left(1.0, 52.0, 400.0) - 348.0).abs() < 1e-4);
        assert!(bubble_left(0.5, 52.0, 30.0).abs() < 1e-4, "a track narrower than the bubble");
    }

    #[test]
    fn the_bar_reads_its_time_and_maps_presses() {
        // The bar's clock and its press math are the audio row's.
        assert_eq!(super::super::audio::times(3.2, Some(5.0)), "0:03 / 0:05");
        assert!((super::super::audio::fraction(250.0, 100.0, 600.0) - 0.25).abs() < 1e-9);
        assert_eq!((volume_glyph(0.6, false), volume_glyph(0.6, true), volume_glyph(0.0, false)), ("volume", "volume-off", "volume-off"));
    }
}
