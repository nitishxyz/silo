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
    NewTab => "New Tab", "Tabs", ["cmd-t"], "Same kind as the focused block";
    NewTerminalTab => "New Terminal Tab", "Tabs", [];
    NewBrowserTab => "New Browser Tab", "Tabs", ["cmd-alt-t"];
    NewAgentTab => "New Agent Tab", "Tabs", [];
    NewTabChooser => "New Tab…", "Tabs", ["ctrl-cmd-t"], "Choose a terminal, browser, agent, address or >command";
    CloseTab => "Close Tab", "Tabs", ["cmd-shift-w"];
    RenameTab => "Rename Tab…", "Tabs", [];
    NextTab => "Next Tab", "Tabs", ["cmd-}", "cmd-shift-]"];
    PreviousTab => "Previous Tab", "Tabs", ["cmd-{", "cmd-shift-["];
    // Panes
    NewPane => "New Pane", "Panes", ["cmd-enter"], "Beside the focused pane, along its longer side";
    SplitRight => "Split Right", "Panes", ["cmd-d"];
    SplitDown => "Split Down", "Panes", ["cmd-shift-d"];
    NewPaneChooser => "New Pane…", "Panes", ["ctrl-cmd-enter"], "Choose what opens beside the focused pane";
    SplitRightChooser => "Split Right…", "Panes", ["ctrl-cmd-d"], "Choose what opens on the right";
    SplitDownChooser => "Split Down…", "Panes", ["ctrl-cmd-shift-d"], "Choose what opens below";
    ClosePane => "Close Pane", "Panes", ["cmd-w"];
    FocusNextPane => "Focus Next Pane", "Panes", ["cmd-]"];
    FocusPreviousPane => "Focus Previous Pane", "Panes", ["cmd-["];
    FocusPaneLeft => "Focus Pane Left", "Panes", ["ctrl-cmd-h", "cmd-alt-left"];
    FocusPaneRight => "Focus Pane Right", "Panes", ["ctrl-cmd-l", "cmd-alt-right"];
    FocusPaneUp => "Focus Pane Up", "Panes", ["ctrl-cmd-k", "cmd-alt-up"];
    FocusPaneDown => "Focus Pane Down", "Panes", ["ctrl-cmd-j", "cmd-alt-down"];
    GrowPane => "Grow Pane", "Panes", ["ctrl-cmd-]"];
    ShrinkPane => "Shrink Pane", "Panes", ["ctrl-cmd-["];
    MovePaneEdgeLeft => "Move Pane Edge Left", "Panes", ["ctrl-cmd-left"];
    MovePaneEdgeRight => "Move Pane Edge Right", "Panes", ["ctrl-cmd-right"];
    MovePaneEdgeUp => "Move Pane Edge Up", "Panes", ["ctrl-cmd-up"];
    MovePaneEdgeDown => "Move Pane Edge Down", "Panes", ["ctrl-cmd-down"];
    EqualizePanes => "Equalize Panes", "Panes", ["ctrl-cmd-="];
    ZoomPane => "Zoom Pane", "Panes", ["cmd-shift-enter"], "Toggle";
    FloatPane => "Float Pane", "Panes", [], "Lift it above the layout";
    DockPane => "Dock Pane", "Panes", [], "Put it back in the layout";
    // Browser
    NewBrowser => "New Browser", "Browser", [];
    OpenUrl => "Open URL…", "Browser", [];
    // Agent
    NewAgent => "New Agent", "Agent", ["cmd-shift-o"], "An otto agent beside the focused pane";
    OttoSettings => "Otto Settings", "Agent", [], "Providers, models and agents of the agent in front";
    // Hosts
    AddHost => "Add Machine…", "Hosts", [], "Pair another machine running Silo";
    RenameHost => "Rename Host…", "Hosts", [];
    RemoveHost => "Remove Host…", "Hosts", [];
    // Sharing
    ShareSession => "Share Session…", "Sharing", [], "A link to watch or type in this session";
    SharePane => "Share Pane…", "Sharing", [], "A link to watch or type in this pane";
    OpenSharedLink => "Open Shared Link…", "Sharing", [];
    // Edit & terminal
    Copy => "Copy", "Edit", ["cmd-c"];
    Paste => "Paste", "Edit", ["cmd-v"];
    SelectAll => "Select All", "Edit", ["cmd-a"];
    ClearScrollback => "Clear Scrollback", "Terminal", [], "Clears the screen and history for every client";
    ResetTerminal => "Reset Terminal", "Terminal", [];
    JumpToPreviousPrompt => "Jump to Previous Prompt", "Terminal", ["cmd-up"];
    JumpToNextPrompt => "Jump to Next Prompt", "Terminal", ["cmd-down"];
    SelectCommandOutput => "Select Command Output", "Terminal", ["cmd-shift-a"], "The last command's output";
    CopyCommandOutput => "Copy Command Output", "Terminal", ["cmd-alt-c"], "The last command's output";
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
    CheckForUpdates => "Check for Updates", "App", [];
    CommandPalette => "Command Palette", "App", ["cmd-p"];
    HideSilo => "Hide Silo", "App", ["cmd-h"];
    HideOthers => "Hide Others", "App", ["cmd-alt-h"];
    ShowAll => "Show All", "App", [];
    Quit => "Quit Silo", "App", ["cmd-q"];
}

