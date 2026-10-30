//! Settings › Voice, saved with the app's other settings.

use serde::{Deserialize, Serialize};

fn yes() -> bool {
    true
}

/// Which speech engine transcribes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    /// Whistle: small, live words while you speak, seven languages.
    #[default]
    Fast,
    /// Whisper large-v3 turbo: many languages, a larger download.
    Multilingual,
}

impl EngineKind {
    pub const ALL: [Self; 2] = [Self::Fast, Self::Multilingual];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Multilingual => "Multilingual",
        }
    }

    /// One line on what it is.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Fast => "Words appear as you speak; English, German, French, Spanish, Italian, Dutch, Polish",
            Self::Multilingual => "About 100 languages; text arrives when you stop",
        }
    }

    #[must_use]
    pub const fn model(self) -> &'static crate::models::Model {
        match self {
            Self::Fast => &crate::models::WHISTLE,
            Self::Multilingual => &crate::models::WHISPER_TURBO,
        }
    }

    /// Whether it shows words while recording.
    #[must_use]
    pub const fn live(self) -> bool {
        matches!(self, Self::Fast)
    }

    /// The languages it can be set to (codes in [`LANGUAGES`]).
    pub fn languages(self) -> impl Iterator<Item = &'static (&'static str, &'static str)> {
        LANGUAGES.iter().filter(move |(code, _)| self == Self::Multilingual || FAST_LANGUAGES.contains(code))
    }

    /// `language` if this engine has it, else `None` (detect).
    #[must_use]
    pub fn language(self, language: &str) -> Option<&'static str> {
        self.languages().find(|(code, _)| *code == language).map(|(code, _)| *code)
    }
}

const FAST_LANGUAGES: [&str; 7] = ["en", "de", "fr", "es", "it", "nl", "pl"];

/// Fixed languages offered besides detecting it: (code, name).
pub const LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "German"),
    ("fr", "French"),
    ("es", "Spanish"),
    ("it", "Italian"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("sv", "Swedish"),
    ("tr", "Turkish"),
    ("ru", "Russian"),
    ("uk", "Ukrainian"),
    ("hi", "Hindi"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("zh", "Chinese"),
];

/// A language's name; "" is detecting it.
#[must_use]
pub fn language_name(code: &str) -> &'static str {
    LANGUAGES.iter().find(|(c, _)| *c == code).map_or("Detect", |(_, name)| name)
}

/// Where text goes, for the per-kind "press Return after" switches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Terminal,
    Agent,
    Editor,
    Browser,
    /// Any other text field.
    Field,
}

impl Target {
    pub const ALL: [Self; 5] = [Self::Terminal, Self::Agent, Self::Editor, Self::Browser, Self::Field];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminals",
            Self::Agent => "Agents",
            Self::Editor => "Files editor",
            Self::Browser => "Web pages",
            Self::Field => "Other text fields",
        }
    }
}

/// Press Return after inserting, per target; off everywhere by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnterAfter {
    pub terminal: bool,
    pub agent: bool,
    pub editor: bool,
    pub browser: bool,
    pub field: bool,
}

impl EnterAfter {
    #[must_use]
    pub fn get(self, target: Target) -> bool {
        match target {
            Target::Terminal => self.terminal,
            Target::Agent => self.agent,
            Target::Editor => self.editor,
            Target::Browser => self.browser,
            Target::Field => self.field,
        }
    }

    pub fn set(&mut self, target: Target, on: bool) {
        *match target {
            Target::Terminal => &mut self.terminal,
            Target::Agent => &mut self.agent,
            Target::Editor => &mut self.editor,
            Target::Browser => &mut self.browser,
            Target::Field => &mut self.field,
        } = on;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoicePrefs {
    /// Hold or double-press Fn to dictate.
    #[serde(default = "yes")]
    pub on: bool,
    #[serde(default)]
    pub engine: EngineKind,
    /// A code from [`LANGUAGES`]; empty detects it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language: String,
    /// An input device's name; empty follows the system's.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub microphone: String,
    #[serde(default)]
    pub enter_after: EnterAfter,
    /// Start and stop cues (when interface sounds are on).
    #[serde(default = "yes")]
    pub sounds: bool,
}

impl Default for VoicePrefs {
    fn default() -> Self {
        Self {
            on: true,
            engine: EngineKind::Fast,
            language: String::new(),
            microphone: String::new(),
            enter_after: EnterAfter::default(),
            sounds: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_round_trip() {
        let old: VoicePrefs = serde_json::from_str("{}").unwrap();
        assert_eq!(old, VoicePrefs::default());
        assert!(old.on && old.sounds && old.engine == EngineKind::Fast);
        assert!(Target::ALL.iter().all(|t| !old.enter_after.get(*t)), "never Return by default");
        let mut prefs = VoicePrefs { engine: EngineKind::Multilingual, language: "ja".into(), ..VoicePrefs::default() };
        prefs.enter_after.set(Target::Agent, true);
        let text = serde_json::to_string(&prefs).unwrap();
        assert!(text.contains(r#""engine":"multilingual""#) && !text.contains("microphone"), "{text}");
        assert_eq!(serde_json::from_str::<VoicePrefs>(&text).unwrap(), prefs);
    }

    #[test]
    fn each_engine_offers_its_languages() {
        assert_eq!(EngineKind::Fast.languages().count(), 7);
        assert_eq!(EngineKind::Multilingual.languages().count(), LANGUAGES.len());
        assert_eq!(EngineKind::Fast.language("de"), Some("de"));
        assert_eq!(EngineKind::Fast.language("ja"), None, "detected instead");
        assert_eq!(EngineKind::Multilingual.language("ja"), Some("ja"));
        assert_eq!(EngineKind::Fast.language(""), None);
        assert_eq!((language_name(""), language_name("nl")), ("Detect", "Dutch"));
    }
}
