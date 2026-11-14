//! Key bindings as `init.lua` and `keybindings.json` write them
//! (`docs/LUA.md` §4.1): a [`Chord`] is a key with modifiers
//! (`ctrl-alt-shift-cmd-key`, as gpui writes it), a [`Sequence`] chords
//! separated by spaces (`ctrl-b c`), and [`Keys`] a sequence in an optional
//! mode (`resize/left`). A shifted symbol is its own key (`cmd-}`, not
//! `cmd-shift-]`), as macOS reports it.

/// US layout: a symbol key and what Shift makes of it.
const SHIFTED: &[(char, char)] = &[
    ('`', '~'),
    ('1', '!'),
    ('2', '@'),
    ('3', '#'),
    ('4', '$'),
    ('5', '%'),
    ('6', '^'),
    ('7', '&'),
    ('8', '*'),
    ('9', '('),
    ('0', ')'),
    ('-', '_'),
    ('=', '+'),
    ('[', '{'),
    (']', '}'),
    ('\\', '|'),
    (';', ':'),
    ('\'', '"'),
    (',', '<'),
    ('.', '>'),
    ('/', '?'),
];

/// Named keys and their glyphs.
const NAMED: &[(&str, &str)] = &[
    ("enter", "↩"),
    ("tab", "⇥"),
    ("space", "Space"),
    ("backspace", "⌫"),
    ("delete", "⌦"),
    ("escape", "⎋"),
    ("up", "↑"),
    ("down", "↓"),
    ("left", "←"),
    ("right", "→"),
    ("home", "↖"),
    ("end", "↘"),
    ("pageup", "⇞"),
    ("pagedown", "⇟"),
];

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub cmd: bool,
    pub key: String,
}

fn function_key(key: &str) -> bool {
    key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| (1..=20).contains(&n))
}

impl Chord {
    /// `ctrl-alt-shift-cmd-key` in any modifier order; `cmd-shift-]` reads as `cmd-}`.
    ///
    /// # Errors
    /// Not one known key with `ctrl`, `alt`, `shift` or `cmd` modifiers.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || format!("“{text}” isn't a shortcut");
        let (mods, key) = match text.strip_suffix("--") {
            Some(mods) => (mods, "-"),
            None => match text.rsplit_once('-') {
                Some((mods, key)) => (mods, key),
                None => ("", text),
            },
        };
        let mut chord = Self { ctrl: false, alt: false, shift: false, cmd: false, key: String::new() };
        for m in mods.split('-').filter(|m| !m.is_empty()) {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => chord.ctrl = true,
                "alt" | "option" => chord.alt = true,
                "shift" => chord.shift = true,
                "cmd" | "command" => chord.cmd = true,
                _ => return Err(bad()),
            }
        }
        chord.set_key(key).ok_or_else(bad)?;
        Ok(chord)
    }

    /// Set the key as a key press names it (`A` is shift-a, shift-`]` is
    /// `}`); `None` for a key this does not know.
    pub fn set_key(&mut self, key: &str) -> Option<()> {
        let mut chars = key.chars();
        let key = match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_uppercase() => {
                self.shift = true;
                c.to_ascii_lowercase().to_string()
            }
            (Some(c), None) if c.is_ascii_graphic() => c.to_string(),
            _ => {
                let key = key.to_ascii_lowercase();
                (NAMED.iter().any(|(name, _)| *name == key) || function_key(&key)).then_some(key)?
            }
        };
        self.key = match SHIFTED.iter().find(|(base, _)| key.len() == 1 && key.starts_with(*base)) {
            Some((_, shifted)) if self.shift => {
                self.shift = false;
                shifted.to_string()
            }
            _ => key,
        };
        Some(())
    }

    /// In macOS notation: `⇧⌘N`, `⇧⌘]` for `cmd-}`, `⌥⌘←`.
    #[must_use]
    pub fn hint(&self) -> String {
        let shifted = SHIFTED.iter().find(|(_, shifted)| self.key.len() == 1 && self.key.starts_with(*shifted));
        let key = match (shifted, NAMED.iter().find(|(name, _)| *name == self.key)) {
            (Some((base, _)), _) => base.to_string(),
            (None, Some((_, glyph))) => (*glyph).to_owned(),
            (None, None) => self.key.to_uppercase(),
        };
        let mut out = String::new();
        for (on, glyph) in [(self.ctrl, "⌃"), (self.alt, "⌥"), (self.shift || shifted.is_some(), "⇧"), (self.cmd, "⌘")] {
            if on {
                out.push_str(glyph);
            }
        }
        out + &key
    }
}

impl std::fmt::Display for Chord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (on, name) in [(self.ctrl, "ctrl-"), (self.alt, "alt-"), (self.shift, "shift-"), (self.cmd, "cmd-")] {
            if on {
                f.write_str(name)?;
            }
        }
        f.write_str(&self.key)
    }
}

/// Chords pressed one after another (`ctrl-b c`); at least one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sequence(pub Vec<Chord>);

impl Sequence {
    /// Chords separated by spaces.
    ///
    /// # Errors
    /// No chord, or one that is not a shortcut.
    pub fn parse(text: &str) -> Result<Self, String> {
        let chords = text.split(' ').filter(|c| !c.is_empty()).map(Chord::parse).collect::<Result<Vec<_>, _>>()?;
        if chords.is_empty() {
            return Err(format!("“{text}” isn't a shortcut"));
        }
        Ok(Self(chords))
    }

