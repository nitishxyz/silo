//! Central command registry: every app command has one GPUI action, one
//! (optional) key binding and palette metadata. Key bindings, the action
//! handlers on the workspace and the ⌘P palette are all generated from this
//! table, so the shortcuts shown in the palette are the real ones.

use gpui::{App, Context, Div, KeyBinding, Stateful, prelude::*};

use crate::workspace::Workspace;

macro_rules! commands {
    ($( $variant:ident => $title:literal, $group:literal, [$($key:literal),*] $(, $subtitle:literal)? ;)*) => {
        /// GPUI actions, one per command.
        pub mod act {
            gpui::actions!(silo, [$($variant),*]);
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[allow(clippy::enum_variant_names, reason = "CommandPalette is the palette's own command")]
        pub enum Command {
            $($variant),*
        }

        impl Command {
            pub const ALL: &[Command] = &[$(Command::$variant),*];

            #[must_use]
            pub fn title(self) -> &'static str {
                match self { $(Command::$variant => $title),* }
            }

            #[must_use]
            pub fn group(self) -> &'static str {
                match self { $(Command::$variant => $group),* }
            }

            #[must_use]
            #[allow(unreachable_patterns)]
            pub fn subtitle(self) -> Option<&'static str> {
                match self {
                    $($(Command::$variant => Some($subtitle),)?)*
                    _ => None,
                }
            }

            /// Key bindings; the first is shown as the hint.
            #[must_use]
            pub fn keys(self) -> &'static [&'static str] {
                match self { $(Command::$variant => &[$($key),*]),* }
            }

            #[must_use]
            pub fn action(self) -> Box<dyn gpui::Action> {
                match self { $(Command::$variant => Box::new(act::$variant)),* }
            }
        }

        /// Route every command action on `element` to [`Workspace::run`].
        pub fn handle_actions(element: Stateful<Div>, cx: &mut Context<Workspace>) -> Stateful<Div> {
            element
                $(.on_action(cx.listener(|this, _: &act::$variant, window, cx| this.run(Command::$variant, window, cx))))*
        }
    };
}

commands! {
    // Sessions
    NewSession => "New Session", "Sessions", ["cmd-shift-n"];
    NewWindow => "New Window…", "Sessions", ["cmd-n"], "Open a session in another window";
    RenameSession => "Rename Session…", "Sessions", [];
    CloseSession => "Close Session", "Sessions", [], "Ends every process in the session";
    SwitchSession => "Switch to Session…", "Sessions", ["cmd-k"];
    NextSession => "Next Session", "Sessions", ["cmd-alt-]"];
    PreviousSession => "Previous Session", "Sessions", ["cmd-alt-["];
    // Tabs
    NewTab => "New Tab", "Tabs", ["cmd-t"];
    CloseTab => "Close Tab", "Tabs", ["cmd-shift-w"];
    RenameTab => "Rename Tab…", "Tabs", [];
    NextTab => "Next Tab", "Tabs", ["cmd-}", "cmd-shift-]"];
    PreviousTab => "Previous Tab", "Tabs", ["cmd-{", "cmd-shift-["];
    // Panes
    SplitRight => "Split Right", "Panes", ["cmd-d"];
    SplitDown => "Split Down", "Panes", ["cmd-shift-d"];
    ClosePane => "Close Pane", "Panes", ["cmd-w"];
    FocusNextPane => "Focus Next Pane", "Panes", ["cmd-]"];
    FocusPreviousPane => "Focus Previous Pane", "Panes", ["cmd-["];
    FocusPaneLeft => "Focus Pane Left", "Panes", ["cmd-h", "cmd-alt-left"];
    FocusPaneRight => "Focus Pane Right", "Panes", ["cmd-l", "cmd-alt-right"];
    FocusPaneUp => "Focus Pane Up", "Panes", ["cmd-alt-up"];
    FocusPaneDown => "Focus Pane Down", "Panes", ["cmd-j", "cmd-alt-down"];
    ZoomPane => "Zoom Pane", "Panes", ["cmd-shift-enter"], "Toggle";
    FloatPane => "Float Pane", "Panes", [], "Lift it above the layout";
    DockPane => "Dock Pane", "Panes", [], "Put it back in the layout";
    // Browser
    NewBrowser => "New Browser", "Browser", [];
    OpenUrl => "Open URL…", "Browser", [];
    // Hosts
    AddHost => "Add Remote Host…", "Hosts", [];
    RemoveHost => "Remove Host…", "Hosts", [];
    // Edit & terminal
    Copy => "Copy", "Edit", ["cmd-c"];
    Paste => "Paste", "Edit", ["cmd-v"];
    SelectAll => "Select All", "Edit", ["cmd-a"];
    ClearScrollback => "Clear Scrollback", "Terminal", [], "Clears the screen and history for every client";
    ResetTerminal => "Reset Terminal", "Terminal", [];
    // Server & app
    RestartServer => "Restart Server", "Server", [], "Ends every session";
    StopServer => "Stop Server", "Server", [], "Ends every session";
    StartServer => "Start Server", "Server", [];
    ShowServerLog => "Show Server Log", "Server", [];
    // Appearance
    AppearanceComfortable => "Appearance: Comfortable", "View", [], "Rounded pane cards";
    AppearanceCompact => "Appearance: Compact", "View", [], "Flush panes with dividers";
    ShowPaneHeaders => "Show Pane Headers", "View", [];
    HidePaneHeaders => "Hide Pane Headers", "View", [];
    ToggleFullScreen => "Toggle Full Screen", "View", ["ctrl-cmd-f"];
    CycleWindows => "Cycle Through Windows", "View", ["cmd-`"];
    OpenSettings => "Settings…", "App", ["cmd-,"];
    ReloadTerminalSettings => "Reload Terminal Settings", "App", [], "Apply changes made by hand";
    OpenTerminalSettingsFile => "Open Terminal Settings File", "App", [];
    CommandPalette => "Command Palette", "App", ["cmd-p"];
    Quit => "Quit Silo", "App", ["cmd-q"];
}

