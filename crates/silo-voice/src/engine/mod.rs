//! The speech engines behind one trait. Every call runs on the engine
//! thread ([`crate::session`]): Whistle's engine is one process-global,
//! non-thread-safe model.

use crate::prefs::EngineKind;

#[cfg(target_os = "macos")]
mod whisper;
#[cfg(whistle)]
mod whistle;

/// What a transcription is asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// A language code, or `None` to detect it.
    pub language: Option<&'static str>,
    /// Words and names to favour (keyword biasing or a prompt).
    pub keywords: Vec<String>,
}

/// Live words so far: settled, then the unconfirmed tail.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Partial {
    pub committed: String,
    pub pending: String,
}

impl Partial {
    /// The words as one line.
    #[must_use]
    pub fn text(&self) -> String {
        [self.committed.trim(), self.pending.trim()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ")
    }
}

pub trait Engine {
    /// The whole clip (16 kHz mono) as text; empty for silence.
    ///
    /// # Errors
    /// The engine failed.
    fn transcribe(&mut self, pcm: &[f32], options: &Options) -> anyhow::Result<String>;

    /// Feed about a second more of a live clip; the words so far, for
    /// engines that show them.
    ///
    /// # Errors
    /// The engine failed.
    fn stream(&mut self, _pcm: &[f32], _options: &Options) -> anyhow::Result<Option<Partial>> {
        Ok(None)
    }

    /// The live clip is over: the next [`Engine::stream`] starts afresh.
    fn end_stream(&mut self) {}
}

/// Load `kind`'s model from `path`.
///
/// # Errors
/// The model can't be read or loaded, or this build has no such engine.
pub fn load(kind: EngineKind, path: &std::path::Path) -> anyhow::Result<Box<dyn Engine>> {
    match kind {
        #[cfg(whistle)]
        EngineKind::Fast => Ok(Box::new(whistle::Whistle::load(path)?)),
        #[cfg(target_os = "macos")]
        EngineKind::Multilingual => Ok(Box::new(whisper::Whisper::load(path)?)),
        #[allow(unreachable_patterns)]
        _ => anyhow::bail!("{} voice typing isn't available on this Mac", kind.label()),
    }
}

/// Whether this build has `kind`'s engine.
#[must_use]
pub const fn available(kind: EngineKind) -> bool {
    match kind {
        EngineKind::Fast => cfg!(whistle),
        EngineKind::Multilingual => cfg!(target_os = "macos"),
    }
}

/// Transcripts tidied for typing: trimmed, whitespace runs folded.
#[must_use]
pub fn tidy(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partials_and_transcripts_read_as_one_line() {
        let p = Partial { committed: "open the ".into(), pending: " settings file".into() };
        assert_eq!(p.text(), "open the settings file");
        assert_eq!(Partial::default().text(), "");
        assert_eq!(tidy("  hello \n world  "), "hello world");
    }
}