    /// Each chord's [`Chord::hint`], separated by spaces: `⌃B C`.
    #[must_use]
    pub fn hint(&self) -> String {
        self.0.iter().map(Chord::hint).collect::<Vec<_>>().join(" ")
    }
}

impl std::fmt::Display for Sequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, chord) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{chord}")?;
        }
        Ok(())
    }
}

/// A binding's keys: a sequence, in a mode or not.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Keys {
    pub mode: Option<String>,
    pub sequence: Sequence,
}

impl std::fmt::Display for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(mode) = &self.mode {
            write!(f, "{mode}/")?;
        }
        write!(f, "{}", self.sequence)
    }
}

/// `mode/keys` or `keys`. A mode is [`valid_mode`] followed by `/` and
/// more; anything else has no mode (`cmd-/` is a chord, `resize//` is `/`
/// in mode `resize`).
///
/// # Errors
/// The keys are not a [`Sequence`].
pub fn parse(text: &str) -> Result<Keys, String> {
    let (mode, rest) = match text.split_once('/') {
        Some((mode, rest)) if valid_mode(mode) && !rest.is_empty() => (Some(mode.to_owned()), rest),
        _ => (None, text),
    };
    Ok(Keys { mode, sequence: Sequence::parse(rest)? })
}

/// Whether `name` is a mode name: `[a-z][a-z0-9_]*`.
#[must_use]
pub fn valid_mode(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase()) && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(text: &str) -> Chord {
        Chord::parse(text).unwrap()
    }

    #[test]
    fn chords_round_trip_in_canonical_order() {
        for (text, canonical, hint) in [
            ("cmd-shift-n", "shift-cmd-n", "⇧⌘N"),
            ("cmd-}", "cmd-}", "⇧⌘]"),
            ("cmd-shift-]", "cmd-}", "⇧⌘]"),
            ("cmd-alt-left", "alt-cmd-left", "⌥⌘←"),
            ("ctrl-cmd-enter", "ctrl-cmd-enter", "⌃⌘↩"),
            ("cmd-N", "shift-cmd-n", "⇧⌘N"),
            ("cmd--", "cmd--", "⌘-"),
            ("cmd-shift--", "cmd-_", "⇧⌘-"),
            ("ctrl-space", "ctrl-space", "⌃Space"),
            ("f5", "f5", "F5"),
            ("Command-Option-t", "alt-cmd-t", "⌥⌘T"),
        ] {
            let parsed = chord(text);
            assert_eq!(parsed.to_string(), canonical, "{text}");
            assert_eq!(parsed.hint(), hint, "{text}");
            assert_eq!(chord(canonical), parsed, "{canonical} parses back");
        }
        for bad in ["", "cmd-", "cmd-foo", "hyper-k", "cmd-f99"] {
            assert!(Chord::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sequences_are_chords_separated_by_spaces() {
        let seq = Sequence::parse("ctrl-b c").unwrap();
        assert_eq!(seq.0, [chord("ctrl-b"), chord("c")]);
        assert_eq!(seq.to_string(), "ctrl-b c");
        assert_eq!(seq.hint(), "⌃B C");
        assert_eq!(Sequence::parse("ctrl-b %").unwrap().to_string(), "ctrl-b %");
        assert_eq!(Sequence::parse("cmd-shift-k  cmd-]").unwrap().to_string(), "shift-cmd-k cmd-]");
        assert_eq!(Sequence::parse("cmd-k").unwrap().0.len(), 1);
        for bad in ["", " ", "ctrl-b nope", "cmd-"] {
            assert!(Sequence::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn keys_may_name_a_mode() {
        let keys = parse("resize/left").unwrap();
        assert_eq!((keys.mode.as_deref(), keys.sequence.to_string().as_str()), (Some("resize"), "left"));
        assert_eq!(keys.to_string(), "resize/left");
        let slash = parse("resize//").unwrap();
        assert_eq!((slash.mode.as_deref(), slash.sequence.0.clone()), (Some("resize"), vec![chord("/")]));
        let cmd_slash = parse("cmd-/").unwrap();
        assert_eq!((cmd_slash.mode, cmd_slash.sequence.0.clone()), (None, vec![chord("cmd-/")]));
        assert_eq!(parse("/").unwrap().sequence.0, [chord("/")]);
        let prefix = parse("tmux_2/ctrl-b c").unwrap();
        assert_eq!((prefix.mode.as_deref(), prefix.sequence.0.len()), (Some("tmux_2"), 2));
        assert_eq!(parse("ctrl-b c").unwrap().mode, None);
        // Not a mode name: the whole text is keys, which do not parse.
        assert!(parse("Resize/left").is_err());
        assert!(parse("resize/").is_err());
        assert!(parse("resize/nope").is_err());
    }

    #[test]
    fn mode_names() {
        for good in ["resize", "a", "tmux_2", "x9"] {
            assert!(valid_mode(good), "{good}");
        }
        for bad in ["", "Resize", "2x", "_a", "re-size", "rés"] {
            assert!(!valid_mode(bad), "{bad}");
        }
    }
}
