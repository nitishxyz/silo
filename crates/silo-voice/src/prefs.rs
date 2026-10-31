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
    /// Whisper small: many languages, a mid-sized download.
    Balanced,
    /// Whisper large-v3 turbo: many languages, a larger download.
    Multilingual,
}

impl EngineKind {
    pub const ALL: [Self; 3] = [Self::Fast, Self::Balanced, Self::Multilingual];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Balanced => "Balanced",
            Self::Multilingual => "Multilingual",
        }
    }

    /// One line on what it is.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Fast => "Words appear as you speak; English, German, French, Spanish, Italian, Dutch, Polish",
            Self::Balanced => "About 100 languages; more accurate than Fast; text arrives when you stop",
            Self::Multilingual => "About 100 languages; the most accurate; text arrives when you stop",
        }
    }

    #[must_use]
    pub const fn model(self) -> &'static crate::models::Model {
        match self {
            Self::Fast => &crate::models::WHISTLE,
            Self::Balanced => &crate::models::WHISPER_SMALL,
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
        LANGUAGES.iter().filter(move |(code, _)| self != Self::Fast || FAST_LANGUAGES.contains(code))
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

/// Turn enumerations into lists, per target; terminals get one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatLists {
    pub agent: bool,
    pub editor: bool,
    pub browser: bool,
    pub field: bool,
}

impl Default for FormatLists {
    fn default() -> Self {
        Self { agent: true, editor: true, browser: false, field: true }
    }
}

impl FormatLists {
    /// The targets it can be set for.
    pub const TARGETS: [Target; 4] = [Target::Agent, Target::Editor, Target::Browser, Target::Field];

    #[must_use]
    pub const fn get(self, target: Target) -> bool {
        match target {
            Target::Terminal => false,
            Target::Agent => self.agent,
            Target::Editor => self.editor,
            Target::Browser => self.browser,
            Target::Field => self.field,
        }
    }

    pub fn set(&mut self, target: Target, on: bool) {
        match target {
            Target::Terminal => {}
            Target::Agent => self.agent = on,
            Target::Editor => self.editor = on,
            Target::Browser => self.browser = on,
            Target::Field => self.field = on,
        }
    }
}

/// A rule applied to every transcript: `heard` (whole words, any case)
/// is written as `write`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    pub heard: String,
    pub write: String,
}

/// Words every dictation favours, besides the user's and the focused
/// block's.
pub const BUILT_IN_VOCABULARY: [&str; 2] = ["Silo", "otto"];

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
    /// Words and names to favour, as the user spells them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vocabulary: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replacements: Vec<Replacement>,
    #[serde(default)]
    pub format_lists: FormatLists,
    /// Send the text (never audio) to the agent's model to tidy.
    #[serde(default)]
    pub polish: bool,
}

impl VoicePrefs {
    /// The words a dictation favours: the focused block's `hints`, the
    /// built-in ones, the replacements' and the user's, most telling last
    /// (deduplicated in any case, the later kept).
    #[must_use]
    pub fn keywords(&self, hints: Vec<String>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let all = hints.into_iter().chain(BUILT_IN_VOCABULARY.map(str::to_owned)).chain(self.replacements.iter().map(|r| r.write.clone()));
        for word in all.chain(self.vocabulary.iter().cloned()).collect::<Vec<_>>().into_iter().rev() {
            let word = word.trim();
            if !word.is_empty() && !out.iter().any(|w| w.eq_ignore_ascii_case(word)) {
                out.push(word.to_owned());
            }
        }
        out.reverse();
        out
    }
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
            vocabulary: Vec::new(),
            replacements: Vec::new(),
            format_lists: FormatLists::default(),
            polish: false,
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
        assert!(!old.polish && old.vocabulary.is_empty(), "text stays on this Mac by default");
        assert!(old.format_lists.get(Target::Agent) && !old.format_lists.get(Target::Terminal) && !old.format_lists.get(Target::Browser));
    }

    #[test]
    fn vocabulary_comes_last_and_persists() {
        let prefs = VoicePrefs {
            vocabulary: vec!["nextest".into(), " ".into()],
            replacements: vec![Replacement { heard: "auto".into(), write: "otto".into() }],
            polish: true,
            ..VoicePrefs::default()
        };
        assert_eq!(prefs.keywords(vec!["term".into()]), ["term", "Silo", "otto", "nextest"]);
        let text = serde_json::to_string(&prefs).unwrap();
        assert!(text.contains(r#""replacements":[{"heard":"auto","write":"otto"}]"#), "{text}");
        assert_eq!(serde_json::from_str::<VoicePrefs>(&text).unwrap(), prefs);
    }

    #[test]
    fn each_engine_offers_its_languages() {
        assert_eq!(EngineKind::Fast.languages().count(), 7);
        assert_eq!(EngineKind::Multilingual.languages().count(), LANGUAGES.len());
        assert_eq!(EngineKind::Balanced.languages().count(), LANGUAGES.len());
        assert_eq!(EngineKind::Balanced.language("ja"), Some("ja"));
        assert_eq!(EngineKind::Fast.language("de"), Some("de"));
        assert_eq!(EngineKind::Fast.language("ja"), None, "detected instead");
        assert_eq!(EngineKind::Multilingual.language("ja"), Some("ja"));
        assert_eq!(EngineKind::Fast.language(""), None);
        assert_eq!((language_name(""), language_name("nl")), ("Detect", "Dutch"));
    }
}
