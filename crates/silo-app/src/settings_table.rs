//! Every user-facing setting, declared once: the settings panel takes its
//! row labels, search keywords and visibility from here, and the ⌘P finder
//! lists, reads and applies settings through it.

use crate::{
    hosts::NotifyField,
    material::Material,
    settings_panel::{ColorSlot, CursorShape, Density, Section, SettingsEvent, SettingsSnapshot},
    terminal_prefs,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingId {
    PaneDensity,
    PaneHeaders,
    Theme,
    MatchRunningApp,
    WindowMaterial,
    TintWithApp,
    TranslucentApps,
    InterfaceFont,
    FontFamily,
    FontSize,
    Color(ColorSlot),
    CursorShape,
    CursorBlink,
    Scrollback,
    RestoreDefaults,
    ConfirmClose,
    CheckUpdates,
    Notifications,
    NotifyOnlyHidden,
    NotifyBlocked,
    NotifyDone,
    NotifyError,
    NotifyProgram,
    Shortcuts,
    Hosts,
    PairDevice,
}

/// How the finder edits a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Return flips it.
    Toggle,
    /// A sub-list of values.
    Choice,
    /// A typed number.
    Number,
    /// Only in the settings panel.
    Open,
}

#[derive(Debug)]
pub struct Setting {
    pub id: SettingId,
    /// The finder's title.
    pub title: &'static str,
    /// The panel's row label (within its group).
    pub label: &'static str,
    pub section: Section,
    /// The panel group holding the row (scrolled to by "Open in Settings").
    pub group: Option<&'static str>,
    pub keywords: &'static str,
    pub kind: Kind,
}

macro_rules! setting {
    ($id:expr, $title:literal, $label:literal, $section:ident, $group:expr, $kind:ident, $keywords:literal) => {
        Setting { id: $id, title: $title, label: $label, section: Section::$section, group: $group, keywords: $keywords, kind: Kind::$kind }
    };
}

