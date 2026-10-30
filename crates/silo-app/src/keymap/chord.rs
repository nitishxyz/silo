//! One shortcut: a key with modifiers, written as gpui writes it
//! (`ctrl-alt-shift-cmd-key`). A shifted symbol is its own key (`cmd-}`,
//! not `cmd-shift-]`), as macOS reports it.

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

    /// A key press, unless it is a Fn chord (Fn is voice typing's).
    #[must_use]
    pub fn from_keystroke(keystroke: &gpui::Keystroke) -> Option<Self> {
        let m = &keystroke.modifiers;
        if m.function {
            return None;
        }
        let mut chord = Self { ctrl: m.control, alt: m.alt, shift: m.shift, cmd: m.platform, key: String::new() };
        chord.set_key(&keystroke.key)?;
        Some(chord)
    }

    fn set_key(&mut self, key: &str) -> Option<()> {
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

impl Chord {
    /// Why the chord may not reach Silo or takes a key programs need; never blocking.
    #[must_use]
    pub fn warning(&self) -> Option<&'static str> {
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
