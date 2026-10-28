//! Trackpad gestures on browser panes: a pinch as two touch points (so
//! Chromium zooms the visual viewport, as from a touch screen: CEF takes no
//! trackpad pinch, and CDP's synthesized pinch neither zooms a windowless
//! page nor keeps up, at ~100 ms a step), smart zoom, and two-finger
//! horizontal swipes that navigate back and forward.

use gpui::TouchPhase;

/// Half the distance between the two touch points when a pinch starts.
/// Chromium only scales while their span stays above about 125 points.
const PINCH_RADIUS: f32 = 200.0;
/// Touches start this far inside the pinch radius and move out to it at
/// once: Chromium's touch slop swallows that first move, so the pinch
/// measures from the full radius and lands on the scale asked for.
const PRIME: f32 = 16.0;
/// The spread range: the span stays scalable, the numbers stay sane.
const MIN_SPREAD: f32 = 0.35;
const MAX_SPREAD: f32 = 6.0;
/// Chromium's page-scale limits in a windowless browser (measured).
pub const MIN_SCALE: f32 = 1.0;
pub const MAX_SCALE: f32 = 3.0;
/// Smart zoom's magnified scale.
const SMART_SCALE: f32 = 2.0;

/// A pinch in progress: two touch points spreading from a fixed center.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PinchTouch {
    center: (f32, f32),
    spread: f32,
}

impl PinchTouch {
    pub const fn new(center: (f32, f32)) -> Self {
        Self { center, spread: 1.0 }
    }

    /// A trackpad magnification step (0.1 = 10% bigger); returns the
    /// factor actually applied.
    pub fn magnify(&mut self, delta: f32) -> f32 {
        let before = self.spread;
        self.spread = (self.spread * (1.0 + delta)).clamp(MIN_SPREAD, MAX_SPREAD);
        self.spread / before
    }

    /// Spread to `spread` of the starting span; returns the factor applied.
    pub fn spread_to(&mut self, spread: f32) -> f32 {
        let before = self.spread;
        self.spread = spread.clamp(MIN_SPREAD, MAX_SPREAD);
        self.spread / before
    }

    pub fn points(&self) -> [(f32, f32); 2] {
        let (x, y) = self.center;
        let r = PINCH_RADIUS * self.spread;
        [(x - r, y), (x + r, y)]
    }

    /// Where the touches go down: [`Self::points`] drawn in by [`PRIME`].
    pub fn start_points(&self) -> [(f32, f32); 2] {
        let (x, y) = self.center;
        let r = PINCH_RADIUS * self.spread - PRIME;
        [(x - r, y), (x + r, y)]
    }

    /// Both fingers move by `(dx, dy)`: the page pans with them.
    pub fn pan(&mut self, (dx, dy): (f32, f32)) {
        self.center = (self.center.0 + dx, self.center.1 + dy);
    }
}

/// A touch event for both points of a pinch.
pub type TouchStep = (silo_web::Pinch, [(f32, f32); 2]);

/// A pinch's touch sequence, always ended by a cancel: a release with the
/// fingers still moving flings, and a fling started over a cross-site frame
/// left Chromium's touch scroll running, which swallowed every wheel scroll
/// after it. Cancelled touches also never tap or click.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PinchGesture {
    touch: Option<PinchTouch>,
}

impl PinchGesture {
    pub fn active(&self) -> bool {
        self.touch.is_some()
    }

    /// Touches go down around `center` (any earlier ones are cancelled).
    pub fn begin(&mut self, center: (f32, f32)) -> Vec<TouchStep> {
        let mut steps: Vec<TouchStep> = self.end().into_iter().collect();
        let touch = PinchTouch::new(center);
        steps.extend([(silo_web::Pinch::Start, touch.start_points()), (silo_web::Pinch::Move, touch.points())]);
        self.touch = Some(touch);
        steps
    }