pub const SETTINGS: &[Setting] = &[
    setting!(SettingId::PaneDensity, "Pane density", "Pane density", Appearance, Some("Layout"), Choice, "density compact comfortable cards layout"),
    setting!(SettingId::PaneHeaders, "Pane headers", "Pane headers", Appearance, Some("Layout"), Toggle, "pane headers titles bar"),
    setting!(SettingId::Theme, "Theme", "Theme", Terminal, None, Choice, "theme colors palette light dark scheme"),
    setting!(
        SettingId::MatchRunningApp,
        "Match running app",
        "Match running app",
        Appearance,
        Some("Colors"),
        Toggle,
        "match running app adaptive backdrop background chrome padding"
    ),
    setting!(
        SettingId::WindowMaterial,
        "Window material",
        "Window",
        Appearance,
        Some("Material"),
        Choice,
        "material window solid frosted blur glass"
    ),
    setting!(SettingId::TintWithApp, "Tint with running app", "Tint with running app", Appearance, Some("Material"), Toggle, "tint blur running app"),
    setting!(
        SettingId::TranslucentApps,
        "Translucent app backgrounds",
        "Translucent app backgrounds",
        Appearance,
        Some("Material"),
        Toggle,
        "translucent transparent app backgrounds blur"
    ),
    setting!(
        SettingId::InterfaceFont,
        "Interface font",
        "Interface font",
        Appearance,
        Some("Font"),
        Choice,
        "ui interface font family typeface system automatic"
    ),
    setting!(SettingId::FontFamily, "Terminal font", "Family", Terminal, Some("Font"), Choice, "font family nerd automatic typeface monospace"),
    setting!(SettingId::FontSize, "Font size", "Size", Terminal, Some("Font"), Number, "font size zoom bigger smaller points"),
    setting!(
        SettingId::Color(ColorSlot::Foreground),
        "Foreground color",
        "Foreground",
        Terminal,
        Some("Colors"),
        Open,
        "foreground text color colors"
    ),
    setting!(SettingId::Color(ColorSlot::Background), "Background color", "Background", Terminal, Some("Colors"), Open, "background color colors"),
    setting!(SettingId::Color(ColorSlot::Cursor), "Cursor color", "Cursor", Terminal, Some("Colors"), Open, "cursor color colors"),
    setting!(
        SettingId::Color(ColorSlot::Selection),
        "Selection color",
        "Selection",
        Terminal,
        Some("Colors"),
        Open,
        "selection highlight color colors"
    ),
    setting!(SettingId::CursorShape, "Cursor shape", "Shape", Terminal, Some("Cursor"), Choice, "cursor shape block bar underline"),
    setting!(SettingId::CursorBlink, "Cursor blinking", "Blinking", Terminal, Some("Cursor"), Toggle, "cursor blink blinking"),
    setting!(SettingId::Scrollback, "Scrollback", "History", Terminal, Some("Scrollback"), Number, "scrollback history lines buffer"),
    setting!(
        SettingId::ConfirmClose,
        "Confirm before closing running programs",
        "Confirm before closing running programs",
        Terminal,
        Some("Closing"),
        Toggle,
        "confirm close closing quit running programs processes busy ask warn"
    ),
    setting!(
        SettingId::RestoreDefaults,
        "Restore terminal defaults",
        "Restore defaults",
        Terminal,
        Some("Scrollback"),
        Open,
        "reset restore defaults"
    ),
    setting!(SettingId::Shortcuts, "Keyboard shortcuts", "Keyboard shortcuts", Keyboard, None, Open, "shortcuts keys bindings keyboard"),
    setting!(
        SettingId::CheckUpdates,
        "Check for updates automatically",
        "Check for updates automatically",
        About,
        Some("Updates"),
        Toggle,
        "update updates upgrade new version release automatic"
    ),
    setting!(
        SettingId::Notifications,
        "Notifications",
        "Show notifications",
        Notifications,
        Some("Notifications"),
        Toggle,
        "notifications alerts banners"
    ),
    setting!(
        SettingId::NotifyOnlyHidden,
        "Notify only for blocks not on screen",
        "Only when the block isn't visible",
        Notifications,
        Some("Notifications"),
        Toggle,
        "notifications visible hidden screen focused"
    ),
    setting!(
        SettingId::NotifyBlocked,
        "Notify when a program needs you",
        "A program needs you",
        Notifications,
        Some("Notify when"),
        Toggle,
        "notifications blocked needs you permission question"
    ),
    setting!(
        SettingId::NotifyDone,
        "Notify when a program is done",
        "A program is done",
        Notifications,
        Some("Notify when"),
        Toggle,
        "notifications done finished"
    ),
    setting!(
        SettingId::NotifyError,
        "Notify when a program fails",
        "A program fails",
        Notifications,
        Some("Notify when"),
        Toggle,
        "notifications error failed"
    ),
    setting!(
        SettingId::NotifyProgram,
        "Program notifications",
        "A program sends a notification",
        Notifications,
        Some("Notify when"),
        Toggle,
        "notifications osc 9 777 program"
    ),
    setting!(SettingId::Hosts, "Hosts", "Hosts", Hosts, None, Open, "hosts machines remote add remove rename"),
    setting!(
        SettingId::PairDevice,
        "Pair a device",
        "New device",
        Server,
        Some("Paired devices"),
        Open,
        "pair device devices ticket phone invite revoke paired"
    ),
];

/// A value of a [`Kind::Choice`] setting.
#[derive(Clone, Debug)]
pub struct Choice {
    pub label: String,
    pub event: SettingsEvent,
    pub selected: bool,
    /// Theme tiles: background and foreground.
    pub swatch: Option<(u32, u32)>,
}

fn frosted(s: &SettingsSnapshot) -> bool {
    s.material == Material::Frosted && s.material_unavailable.is_none()
}

fn on_off(on: bool) -> String {
    if on { "On" } else { "Off" }.into()
}

