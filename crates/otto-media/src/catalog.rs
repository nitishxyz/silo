//! What a provider's built-in models are good at, for `list_media_models` and `media.status`; `@` handles.
use otto_proto::MediaKind;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub kind: MediaKind,
    pub name: String,
    /// What it is good at, one line.
    pub about: String,
    /// `text`, `reference images`, `first/last frame`, `audio`.
    pub inputs: Vec<String>,
    /// Sizes, durations, sound.
    pub outputs: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    pub default: bool,
}

impl ModelEntry {
    #[must_use]
    pub(crate) fn default(mut self) -> Self {
        self.default = true;
        self
    }
}

pub(crate) fn model(kind: MediaKind, id: &str, name: &str, about: &str, inputs: &[&str], outputs: &str) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        kind,
        name: name.into(),
        about: about.into(),
        inputs: inputs.iter().map(|&i| i.to_owned()).collect(),
        outputs: outputs.into(),
        price: None,
        default: false,
    }
}

/// `name` slugged: lowercase `[a-z0-9.-]` (`GPT Image 2.6 Flare` → `gpt-image-2.6-flare`).
#[must_use]
pub fn slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        match c {
            'a'..='z' | '0'..='9' | '.' => slug.push(c),
            _ if !slug.is_empty() && !slug.ends_with('-') => slug.push('-'),
            _ => {}
        }
    }
    slug.trim_matches(['-', '.']).to_owned()
}

/// Each `(provider id, model name)`'s `@` handle (docs/MEDIA.md §3.0.1), in order: the name slugged; a slug taken by
/// an earlier entry gets the provider in front, then a number until it is unique.
#[must_use]
pub fn handles(models: &[(&str, &str)]) -> Vec<String> {
    let mut taken = std::collections::HashSet::new();
    models
        .iter()
        .map(|&(provider, name)| {
            let slug = slug(name);
            let mut handle = if taken.contains(&slug) { format!("{provider}-{slug}") } else { slug };
            let base = handle.clone();
            for n in 2.. {
                if !taken.contains(&handle) {
                    break;
                }
                handle = format!("{base}-{n}");
            }
            taken.insert(handle.clone());
            handle
        })
        .collect()
}

/// A speech voice (`generate_audio`'s `voice`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Voice {
    pub id: String,
    pub name: String,
    /// One line: accent, tone, use.
    pub about: String,
    pub default: bool,
}

impl Voice {
    pub(crate) fn new(id: &str, name: &str, about: &str) -> Self {
        Self { id: id.into(), name: name.into(), about: about.into(), default: false }
    }
    /// `- George (JBFqnCBsd6RMkjVDRZzb, default): warm British storyteller`
    #[must_use]
    pub fn line(&self) -> String {
        let mut line = format!("- {} ({}{})", self.name, self.id, if self.default { ", default" } else { "" });
        if !self.about.is_empty() {
            line.push_str(&format!(": {}", self.about));
        }
        line
    }
}

/// Text and reference images in.
pub(crate) const EDITS: &[&str] = &["text", "reference images"];
pub(crate) const TEXT: &[&str] = &["text"];