    /// A magnification step at `center`, starting a fresh sequence there when
    /// none is down (its start was missed, or it was lifted while the
    /// fingers rested); also the scale factor applied.
    pub fn magnify(&mut self, center: (f32, f32), delta: f32) -> (Vec<TouchStep>, f32) {
        let mut steps = if self.touch.is_none() { self.begin(center) } else { Vec::new() };
        let Some(touch) = &mut self.touch else { return (steps, 1.0) };
        let factor = touch.magnify(delta);
        steps.push((silo_web::Pinch::Move, touch.points()));
        (steps, factor)
    }

    /// Spread to `spread` of the starting span (smart zoom's steps).
    pub fn spread_to(&mut self, spread: f32) -> Option<(TouchStep, f32)> {
        let touch = self.touch.as_mut()?;
        let factor = touch.spread_to(spread);
        Some(((silo_web::Pinch::Move, touch.points()), factor))
    }

    /// Two-finger scrolling while pinching pans the page, as the fingers move.
    pub fn pan(&mut self, delta: (f32, f32)) -> Option<TouchStep> {
        let touch = self.touch.as_mut()?;
        touch.pan(delta);
        Some((silo_web::Pinch::Move, touch.points()))
    }

    /// Lifts the touches, if down.
    pub fn end(&mut self) -> Option<TouchStep> {
        self.touch.take().map(|touch| (silo_web::Pinch::Cancel, touch.points()))
    }
}

/// The page scale as this pane last zoomed it.
pub fn scaled(scale: f32, factor: f32) -> f32 {
    (scale * factor).clamp(MIN_SCALE, MAX_SCALE)
}

/// Smart zoom from `scale`: the spread steps that magnify to 2× or, when
/// zoomed, back to 1×: the narrowest spread, which Chromium clamps at 1×
/// (the scale tracked here is only an estimate of the page's).
pub fn smart_zoom_steps(scale: f32, steps: usize) -> Vec<f32> {
    let target = if scale > MIN_SCALE * 1.05 { MIN_SPREAD } else { SMART_SCALE };
    let steps = steps.max(1);
    (1..=steps).map(|i| target.powf(i as f32 / steps as f32)).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Back,
    Forward,
}

/// What a scroll event does under swipe navigation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Swipe {
    /// Not a navigation swipe: the page scrolls.
    Pass,
    /// A navigation swipe, hidden from the page; `progress` 0…1 to commit.
    Track { direction: Direction, progress: f32 },
    /// Released past the threshold: navigate.
    Commit(Direction),
    /// Released short, cancelled, or the page scrolled after all.
    Abort,
    /// Momentum after a navigation swipe: dropped.
    Drop,
}

/// Movement before the axis locks.
const LOCK: f32 = 8.0;
/// Horizontal movement before a swipe takes over from the page.
const ARM: f32 = 32.0;
/// Further movement to commit.
const DISTANCE: f32 = 140.0;
/// The page's horizontal offset counts as moved past this.
const OFFSET_SLOP: f64 = 1.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum State {
    /// No trackpad gesture (wheel mice, phase-less input).
    #[default]
    Idle,
    /// Fingers down, axis undecided.
    Pending {
        dx: f32,
        dy: f32,
        offset_x: f64,
    },
    /// Horizontal, not yet past `ARM`.
    Horizontal {
        dx: f32,
        offset_x: f64,
    },
    /// The page has this gesture.
    Page,
    Armed {
        direction: Direction,
        dx: f32,
        offset_x: f64,
    },
    /// Fingers up: momentum, dropped after a navigation swipe.
    Coast {
        drop: bool,
    },
}

/// Tells a page's scroll from a navigation swipe: a horizontal two-finger
/// scroll that doesn't move the page, with history in its direction.
#[derive(Clone, Copy, Debug, Default)]
pub struct SwipeTracker {
    state: State,
}