const DENSITIES: [(Density, &str); 2] = [(Density::Comfortable, "Comfortable"), (Density::Compact, "Compact")];
const MATERIALS: [(Material, &str); 2] = [(Material::Solid, "Solid"), (Material::Frosted, "Frosted")];
pub const CURSOR_SHAPES: [(CursorShape, &str); 3] = [(CursorShape::Block, "Block"), (CursorShape::Bar, "Bar"), (CursorShape::Underline, "Underline")];

fn label_of<T: PartialEq>(options: &[(T, &'static str)], value: &T) -> String {
    options.iter().find(|(v, _)| v == value).map_or("", |(_, l)| l).into()
}

impl SettingId {
    /// The notification switch this setting is.
    fn notify_field(self) -> Option<NotifyField> {
        Some(match self {
            Self::Notifications => NotifyField::On,
            Self::NotifyOnlyHidden => NotifyField::OnlyHidden,
            Self::NotifyBlocked => NotifyField::Blocked,
            Self::NotifyDone => NotifyField::Done,
            Self::NotifyError => NotifyField::Error,
            Self::NotifyProgram => NotifyField::Program,
            _ => return None,
        })
    }

    #[must_use]
    pub fn setting(self) -> &'static Setting {
        SETTINGS.iter().find(|s| s.id == self).expect("every setting is declared")
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        self.setting().label
    }

    /// Whether the panel shows it now (some apply to one material only).
    #[must_use]
    pub fn visible(self, s: &SettingsSnapshot) -> bool {
        match self {
            Self::MatchRunningApp => !frosted(s),
            Self::TintWithApp | Self::TranslucentApps => frosted(s),
            Self::CheckUpdates => s.updates,
            _ => true,
        }
    }

    /// A toggle's state.
    #[must_use]
    pub fn on(self, s: &SettingsSnapshot) -> Option<bool> {
        if let Some(field) = self.notify_field() {
            return Some(s.notify.get(field));
        }
        Some(match self {
            Self::PaneHeaders => s.show_pane_headers,
            Self::MatchRunningApp => s.match_running_app,
            Self::TintWithApp => s.tint_with_app,
            Self::TranslucentApps => s.translucent_apps,
            Self::CursorBlink => s.terminal.cursor_blink,
            Self::ConfirmClose => s.confirm_close,
            Self::CheckUpdates => s.check_updates,
            _ => return None,
        })
    }

    /// A toggle set to `on`.
    #[must_use]
    pub fn toggled(self, on: bool) -> Option<SettingsEvent> {
        if let Some(field) = self.notify_field() {
            return Some(SettingsEvent::SetNotify(field, on));
        }
        Some(match self {
            Self::PaneHeaders => SettingsEvent::SetPaneHeaders(on),
            Self::MatchRunningApp => SettingsEvent::SetMatchRunningApp(on),
            Self::TintWithApp => SettingsEvent::SetTintWithApp(on),
            Self::TranslucentApps => SettingsEvent::SetTranslucentApps(on),
            Self::CursorBlink => SettingsEvent::SetCursorBlink(on),
            Self::ConfirmClose => SettingsEvent::SetConfirmClose(on),
            Self::CheckUpdates => SettingsEvent::SetCheckUpdates(on),
            _ => return None,
        })
    }

    /// Record a toggle's new state in `s` (the app applies it a moment later).
    pub fn set_on(self, s: &mut SettingsSnapshot, on: bool) {
        if let Some(field) = self.notify_field() {
            return s.notify.set(field, on);
        }
        match self {
            Self::PaneHeaders => s.show_pane_headers = on,
            Self::MatchRunningApp => s.match_running_app = on,
            Self::TintWithApp => s.tint_with_app = on,
            Self::TranslucentApps => s.translucent_apps = on,
            Self::CursorBlink => s.terminal.cursor_blink = on,
            Self::ConfirmClose => s.confirm_close = on,
            Self::CheckUpdates => s.check_updates = on,
            _ => {}
        }
    }

    /// The current value as shown at the right of a finder row.
    #[must_use]
    pub fn value(self, s: &SettingsSnapshot) -> Option<String> {
        if let Some(on) = self.on(s) {
            return Some(on_off(on));
        }
        let t = &s.terminal;
        let automatic = |family: &str| if family.is_empty() { "Automatic".to_owned() } else { family.to_owned() };
        Some(match self {
            Self::PaneDensity => label_of(&DENSITIES, &s.density),
            Self::WindowMaterial => label_of(&MATERIALS, &s.material),
            Self::Theme => t.themes.iter().find(|c| c.id == t.theme).map_or_else(|| t.theme.clone(), |c| c.name.clone()),
            Self::InterfaceFont => automatic(&s.ui_font.family),
            Self::FontFamily => automatic(&t.font_family),
            Self::FontSize => format!("{} pt", t.font_size),
            Self::CursorShape => label_of(&CURSOR_SHAPES, &t.cursor_shape),
            Self::Scrollback => format!("{} lines", t.scrollback_lines),
            _ => return None,
        })
    }

    /// A choice's values, the current one selected.
    #[must_use]
    pub fn choices(self, s: &SettingsSnapshot) -> Vec<Choice> {
        let simple = |label: &str, event: SettingsEvent, selected: bool| Choice { label: label.into(), event, selected, swatch: None };
        let t = &s.terminal;
        match self {
            Self::PaneDensity => DENSITIES.iter().map(|(d, l)| simple(l, SettingsEvent::SetDensity(*d), *d == s.density)).collect(),
            Self::WindowMaterial => MATERIALS
                .iter()
                .map(|(m, l)| {
                    let label = match s.material_unavailable {
                        Some(_) if *m == Material::Frosted => format!("{l} (unavailable)"),
                        _ => (*l).to_owned(),
                    };
                    simple(&label, SettingsEvent::SetMaterial(*m), *m == s.material)
                })
                .collect(),
            Self::CursorShape => CURSOR_SHAPES.iter().map(|(c, l)| simple(l, SettingsEvent::SetCursorShape(*c), *c == t.cursor_shape)).collect(),
            Self::Theme => t
                .themes
                .iter()
                .map(|c| Choice {
                    label: c.name.clone(),
                    event: SettingsEvent::SetTheme(c.id.clone()),
                    selected: c.id == t.theme,
                    swatch: Some((c.background, c.foreground)),
                })
                .collect(),
            Self::InterfaceFont => fonts(&s.ui_font.choices, &s.ui_font.family, SettingsEvent::SetUiFontFamily),
            Self::FontFamily => fonts(&t.font_choices, &t.font_family, SettingsEvent::SetFontFamily),
            _ => Vec::new(),
        }
    }

    /// A number's current value, as typed.
    #[must_use]
    pub fn number(self, s: &SettingsSnapshot) -> Option<String> {
        match self {
            Self::FontSize => Some(s.terminal.font_size.to_string()),
            Self::Scrollback => Some(s.terminal.scrollback_lines.to_string()),
            _ => None,
        }
    }

    /// A typed number as the change it makes; the panel's rules.
    ///
    /// # Errors
    /// Not a number or out of range, said for the user.
    pub fn parse(self, text: &str) -> Result<SettingsEvent, String> {
        let text: String = text.chars().filter(|c| !matches!(c, ',' | '_' | ' ')).collect();
        match self {
            Self::FontSize => {
                let range = terminal_prefs::MIN_FONT_SIZE..=terminal_prefs::MAX_FONT_SIZE;
                match text.trim_end_matches("pt").parse::<f32>() {
                    Ok(size) if size.is_finite() && range.contains(&size) => Ok(SettingsEvent::SetFontSize(size)),
                    _ => Err(format!("Font size must be from {} to {} pt.", range.start(), range.end())),
                }
            }
            Self::Scrollback => {
                let range = silo_proto::model::TerminalConfig::MIN_SCROLLBACK..=silo_proto::model::TerminalConfig::MAX_SCROLLBACK;
                match text.parse::<u64>() {
                    Ok(lines) if range.contains(&lines) => Ok(SettingsEvent::SetScrollback(lines)),
                    _ => Err(format!("Scrollback must be a number of lines from {} to {}.", range.start(), range.end())),
                }
            }
            _ => Err("Not a number setting.".into()),
        }
    }
}

/// Automatic, then the installed families.
fn fonts(choices: &[String], preference: &str, event: fn(String) -> SettingsEvent) -> Vec<Choice> {
    let automatic = Choice { label: "Automatic".into(), event: event(String::new()), selected: preference.is_empty(), swatch: None };
    std::iter::once(automatic)
        .chain(choices.iter().map(|family| Choice {
            label: family.clone(),
            event: event(family.clone()),
            selected: !preference.is_empty() && crate::fonts::key(family) == crate::fonts::key(preference),
            swatch: None,
        }))
        .collect()
}

/// The search words of a panel section's settings.
#[must_use]
pub fn section_keywords(section: Section) -> String {
    SETTINGS.iter().filter(|s| s.section == section).flat_map(|s| [s.label, s.title, s.keywords]).collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_is_declared_once() {
        for (i, a) in SETTINGS.iter().enumerate() {
            assert!(SETTINGS[i + 1..].iter().all(|b| b.id != a.id), "{:?} twice", a.id);
            assert!(!a.keywords.is_empty() && !a.title.is_empty());
        }
        for slot in ColorSlot::ALL {
            assert_eq!(SettingId::Color(slot).label(), slot.label(), "the panel's color rows");
        }
    }

    #[test]
    fn kinds_have_what_the_finder_needs() {
        let s = SettingsSnapshot::default();
        for setting in SETTINGS {
            let id = setting.id;
            match setting.kind {
                Kind::Toggle => {
                    let on = id.on(&s).unwrap();
                    assert!(id.toggled(!on).is_some());
                    assert_eq!(id.value(&s).as_deref(), Some(if on { "On" } else { "Off" }));
                    let mut flipped = s.clone();
                    id.set_on(&mut flipped, !on);
                    assert_eq!(id.on(&flipped), Some(!on), "{id:?}");
                }
                Kind::Choice => {
                    let choices = id.choices(&s);
                    // Fonts list Automatic even when none are installed.
                    assert!(!choices.is_empty(), "{id:?}");
                    assert_eq!(choices.iter().filter(|c| c.selected).count(), 1, "{id:?}");
                    assert!(id.value(&s).is_some());
                }
                Kind::Number => assert!(id.parse(&id.number(&s).unwrap()).is_ok(), "{id:?}"),
                Kind::Open => assert!(id.on(&s).is_none()),
            }
        }
        assert_eq!(SettingId::Theme.choices(&s).len(), terminal_prefs::THEMES.len());
    }

    #[test]
    fn material_decides_which_rows_show() {
        let solid = SettingsSnapshot::default();
        let frosted = SettingsSnapshot { material: Material::Frosted, ..SettingsSnapshot::default() };
        let unavailable = SettingsSnapshot { material_unavailable: Some("Off"), ..frosted.clone() };
        assert!(SettingId::MatchRunningApp.visible(&solid) && !SettingId::TintWithApp.visible(&solid));
        assert!(!SettingId::MatchRunningApp.visible(&frosted) && SettingId::TranslucentApps.visible(&frosted));
        assert!(SettingId::MatchRunningApp.visible(&unavailable), "Frosted that cannot render shows the solid rows");
    }

    #[test]
    fn numbers_follow_the_panel_rules() {
        assert!(matches!(SettingId::FontSize.parse("14"), Ok(SettingsEvent::SetFontSize(s)) if (s - 14.0).abs() < f32::EPSILON));
        assert!(matches!(SettingId::FontSize.parse("12.5pt"), Ok(SettingsEvent::SetFontSize(_))));
        assert!(SettingId::FontSize.parse("7").is_err());
        assert!(SettingId::FontSize.parse("NaN").is_err());
        assert!(matches!(SettingId::Scrollback.parse("20,000"), Ok(SettingsEvent::SetScrollback(20_000))));
        assert!(SettingId::Scrollback.parse("5").unwrap_err().contains("lines from 100"));
        assert!(SettingId::Scrollback.parse("lots").is_err());
    }
}
