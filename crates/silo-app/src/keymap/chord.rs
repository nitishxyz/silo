//! One shortcut: [`silo_proto::keys::Chord`], the parser `init.lua` and
//! `keybindings.json` share, plus what only the app knows: a gpui key
//! press, and why a chord may be a poor choice on a Mac.

pub use silo_proto::keys::Chord;

/// Chords macOS keeps for itself: input source, Spotlight, app switching,
/// Look Up, Dock, emoji, screenshots, Force Quit, Lock Screen, Spaces.
const RESERVED: &[&str] = &[
    "cmd-space",
    "ctrl-space",
    "alt-cmd-space",
    "cmd-tab",
    "shift-cmd-tab",
    "ctrl-cmd-d",
    "alt-cmd-d",
    "ctrl-cmd-space",
    "cmd-#",
    "cmd-$",
    "cmd-%",
    "alt-cmd-escape",
    "ctrl-cmd-q",
    "ctrl-up",
    "ctrl-down",
    "ctrl-left",
    "ctrl-right",
];

/// The app's side of a [`Chord`].
pub trait ChordExt: Sized {
    /// A key press, unless it is a Fn chord (Fn is voice typing's).
    fn from_keystroke(keystroke: &gpui::Keystroke) -> Option<Self>;

    /// Why the chord may not reach Silo or takes a key programs need; never blocking.
    fn warning(&self) -> Option<&'static str>;
}

impl ChordExt for Chord {
    fn from_keystroke(keystroke: &gpui::Keystroke) -> Option<Self> {
        let m = &keystroke.modifiers;
        if m.function {
            return None;
        }
        let mut chord = Self { ctrl: m.control, alt: m.alt, shift: m.shift, cmd: m.platform, key: String::new() };
        chord.set_key(&keystroke.key)?;
        Some(chord)
    }

    fn warning(&self) -> Option<&'static str> {
        let text = self.to_string();
        if RESERVED.contains(&text.as_str()) {
            Some("macOS uses this shortcut")
        } else if !self.cmd {
            Some("Terminal programs use this key")
        } else if !(self.ctrl || self.alt || self.shift) && crate::keys::terminal_cmd_key(&self.key) {
            Some("Terminals use it for line editing")
        } else {
            None
        }
    }
}
