//! The Fn (🌐) key as voice typing's trigger, a pure state machine:
//!
//! - held alone for [`HOLD`]: records while held, released stops (insert);
//! - pressed twice within [`DOUBLE`]: records hands-free until the next
//!   press, which stops;
//! - Esc cancels either;
//! - Fn with another key or modifier (Fn+arrow, Fn+F-keys) never records,
//!   and a single short tap does nothing.
//!
//! The owner feeds it [`Input`]s with the time since any fixed instant, and
//! [`Input::Tick`]s while [`Trigger::wants_ticks`].

use std::time::Duration;

/// Fn held alone this long starts recording.
pub const HOLD: Duration = Duration::from_millis(200);
/// A second press this soon after the first starts hands-free recording.
pub const DOUBLE: Duration = Duration::from_millis(350);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// The modifier keys changed: Fn down, and any other modifier down.
    Modifiers { function: bool, others: bool },
    /// A key (not a modifier) went down.
    Key { escape: bool },
    /// Time passed.
    Tick,
    /// The window lost focus: no release will arrive.
    Blur,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    StartHold,
    StartHandsFree,
    /// Stop recording and insert.
    Stop,
    /// Stop recording and drop it.
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// Fn down alone since `at`.
    Pressed {
        at: Duration,
    },
    /// Fn tapped, first down at `at`: a second press may follow.
    Tapped {
        at: Duration,
    },
    /// Recording while Fn is held.
    Holding,
    /// Recording hands-free; `released`: the press that started it is up.
    HandsFree {
        released: bool,
    },
    /// Fn is down but used up (a combo, a cancel, a stop): nothing until it's up.
    Spent,
}

#[derive(Debug)]
pub struct Trigger {
    state: State,
    function: bool,
}

impl Default for Trigger {
    fn default() -> Self {
        Self { state: State::Idle, function: false }
    }
}

impl Trigger {
    /// Recording, hands-free or not.
    #[must_use]
    pub fn recording(&self) -> bool {
        matches!(self.state, State::Holding | State::HandsFree { .. })
    }

    #[must_use]
    pub fn hands_free(&self) -> bool {
        matches!(self.state, State::HandsFree { .. })
    }

    /// Waiting on time: a hold or a second press.
    #[must_use]
    pub fn wants_ticks(&self) -> bool {
        matches!(self.state, State::Pressed { .. } | State::Tapped { .. })
    }

    /// Recording ended elsewhere (a failure, the pill's own stop).
    pub fn reset(&mut self) {
        self.state = if self.function { State::Spent } else { State::Idle };
    }

    /// The machine's answer to `input` at `now`.
    pub fn input(&mut self, input: Input, now: Duration) -> Option<Action> {
        let (next, action) = self.next(input, now);
        self.state = next;
        action
    }