impl SwipeTracker {
    /// `delta` in points (positive x: fingers move right, i.e. back);
    /// `offset_x` the page's horizontal scroll offset; `can` whether
    /// history steps that way.
    pub fn scroll(&mut self, phase: TouchPhase, delta: (f32, f32), offset_x: f64, can: impl Fn(Direction) -> bool) -> Swipe {
        let armed = matches!(self.state, State::Armed { .. });
        match phase {
            TouchPhase::Started => {
                self.state = State::Pending { dx: 0.0, dy: 0.0, offset_x };
                self.moved(delta, offset_x, can)
            }
            TouchPhase::Cancelled => {
                self.state = State::Idle;
                if armed { Swipe::Abort } else { Swipe::Pass }
            }
            TouchPhase::Ended => {
                let result = match self.moved(delta, offset_x, can) {
                    Swipe::Track { direction, progress } if progress >= 1.0 => Swipe::Commit(direction),
                    Swipe::Track { .. } | Swipe::Abort => Swipe::Abort,
                    _ => Swipe::Pass,
                };
                self.state = State::Coast { drop: matches!(self.state, State::Armed { .. }) };
                result
            }
            TouchPhase::Moved => match self.state {
                State::Idle => Swipe::Pass,
                State::Coast { drop } => {
                    if drop {
                        Swipe::Drop
                    } else {
                        Swipe::Pass
                    }
                }
                _ => self.moved(delta, offset_x, can),
            },
        }
    }

    fn moved(&mut self, (x, y): (f32, f32), offset: f64, can: impl Fn(Direction) -> bool) -> Swipe {
        match self.state {
            State::Pending { dx, dy, offset_x } => {
                let (dx, dy) = (dx + x, dy + y);
                if dx.abs() + dy.abs() < LOCK {
                    self.state = State::Pending { dx, dy, offset_x };
                } else if dx.abs() > 2.0 * dy.abs() {
                    self.state = State::Horizontal { dx, offset_x };
                    return self.moved((0.0, 0.0), offset, can);
                } else {
                    self.state = State::Page;
                }
                Swipe::Pass
            }
            State::Horizontal { dx, offset_x } => {
                let dx = dx + x;
                if (offset - offset_x).abs() > OFFSET_SLOP {
                    self.state = State::Page;
                } else if dx.abs() >= ARM {
                    let direction = if dx > 0.0 { Direction::Back } else { Direction::Forward };
                    self.state = if can(direction) { State::Armed { direction, dx, offset_x } } else { State::Page };
                    if let State::Armed { .. } = self.state {
                        return self.track();
                    }
                } else {
                    self.state = State::Horizontal { dx, offset_x };
                }
                Swipe::Pass
            }
            State::Armed { direction, dx, offset_x } => {
                // The page scrolled from input sent before the swipe armed.
                if (offset - offset_x).abs() > OFFSET_SLOP {
                    self.state = State::Page;
                    return Swipe::Abort;
                }
                self.state = State::Armed { direction, dx: dx + x, offset_x };
                self.track()
            }
            State::Idle | State::Page | State::Coast { .. } => Swipe::Pass,
        }
    }

