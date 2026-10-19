//! Engine-agnostic input types and their CEF translation.

use cef::{MouseButtonType, MouseEvent};

/// Modifier state. `command` is the macOS ⌘ key (gpui's `platform`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub command: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

use cef::sys::cef_event_flags_t as F;

pub(crate) const IS_REPEAT: u32 = F::EVENTFLAG_IS_REPEAT.0;

impl Modifiers {
    pub(crate) fn flags(self) -> u32 {
        let mut flags = 0;
        for (on, flag) in [
            (self.shift, F::EVENTFLAG_SHIFT_DOWN),
            (self.control, F::EVENTFLAG_CONTROL_DOWN),
            (self.alt, F::EVENTFLAG_ALT_DOWN),
            (self.command, F::EVENTFLAG_COMMAND_DOWN),
        ] {
            if on {
                flags |= flag.0;
            }
        }
        flags
    }
}

impl MouseButton {
    pub(crate) fn cef(self) -> MouseButtonType {
        match self {
            MouseButton::Left => MouseButtonType::LEFT,
            MouseButton::Middle => MouseButtonType::MIDDLE,
            MouseButton::Right => MouseButtonType::RIGHT,
        }
    }

    fn flag(self) -> u32 {
        match self {
            MouseButton::Left => F::EVENTFLAG_LEFT_MOUSE_BUTTON.0,
            MouseButton::Middle => F::EVENTFLAG_MIDDLE_MOUSE_BUTTON.0,
            MouseButton::Right => F::EVENTFLAG_RIGHT_MOUSE_BUTTON.0,
        }
    }
}

/// A mouse event at `(x, y)` in view points (DIP), with the buttons currently held.
pub(crate) fn mouse_event(x: f32, y: f32, mods: Modifiers, held: &[MouseButton]) -> MouseEvent {
    let buttons = held.iter().fold(0, |acc, b| acc | b.flag());
    MouseEvent {
        x: x.round() as i32,
        y: y.round() as i32,
        modifiers: mods.flags() | buttons,
    }
}