impl Command {
    /// The shortcut hint in macOS notation, e.g. `⇧⌘N`.
    #[must_use]
    pub fn hint(self) -> Option<String> {
        self.keys().first().map(|k| key_hint(k))
    }

    /// Commands that need confirmation in the palette.
    #[must_use]
    pub fn destructive(self) -> bool {
        matches!(self, Self::CloseSession | Self::RestartServer | Self::StopServer)
    }
}

/// `cmd-shift-n` → `⇧⌘N`, `cmd-}` → `⇧⌘]`, `cmd-alt-left` → `⌥⌘←`.
#[must_use]
pub fn key_hint(binding: &str) -> String {
    let parts: Vec<&str> = binding.split('-').collect();
    let (mods, key) = match parts.split_last() {
        // `cmd--` style bindings (a literal minus) are not used here.
        Some((key, mods)) => (mods, *key),
        None => return binding.to_owned(),
    };
    let (mut ctrl, mut alt, mut shift, mut cmd) = (false, false, false, false);
    for m in mods {
        match *m {
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "cmd" => cmd = true,
            _ => {}
        }
    }
    let key = match key {
        "}" => {
            shift = true;
            "]".to_owned()
        }
        "{" => {
            shift = true;
            "[".to_owned()
        }
        "enter" => "↩".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "escape" => "⎋".into(),
        "tab" => "⇥".into(),
        "space" => "Space".into(),
        "backspace" => "⌫".into(),
        k => k.to_uppercase(),
    };
    let mut out = String::new();
    for (on, glyph) in [(ctrl, "⌃"), (alt, "⌥"), (shift, "⇧"), (cmd, "⌘")] {
        if on {
            out.push_str(glyph);
        }
    }
    out.push_str(&key);
    out
}

/// Install every command's key bindings (plus ⌘1–9, handled separately).
pub fn bind_keys(cx: &mut App) {
    let mut bindings = Vec::new();
    for command in Command::ALL {
        for key in command.keys() {
            match KeyBinding::load(key, command.action(), None, false, None, &gpui::DummyKeyboardMapper) {
                Ok(binding) => bindings.push(binding),
                Err(error) => tracing::warn!("bad key binding {key:?}: {error:?}"),
            }
        }
    }
    cx.bind_keys(bindings);
    crate::workspace::bind_number_keys(cx);
}

/// Fuzzy subsequence match. Returns a score (higher is better) and the
/// matched byte ranges of `text` for highlighting.
#[must_use]
pub fn fuzzy(query: &str, text: &str) -> Option<(i32, Vec<std::ops::Range<usize>>)> {
    let query: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut score = 0;
    let mut qi = 0;
    let mut prev_match: Option<usize> = None;
    let mut prev_char: Option<char> = None;
    for (byte, ch) in text.char_indices() {
        if qi < query.len() && ch.to_lowercase().eq(std::iter::once(query[qi])) {
            let word_start = prev_char.is_none_or(|p| !p.is_alphanumeric());
            score += 1 + if word_start { 8 } else { 0 } + if prev_match.is_some_and(|p| p + 1 == byte) { 5 } else { 0 };
            let end = byte + ch.len_utf8();
            match ranges.last_mut() {
                Some(last) if last.end == byte => last.end = end,
                _ => ranges.push(byte..end),
            }
            prev_match = Some(byte + ch.len_utf8() - 1);
            qi += 1;
        }
        prev_char = Some(ch);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let penalty = text.len() as i32 / 8;
    (qi == query.len()).then_some((score - penalty, ranges))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_use_mac_glyphs() {
        assert_eq!(key_hint("cmd-shift-n"), "⇧⌘N");
        assert_eq!(key_hint("cmd-}"), "⇧⌘]");
        assert_eq!(key_hint("cmd-alt-left"), "⌥⌘←");
        assert_eq!(key_hint("cmd-shift-enter"), "⇧⌘↩");
        assert_eq!(Command::NewSession.hint().as_deref(), Some("⇧⌘N"));
        assert_eq!(Command::RenameSession.hint(), None);
        assert_eq!(Command::SwitchSession.hint().as_deref(), Some("⌘K"));
        assert_eq!(Command::FocusPaneLeft.hint().as_deref(), Some("⌘H"));
        assert_eq!(Command::FocusPaneUp.hint().as_deref(), Some("⌥⌘↑"));
    }

    #[test]
    fn bindings_are_unique() {
        let mut seen = std::collections::HashMap::new();
        for c in Command::ALL {
            for k in c.keys() {
                if let Some(other) = seen.insert(*k, *c) {
                    panic!("{k} bound to {other:?} and {c:?}");
                }
            }
        }
    }

    #[test]
    fn fuzzy_prefers_word_starts() {
        let (a, ranges) = fuzzy("spr", "Split Right").unwrap();
        assert_eq!(ranges, vec![0..2, 6..7]);
        let (b, _) = fuzzy("spr", "Show Server Log... previous").unwrap_or((i32::MIN, vec![]));
        assert!(a > b);
        assert!(fuzzy("xyz", "Split Right").is_none());
        assert!(fuzzy("", "anything").is_some());
    }
}