    fn track(&self) -> Swipe {
        let State::Armed { direction, dx, .. } = self.state else { return Swipe::Pass };
        let along = if direction == Direction::Back { dx } else { -dx };
        Swipe::Track { direction, progress: ((along - ARM) / DISTANCE).clamp(0.0, 1.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(tracker: &mut SwipeTracker, events: &[(TouchPhase, f32, f32)], offset: f64, can: bool) -> Vec<Swipe> {
        events.iter().map(|&(phase, x, y)| tracker.scroll(phase, (x, y), offset, |_| can)).collect()
    }

    fn swipe(dx: f32, steps: usize) -> Vec<(TouchPhase, f32, f32)> {
        let mut events = vec![(TouchPhase::Started, 0.0, 0.0)];
        events.extend((0..steps).map(|_| (TouchPhase::Moved, dx, 0.0)));
        events.push((TouchPhase::Ended, 0.0, 0.0));
        events
    }

    #[test]
    fn pinch_points_spread_around_the_center() {
        let mut pinch = PinchTouch::new((300.0, 200.0));
        assert_eq!(pinch.points(), [(100.0, 200.0), (500.0, 200.0)]);
        assert_eq!(pinch.start_points(), [(116.0, 200.0), (484.0, 200.0)], "touches go down inside the radius");
        let factor = pinch.magnify(0.5);
        assert!((factor - 1.5).abs() < 1e-6);
        assert_eq!(pinch.points(), [(0.0, 200.0), (600.0, 200.0)]);
    }

    #[test]
    fn pinch_spread_is_clamped() {
        let mut pinch = PinchTouch::new((0.0, 0.0));
        for _ in 0..50 {
            pinch.magnify(-0.5);
        }
        let [(a, _), (b, _)] = pinch.points();
        assert!((b - a) >= 2.0 * PINCH_RADIUS * MIN_SPREAD - 1e-3);
        assert!(pinch.magnify(-0.5) >= 1.0 - 1e-6, "no further shrink at the floor");
    }

    #[test]
    fn a_pinch_always_ends_cancelled() {
        use silo_web::Pinch::{Cancel, Move, Start};
        let mut pinch = PinchGesture::default();
        let kinds = |steps: &[TouchStep]| steps.iter().map(|s| s.0).collect::<Vec<_>>();
        assert_eq!(kinds(&pinch.begin((300.0, 200.0))), [Start, Move]);
        assert_eq!(kinds(&pinch.begin((300.0, 200.0))), [Cancel, Start, Move], "a new start lifts the old touches first");
        let (steps, factor) = pinch.magnify((0.0, 0.0), 0.5);
        assert_eq!(kinds(&steps), [Move]);
        assert!((factor - 1.5).abs() < 1e-6);
        assert_eq!(pinch.end().map(|s| s.0), Some(Cancel), "never a release: that flings");
        assert_eq!(pinch.end(), None);
        assert!(!pinch.active());
    }

    #[test]
    fn a_pinch_whose_start_was_missed_starts_at_its_next_step() {
        use silo_web::Pinch::{Move, Start};
        let mut pinch = PinchGesture::default();
        let (steps, factor) = pinch.magnify((300.0, 200.0), 0.1);
        assert_eq!(steps.iter().map(|s| s.0).collect::<Vec<_>>(), [Start, Move, Move]);
        assert_eq!(steps[1].1, [(100.0, 200.0), (500.0, 200.0)]);
        assert!((factor - 1.1).abs() < 1e-6);
        assert!(pinch.active());
        assert_eq!(PinchGesture::default().spread_to(2.0), None, "smart zoom steps need touches down");
    }

    #[test]
    fn scrolling_while_pinching_pans_both_fingers() {
        let mut pinch = PinchGesture::default();
        assert_eq!(pinch.pan((5.0, 5.0)), None);
        pinch.begin((300.0, 200.0));
        let (_, points) = pinch.pan((10.0, -20.0)).unwrap();
        assert_eq!(points, [(110.0, 180.0), (510.0, 180.0)]);
    }

    #[test]
    fn page_scale_stays_in_chromium_limits() {
        assert_eq!(scaled(1.0, 0.5), MIN_SCALE);
        assert_eq!(scaled(2.5, 2.0), MAX_SCALE);
        assert!((scaled(1.0, 1.5) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn smart_zoom_toggles() {
        let steps = smart_zoom_steps(1.0, 4);
        assert_eq!(steps.len(), 4);
        assert!((steps[3] - 2.0).abs() < 1e-5);
        assert!(steps.windows(2).all(|w| w[0] < w[1]));
        let back = smart_zoom_steps(2.0, 4);
        assert!((back[3] - MIN_SPREAD).abs() < 1e-5);
    }

    #[test]
    fn a_long_swipe_commits() {
        let mut t = SwipeTracker::default();
        let out = run(&mut t, &swipe(20.0, 12), 0.0, true);
        assert_eq!(out[1], Swipe::Pass, "the page sees the start");
        assert!(out.iter().any(|s| matches!(s, Swipe::Track { direction: Direction::Back, .. })));
        assert_eq!(*out.last().unwrap(), Swipe::Commit(Direction::Back));
        assert_eq!(t.scroll(TouchPhase::Moved, (30.0, 0.0), 0.0, |_| true), Swipe::Drop, "momentum dropped");
    }

    #[test]
    fn leftwards_goes_forward() {
        let mut t = SwipeTracker::default();
        let out = run(&mut t, &swipe(-20.0, 12), 0.0, true);
        assert_eq!(*out.last().unwrap(), Swipe::Commit(Direction::Forward));
    }

    #[test]
    fn a_short_swipe_aborts() {
        let mut t = SwipeTracker::default();
        let out = run(&mut t, &swipe(20.0, 4), 0.0, true);
        assert!(out.iter().any(|s| matches!(s, Swipe::Track { .. })));
        assert_eq!(*out.last().unwrap(), Swipe::Abort);
    }

    #[test]
    fn reversing_aborts() {
        let mut t = SwipeTracker::default();
        let mut events = swipe(20.0, 12);
        events.pop();
        events.extend((0..12).map(|_| (TouchPhase::Moved, -20.0, 0.0)));
        events.push((TouchPhase::Ended, 0.0, 0.0));
        assert_eq!(*run(&mut t, &events, 0.0, true).last().unwrap(), Swipe::Abort);
    }

    #[test]
    fn cancel_aborts() {
        let mut t = SwipeTracker::default();
        let mut events = swipe(20.0, 6);
        events.pop();
        events.push((TouchPhase::Cancelled, 0.0, 0.0));
        assert_eq!(*run(&mut t, &events, 0.0, true).last().unwrap(), Swipe::Abort);
    }

    #[test]
    fn vertical_scrolls_stay_with_the_page() {
        let mut t = SwipeTracker::default();
        let events = [(TouchPhase::Started, 0.0, 0.0), (TouchPhase::Moved, 6.0, 10.0), (TouchPhase::Moved, 60.0, 0.0), (TouchPhase::Ended, 0.0, 0.0)];
        assert!(run(&mut t, &events, 0.0, true).iter().all(|s| *s == Swipe::Pass));
        assert_eq!(t.scroll(TouchPhase::Moved, (30.0, 0.0), 0.0, |_| true), Swipe::Pass, "momentum scrolls the page");
    }

    #[test]
    fn no_history_no_swipe() {
        let mut t = SwipeTracker::default();
        assert!(run(&mut t, &swipe(20.0, 12), 0.0, false).iter().all(|s| *s == Swipe::Pass));
    }

    #[test]
    fn a_page_that_scrolls_keeps_the_gesture() {
        let mut t = SwipeTracker::default();
        t.scroll(TouchPhase::Started, (0.0, 0.0), 100.0, |_| true);
        t.scroll(TouchPhase::Moved, (10.0, 0.0), 100.0, |_| true);
        assert_eq!(t.scroll(TouchPhase::Moved, (30.0, 0.0), 90.0, |_| true), Swipe::Pass);
        assert_eq!(t.scroll(TouchPhase::Moved, (30.0, 0.0), 80.0, |_| true), Swipe::Pass);
    }

    #[test]
    fn a_late_page_scroll_aborts_an_armed_swipe() {
        let mut t = SwipeTracker::default();
        t.scroll(TouchPhase::Started, (0.0, 0.0), 0.0, |_| true);
        assert!(matches!(t.scroll(TouchPhase::Moved, (40.0, 0.0), 0.0, |_| true), Swipe::Track { .. }));
        assert_eq!(t.scroll(TouchPhase::Moved, (10.0, 0.0), 12.0, |_| true), Swipe::Abort);
        assert_eq!(t.scroll(TouchPhase::Moved, (10.0, 0.0), 12.0, |_| true), Swipe::Pass);
    }

    #[test]
    fn wheel_mice_pass() {
        let mut t = SwipeTracker::default();
        assert_eq!(t.scroll(TouchPhase::Moved, (80.0, 0.0), 0.0, |_| true), Swipe::Pass);
    }
}