    fn next(&mut self, input: Input, now: Duration) -> (State, Option<Action>) {
        use State as S;
        let state = self.state;
        let spent = |fn_down: bool| if fn_down { S::Spent } else { S::Idle };
        match input {
            Input::Modifiers { function, others } => {
                let pressed = function && !self.function;
                let released = !function && self.function;
                self.function = function;
                match state {
                    // Fn with another modifier is a combo.
                    S::Pressed { .. } | S::Tapped { .. } if others => (spent(function), None),
                    S::Holding if others => (spent(function), Some(Action::Cancel)),
                    S::Idle if pressed => (S::Pressed { at: now }, None),
                    S::Tapped { at } if pressed && now.saturating_sub(at) <= DOUBLE => {
                        (S::HandsFree { released: false }, Some(Action::StartHandsFree))
                    }
                    S::Tapped { .. } if pressed => (S::Pressed { at: now }, None),
                    S::Pressed { at } if released && now.saturating_sub(at) < HOLD => (S::Tapped { at }, None),
                    S::Pressed { .. } if released => (S::Idle, None),
                    S::Holding if released => (S::Idle, Some(Action::Stop)),
                    S::HandsFree { released: false } if released => (S::HandsFree { released: true }, None),
                    S::HandsFree { released: true } if pressed => (S::Spent, Some(Action::Stop)),
                    S::Spent if released => (S::Idle, None),
                    _ => (state, None),
                }
            }
            Input::Key { escape } => match state {
                S::Holding | S::HandsFree { .. } if escape => (spent(self.function), Some(Action::Cancel)),
                // Fn+key after the hold began: a combo, not dictation.
                S::Holding => (S::Spent, Some(Action::Cancel)),
                S::Pressed { .. } => (S::Spent, None),
                S::Tapped { .. } => (S::Idle, None),
                _ => (state, None),
            },
            Input::Tick => match state {
                S::Pressed { at } if now.saturating_sub(at) >= HOLD => (S::Holding, Some(Action::StartHold)),
                S::Tapped { at } if now.saturating_sub(at) > DOUBLE => (S::Idle, None),
                _ => (state, None),
            },
            Input::Blur => {
                self.function = false;
                match state {
                    S::Holding => (S::Idle, Some(Action::Cancel)),
                    S::HandsFree { .. } => (S::HandsFree { released: true }, None),
                    _ => (S::Idle, None),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FN: Input = Input::Modifiers { function: true, others: false };
    const UP: Input = Input::Modifiers { function: false, others: false };
    const ESC: Input = Input::Key { escape: true };
    const KEY: Input = Input::Key { escape: false };

    /// Feeds `(input, ms)` pairs; every action with its time.
    fn run(steps: &[(Input, u64)]) -> Vec<(Action, u64)> {
        let mut t = Trigger::default();
        steps.iter().filter_map(|&(input, ms)| t.input(input, Duration::from_millis(ms)).map(|a| (a, ms))).collect()
    }

    #[test]
    fn holding_records_until_release() {
        assert_eq!(
            run(&[(FN, 0), (Input::Tick, 100), (Input::Tick, 210), (Input::Tick, 900), (UP, 1500)]),
            [(Action::StartHold, 210), (Action::Stop, 1500)]
        );
    }

    #[test]
    fn a_double_tap_records_hands_free_until_the_next_press() {
        let steps = [(FN, 0), (UP, 80), (Input::Tick, 150), (FN, 250), (UP, 330), (Input::Tick, 2000), (KEY, 2500), (FN, 4000), (UP, 4100)];
        assert_eq!(run(&steps), [(Action::StartHandsFree, 250), (Action::Stop, 4000)]);
        // The stopping press's release starts nothing new.
        let mut t = Trigger::default();
        for (input, ms) in steps {
            t.input(input, Duration::from_millis(ms));
        }
        assert!(!t.recording() && !t.wants_ticks());
    }

    #[test]
    fn hands_free_holds_through_a_long_second_press() {
        let mut t = Trigger::default();
        let ms = Duration::from_millis;
        t.input(FN, ms(0));
        t.input(UP, ms(60));
        assert_eq!(t.input(FN, ms(200)), Some(Action::StartHandsFree));
        assert_eq!(t.input(Input::Tick, ms(900)), None);
        assert!(t.hands_free());
        assert_eq!(t.input(UP, ms(1000)), None, "releasing the second press keeps recording");
        assert_eq!(t.input(FN, ms(3000)), Some(Action::Stop));
    }

    #[test]
    fn a_single_tap_does_nothing() {
        assert_eq!(run(&[(FN, 0), (UP, 90), (Input::Tick, 200), (Input::Tick, 500)]), []);
        // A slow second press is a new first press.
        assert_eq!(run(&[(FN, 0), (UP, 90), (Input::Tick, 500), (FN, 600), (UP, 650)]), []);
        assert_eq!(run(&[(FN, 0), (UP, 90), (FN, 600), (Input::Tick, 850)]), [(Action::StartHold, 850)]);
    }

    #[test]
    fn fn_combos_never_record() {
        assert_eq!(run(&[(FN, 0), (KEY, 50), (Input::Tick, 400), (UP, 600)]), [], "Fn+arrow");
        let with_shift = Input::Modifiers { function: true, others: true };
        assert_eq!(run(&[(FN, 0), (with_shift, 40), (Input::Tick, 400), (UP, 500)]), [], "Fn+Shift");
        assert_eq!(run(&[(FN, 0), (KEY, 50), (UP, 60), (FN, 150), (UP, 200)]), [], "a combo is no first tap");
        // A key after the hold began cancels it: it was a slow combo.
        assert_eq!(run(&[(FN, 0), (Input::Tick, 300), (KEY, 700), (UP, 800)]), [(Action::StartHold, 300), (Action::Cancel, 700)]);
        // Typing during hands-free recording leaves it running.
        assert_eq!(run(&[(FN, 0), (UP, 50), (FN, 100), (UP, 150), (KEY, 400)]), [(Action::StartHandsFree, 100)]);
    }

    #[test]
    fn escape_cancels_either_mode() {
        assert_eq!(
            run(&[(FN, 0), (Input::Tick, 250), (ESC, 900), (UP, 1000), (Input::Tick, 1300)]),
            [(Action::StartHold, 250), (Action::Cancel, 900)]
        );
        assert_eq!(run(&[(FN, 0), (UP, 50), (FN, 100), (UP, 150), (ESC, 900), (FN, 1500)]), [(Action::StartHandsFree, 100), (Action::Cancel, 900)]);
        assert_eq!(run(&[(ESC, 0)]), [], "nothing to cancel");
    }

    #[test]
    fn losing_focus_cancels_a_hold_and_keeps_hands_free() {
        assert_eq!(run(&[(FN, 0), (Input::Tick, 250), (Input::Blur, 400), (UP, 500)]), [(Action::StartHold, 250), (Action::Cancel, 400)]);
        assert_eq!(run(&[(FN, 0), (UP, 50), (FN, 100), (Input::Blur, 120), (FN, 900)]), [(Action::StartHandsFree, 100), (Action::Stop, 900)]);
    }

    #[test]
    fn a_reset_waits_for_fn_to_come_up() {
        let mut t = Trigger::default();
        let ms = Duration::from_millis;
        t.input(FN, ms(0));
        assert_eq!(t.input(Input::Tick, ms(300)), Some(Action::StartHold));
        t.reset();
        assert!(!t.recording());
        assert_eq!(t.input(UP, ms(400)), None, "the held press is spent");
        assert_eq!(t.input(FN, ms(500)), None);
        assert!(t.wants_ticks());
    }
}
