//! Client-local cursor blink. The terminal decides *whether* the cursor
//! blinks (DECSCUSR, DECTCEM, the terminal settings' blink default); the
//! pane only animates the phase, and only while that matters, so an idle
//! window with a steady, hidden or unfocused cursor schedules nothing.

use std::time::{Duration, Instant};

/// Half a blink period: shown for one, hidden for the next.
pub const PHASE: Duration = Duration::from_millis(500);

/// Whether the blink animation runs. A pane that is unfocused (or in an
/// inactive window) draws a steady hollow cursor; a hidden cursor or one
/// scrolled out of view has nothing to animate.
#[must_use]
#[allow(clippy::fn_params_excessive_bools, reason = "independent inputs of the policy")]
pub fn active(focused: bool, window_active: bool, visible: bool, blinking: bool, at_bottom: bool) -> bool {
    focused && window_active && visible && blinking && at_bottom
}

#[derive(Debug)]
pub struct Blink {
    epoch: Instant,
    /// Cursor position the phase was last reset for.
    at: Option<(u16, u16)>,
}

impl Default for Blink {
    fn default() -> Self {
        Self { epoch: Instant::now(), at: None }
    }
}

impl Blink {
    /// Restart in the visible phase (typing, focus, cursor movement).
    pub fn reset(&mut self) {
        self.epoch = Instant::now();
    }

    /// Reset when the cursor moved, so it is visible right where output or
    /// typing put it.
    pub fn track(&mut self, column: u16, row: u16) {
        if self.at != Some((column, row)) {
            self.at = Some((column, row));
            self.reset();
        }
    }

    #[must_use]
    pub fn visible_at(&self, now: Instant) -> bool {
        phase_visible(now.saturating_duration_since(self.epoch))
    }

    /// Time until the phase next flips.
    #[must_use]
    pub fn until_next(&self, now: Instant) -> Duration {
        until_next(now.saturating_duration_since(self.epoch))
    }
}

#[must_use]
pub fn phase_visible(elapsed: Duration) -> bool {
    (elapsed.as_millis() / PHASE.as_millis()).is_multiple_of(2)
}

#[must_use]
pub fn until_next(elapsed: Duration) -> Duration {
    let phase = PHASE.as_millis();
    let into = elapsed.as_millis() % phase;
    Duration::from_millis(u64::try_from(phase - into).unwrap_or(1).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_requires_focus_visibility_and_blinking() {
        assert!(active(true, true, true, true, true));
        assert!(!active(false, true, true, true, true), "unfocused pane: steady hollow");
        assert!(!active(true, false, true, true, true), "inactive window: steady");
        assert!(!active(true, true, false, true, true), "DECTCEM hidden");
        assert!(!active(true, true, true, false, true), "DECSCUSR steady");
        assert!(!active(true, true, true, true, false), "scrolled into history");
    }

    #[test]
    fn phases_alternate_every_half_period() {
        assert!(phase_visible(Duration::ZERO));
        assert!(phase_visible(Duration::from_millis(499)));
        assert!(!phase_visible(Duration::from_millis(500)));
        assert!(!phase_visible(Duration::from_millis(999)));
        assert!(phase_visible(Duration::from_millis(1000)));
        assert_eq!(until_next(Duration::from_millis(0)), Duration::from_millis(500));
        assert_eq!(until_next(Duration::from_millis(120)), Duration::from_millis(380));
        assert_eq!(until_next(Duration::from_millis(1499)), Duration::from_millis(1));
    }

    #[test]
    fn moving_the_cursor_restarts_visible() {
        let mut blink = Blink::default();
        blink.track(3, 4);
        let first = blink.epoch;
        std::thread::sleep(Duration::from_millis(2));
        blink.track(3, 4);
        assert_eq!(blink.epoch, first, "same position: phase keeps running");
        blink.track(4, 4);
        assert!(blink.epoch > first);
        assert!(blink.visible_at(Instant::now()));
    }
}
