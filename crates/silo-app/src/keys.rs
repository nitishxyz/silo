//! macOS line-editing shortcuts on top of the upstream Ghostty key encoder.
//!
//! The encoder handles every key, including kitty keyboard and application
//! cursor modes. A few macOS chords have no useful *legacy* encoding though:
//! Cmd+Backspace loses the Cmd and arrives as a plain DEL, Ctrl+Backspace
//! is ^H, and Option/Cmd+arrows are xterm modifier sequences that shells do
//! not bind. Like Ghostty's default macOS bindings, these are rewritten to
//! the bytes shells understand — but only when the encoder produced the
//! legacy form, so apps that enabled the kitty protocol still receive the
//! exact Backspace chord with its modifiers. (Modified arrows look the same
//! in both protocols and are always mapped, like Ghostty's defaults.)

/// Which chord: exactly one of Cmd, Ctrl and Option, without Shift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chord {
    Cmd,
    Ctrl,
    Alt,
}

impl Chord {
    #[must_use]
    pub fn of(m: &gpui::Modifiers) -> Option<Self> {
        match (m.platform, m.control, m.alt, m.shift || m.function) {
            (true, false, false, false) => Some(Self::Cmd),
            (false, true, false, false) => Some(Self::Ctrl),
            (false, false, true, false) => Some(Self::Alt),
            _ => None,
        }
    }
}

/// Cmd chords the terminal handles itself (no app command uses them).
#[must_use]
pub fn terminal_cmd_key(key: &str) -> bool {
    matches!(key, "backspace" | "left" | "right")
}

/// Replacement bytes for a key press, given what the encoder produced.
#[must_use]
pub fn edit_override(key: &str, chord: Option<Chord>, encoded: &[u8]) -> Option<&'static [u8]> {
    let replacement: (&[u8], &'static [u8]) = match (key, chord?) {
        // Delete to the start of the line (readline unix-line-discard).
        ("backspace", Chord::Cmd) => (b"\x7f", b"\x15"),
        // Delete the previous word (unix-word-rubout).
        ("backspace", Chord::Ctrl) => (b"\x08", b"\x17"),
        // Option+Backspace is already ESC DEL (backward-kill-word).
        // Word left/right (readline backward-word / forward-word).
        ("left", Chord::Alt) => (b"\x1b[1;3D", b"\x1bb"),
        ("right", Chord::Alt) => (b"\x1b[1;3C", b"\x1bf"),
        // Start/end of line.
        ("left", Chord::Cmd) => (b"\x1b[1;9D", b"\x01"),
        ("right", Chord::Cmd) => (b"\x1b[1;9C", b"\x05"),
        _ => return None,
    };
    (encoded == replacement.0).then_some(replacement.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_vt::{GridSize, KeyEvent, Modifiers, TerminalCore};

    fn encode(vt: &mut TerminalCore, key: &str, chord: Chord, release: bool) -> Vec<u8> {
        let modifiers = Modifiers { super_key: chord == Chord::Cmd, control: chord == Chord::Ctrl, alt: chord == Chord::Alt, shift: false };
        let event = KeyEvent { key: key.into(), text: String::new(), modifiers, release };
        vt.encode_key(&event).unwrap()
    }

    /// What the pane sends for a press: the override or the encoder bytes.
    fn send(vt: &mut TerminalCore, key: &str, chord: Chord) -> Vec<u8> {
        let encoded = encode(vt, key, chord, false);
        edit_override(key, Some(chord), &encoded).map_or(encoded, <[u8]>::to_vec)
    }

    #[test]
    fn macos_editing_chords_in_legacy_mode() {
        let mut vt = TerminalCore::new(GridSize::new(20, 4).unwrap()).unwrap();
        assert_eq!(send(&mut vt, "backspace", Chord::Cmd), b"\x15");
        assert_eq!(send(&mut vt, "backspace", Chord::Alt), b"\x1b\x7f");
        assert_eq!(send(&mut vt, "backspace", Chord::Ctrl), b"\x17");
        assert_eq!(send(&mut vt, "left", Chord::Alt), b"\x1bb");
        assert_eq!(send(&mut vt, "right", Chord::Alt), b"\x1bf");
        assert_eq!(send(&mut vt, "left", Chord::Cmd), b"\x01");
        assert_eq!(send(&mut vt, "right", Chord::Cmd), b"\x05");
        // Releases stay silent in legacy mode: no duplicate writes.
        assert!(encode(&mut vt, "backspace", Chord::Cmd, true).is_empty());
        // Application cursor mode changes plain arrows, not these chords.
        vt.write_output(b"\x1b[?1h").unwrap();
        assert_eq!(send(&mut vt, "left", Chord::Alt), b"\x1bb");
        assert_eq!(edit_override("up", Some(Chord::Alt), b"\x1b[1;3A"), None);
    }

    #[test]
    fn kitty_keyboard_apps_get_the_real_key() {
        let mut vt = TerminalCore::new(GridSize::new(20, 4).unwrap()).unwrap();
        // CSI > 1 u: disambiguate escape codes.
        vt.write_output(b"\x1b[>1u").unwrap();
        // Modified arrows keep their CSI form under the kitty protocol, so
        // the arrow mappings apply there too — as Ghostty's own macOS
        // defaults (alt+left=esc:b) do.
        for (key, chord) in [("backspace", Chord::Cmd), ("backspace", Chord::Ctrl), ("backspace", Chord::Alt)] {
            let encoded = encode(&mut vt, key, chord, false);
            assert!(encoded.starts_with(b"\x1b["), "{key} {chord:?}: {encoded:?}");
            assert_eq!(edit_override(key, Some(chord), &encoded), None, "{key} {chord:?}");
        }
    }

    #[test]
    fn chords_need_exactly_one_modifier() {
        let m = |platform, control, alt, shift| gpui::Modifiers { platform, control, alt, shift, function: false };
        assert_eq!(Chord::of(&m(true, false, false, false)), Some(Chord::Cmd));
        assert_eq!(Chord::of(&m(true, false, false, true)), None);
        assert_eq!(Chord::of(&m(false, true, true, false)), None);
        assert_eq!(edit_override("backspace", None, b"\x7f"), None);
        assert!(terminal_cmd_key("backspace") && !terminal_cmd_key("c"));
    }
}
