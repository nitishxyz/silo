//! When the player's chrome (and the pointer) hides: after [`IDLE`] without pointer movement while playing.

use std::time::{Duration, Instant};

use crate::motion::Toggle;

pub const IDLE: Duration = Duration::from_millis(2500);
/// The chrome's fade either way (`motion::UI`).
pub const FADE: Duration = Duration::from_millis(260);

#[derive(Clone, Copy, Debug)]
pub struct AutoHide {
    last: Instant,
    shown: Toggle,
}

impl AutoHide {
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self { last: now, shown: Toggle::new(true, FADE) }
    }

    /// The pointer moved, a key or a click: shown, the idle clock restarted.
    pub fn activity(&mut self, now: Instant, reduce: bool) {
        self.last = now;
        self.shown.set(true, now, reduce);
    }

    /// `hold` keeps it up (paused, over the bar, dragging, closing). True exactly when it just hid.
    pub fn tick(&mut self, now: Instant, hold: bool, reduce: bool) -> bool {
        if hold {
            self.activity(now, reduce);
            return false;
        }
        let hide = self.shown.on() && now.saturating_duration_since(self.last) >= IDLE;
        if hide {
            self.shown.set(false, now, reduce);
        }
        hide
    }

    /// Shown, or on its way in.
    #[must_use]
    pub fn shown(&self) -> bool {
        self.shown.on()
    }

    /// The fade (`view::toggled` draws it).
    #[must_use]
    pub fn fade(&self) -> &Toggle {
        &self.shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_after_idle_and_returns_on_activity() {
        let start = Instant::now();
        let mut hide = AutoHide::new(start);
        assert!(hide.shown());
        assert!(!hide.tick(start + Duration::from_millis(2400), false, true));
        assert!(hide.tick(start + IDLE, false, true), "hid just now");
        assert!(!hide.shown());
        assert!(!hide.tick(start + IDLE * 2, false, true), "hid once");
        hide.activity(start + IDLE * 3, true);
        assert!(hide.shown());
        assert!((hide.fade().value(start + IDLE * 3, true) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn hold_keeps_it_up_and_restarts_the_clock() {
        let start = Instant::now();
        let mut hide = AutoHide::new(start);
        assert!(!hide.tick(start + Duration::from_secs(10), true, true));
        assert!(hide.shown());
        assert!(!hide.tick(start + Duration::from_secs(12), false, true), "2 s since the hold ended");
        assert!(hide.tick(start + Duration::from_millis(12_500), false, true));
    }

    #[test]
    fn a_hold_brings_hidden_chrome_back() {
        let start = Instant::now();
        let mut hide = AutoHide::new(start);
        hide.tick(start + IDLE, false, true);
        assert!(!hide.shown());
        hide.tick(start + IDLE * 2, true, true);
        assert!(hide.shown(), "paused: shown again");
    }
}