/// How automation runs a command (`docs/PHASE7.md` §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    /// A control method (`app.*` ones run in a connected app).
    Method(&'static str),
    /// A CLI subcommand: there is no server to call.
    Cli(&'static str),
}

impl Command {
    /// The control method (or CLI subcommand) doing what this command does.
    #[must_use]
    pub fn api(self) -> Api {
        use Api::{Cli, Method};
        match self {
            Self::NewSession => Method("session.create"),
            Self::NewWindow => Method("app.window_new"),
            Self::RenameSession => Method("session.rename"),
            Self::CloseSession => Method("session.close"),
            Self::SwitchSession => Method("app.switcher"),
            Self::NextSession | Self::PreviousSession => Method("app.session_step"),
            // The chooser's pick is the `kind` param.
            Self::NewTab | Self::NewTerminalTab | Self::NewBrowserTab | Self::NewAgentTab | Self::NewTabChooser => Method("window.create"),
            Self::CloseTab => Method("window.close"),
            Self::RenameTab => Method("window.rename"),
            Self::NextTab | Self::PreviousTab => Method("window.step"),
            // The launcher's pick is the `kind` (and `url`) param.
            Self::NewPane
            | Self::SplitRight
            | Self::SplitDown
            | Self::NewPaneChooser
            | Self::SplitRightChooser
            | Self::SplitDownChooser
            | Self::NewBrowser
            | Self::NewAgent => Method("block.split"),
            Self::OttoSettings => Method("app.settings_open"),
            Self::ClosePane => Method("block.close"),
            Self::FocusNextPane | Self::FocusPreviousPane => Method("block.focus_step"),
            Self::FocusPaneLeft | Self::FocusPaneRight | Self::FocusPaneUp | Self::FocusPaneDown => Method("block.focus_dir"),
            Self::GrowPane | Self::ShrinkPane => Method("layout.grow"),
            Self::MovePaneEdgeLeft | Self::MovePaneEdgeRight | Self::MovePaneEdgeUp | Self::MovePaneEdgeDown => Method("layout.edge"),
            Self::EqualizePanes => Method("layout.equalize"),
            Self::ZoomPane => Method("block.zoom"),
            Self::FloatPane => Method("block.float"),
            Self::DockPane => Method("block.move"),
            Self::OpenUrl => Method("browser.navigate"),
            Self::AddHost => Method("app.hosts_add"),
            Self::RenameHost => Method("app.hosts_rename"),
            Self::RemoveHost => Method("app.hosts_remove"),
            Self::ShareSession | Self::SharePane => Method("grant.create"),
            Self::OpenSharedLink => Method("app.open_share"),
            Self::Copy => Method("app.copy"),
            Self::Paste => Method("app.paste"),
            Self::SelectAll => Method("app.select_all"),
            Self::ClearScrollback => Method("terminal.clear"),
            Self::ResetTerminal => Method("terminal.reset"),
            Self::JumpToPreviousPrompt | Self::JumpToNextPrompt => Method("app.jump_prompt"),
            Self::SelectCommandOutput => Method("app.select_output"),
            Self::CopyCommandOutput => Method("app.copy_output"),
            Self::RestartServer => Method("server.restart"),
            Self::StopServer => Method("server.shutdown"),
            Self::StartServer => Cli("up"),
            Self::ShowServerLog => Method("server.log"),
            Self::AppearanceComfortable | Self::AppearanceCompact => Method("app.appearance"),
            Self::ShowPaneHeaders | Self::HidePaneHeaders => Method("app.pane_headers"),
            Self::ToggleFullScreen => Method("app.full_screen"),
            Self::CycleWindows => Method("app.window_cycle"),
            Self::OpenSettings => Method("app.settings_open"),
            Self::ReloadTerminalSettings => Method("app.settings_reload"),
            Self::OpenTerminalSettingsFile => Method("app.settings_open_file"),
            Self::CheckForUpdates => Method("app.update_check"),
            Self::CommandPalette => Method("app.palette"),
            Self::HideSilo => Method("app.hide"),
            Self::HideOthers => Method("app.hide_others"),
            Self::ShowAll => Method("app.show_all"),
            Self::Quit => Method("app.quit"),
        }
    }

    /// The host feature (`silo_proto::compat::feature`) the command needs;
    /// none yet, every protocol-1 host has what these use.
    #[must_use]
    pub fn feature(self) -> Option<&'static str> {
        None
    }

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
    // A contiguous match (ASCII, so byte offsets agree) is highlighted as
    // one run and scores as consecutive matches would.
    let needle: String = query.iter().collect();
    if text.is_ascii()
        && let Some(start) = text.to_ascii_lowercase().find(&needle)
    {
        let word_start = text[..start].chars().next_back().is_none_or(|p| !p.is_alphanumeric());
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let score = (needle.len() as i32) * 6 - 5 + if word_start { 8 } else { 0 } - text.len() as i32 / 8;
        #[allow(clippy::single_range_in_vec_init, reason = "one highlighted run")]
        return Some((score, vec![start..start + needle.len()]));
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
        assert_eq!(Command::FocusPaneLeft.hint().as_deref(), Some("⌃⌘H"));
        assert_eq!(Command::FocusPaneUp.hint().as_deref(), Some("⌃⌘K"));
        assert_eq!(Command::FocusPaneDown.hint().as_deref(), Some("⌃⌘J"));
        assert_eq!(Command::FocusPaneRight.hint().as_deref(), Some("⌃⌘L"));
        assert_eq!(Command::GrowPane.hint().as_deref(), Some("⌃⌘]"));
        assert_eq!(Command::MovePaneEdgeLeft.hint().as_deref(), Some("⌃⌘←"));
        assert_eq!(Command::EqualizePanes.hint().as_deref(), Some("⌃⌘="));
        assert_eq!(Command::NewPane.hint().as_deref(), Some("⌘↩"));
        assert_eq!(Command::NewTab.hint().as_deref(), Some("⌘T"));
        assert_eq!(Command::NewBrowserTab.hint().as_deref(), Some("⌥⌘T"));
        assert_eq!(Command::NewTabChooser.hint().as_deref(), Some("⌃⌘T"));
        assert_eq!(Command::SplitRightChooser.hint().as_deref(), Some("⌃⌘D"));
        assert_eq!(Command::SplitDownChooser.hint().as_deref(), Some("⌃⇧⌘D"));
        assert_eq!(Command::NewPaneChooser.hint().as_deref(), Some("⌃⌘↩"));
        assert_eq!(Command::SplitRight.hint().as_deref(), Some("⌘D"), "instant splits keep their keys");
        assert_eq!(Command::SplitDown.hint().as_deref(), Some("⇧⌘D"));
        assert_eq!(Command::NewTerminalTab.hint(), None);
        assert_eq!(Command::NewAgent.hint().as_deref(), Some("⇧⌘O"));
        assert_eq!(Command::JumpToPreviousPrompt.hint().as_deref(), Some("⌘↑"));
        assert_eq!(Command::JumpToNextPrompt.hint().as_deref(), Some("⌘↓"));
        assert_eq!(Command::SelectCommandOutput.hint().as_deref(), Some("⇧⌘A"));
        assert_eq!(Command::CopyCommandOutput.hint().as_deref(), Some("⌥⌘C"));
    }

    #[test]
    fn every_command_has_an_api_method() {
        let methods: Vec<&str> = silo_proto::control::METHODS.iter().map(|(name, _)| *name).collect();
        for command in Command::ALL {
            match command.api() {
                Api::Method(name) => assert!(methods.contains(&name), "{command:?}: {name} is not a method"),
                Api::Cli(name) => assert_eq!((*command, name), (Command::StartServer, "up"), "only Start Server has no method"),
            }
        }
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
        assert_eq!(fuzzy("light", "Silo Light").unwrap().1, vec![5..10], "a contiguous match is one run");
        assert_eq!(fuzzy("ght", "Silo Light").unwrap().1, vec![7..10]);
        assert!(fuzzy("", "anything").is_some());
    }
}
