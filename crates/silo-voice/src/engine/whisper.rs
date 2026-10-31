//! Whisper (whisper.cpp through whisper-rs) on Metal.

use std::path::Path;

use anyhow::{Context as _, anyhow};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::{Engine, Options};

pub struct Whisper {
    context: WhisperContext,
}

impl Whisper {
    pub fn load(path: &Path, kind: crate::EngineKind) -> anyhow::Result<Self> {
        static HOOKS: std::sync::Once = std::sync::Once::new();
        // whisper.cpp's own logging into tracing, not stderr.
        HOOKS.call_once(whisper_rs::install_logging_hooks);
        let path = path.to_str().ok_or_else(|| anyhow!("model path is not UTF-8"))?;
        let context =
            WhisperContext::new_with_params(path, WhisperContextParameters::default()).with_context(|| format!("load the {} model", kind.label()))?;
        Ok(Self { context })
    }
}

/// The prompt's length at most: whisper.cpp keeps a prompt's last 223
/// tokens (half its text context), about 4 characters each.
const PROMPT_CHARS: usize = 600;

/// Keywords as the text Whisper continues from (its `initial_prompt`): it
/// favours their spelling and the prompt's style (punctuated, cased). Too
/// long, the first ones go: the most telling come last.
fn prompt(keywords: &[String]) -> String {
    let mut words: Vec<&str> = Vec::new();
    let mut chars = 0;
    for word in keywords.iter().rev().map(|k| k.trim()).filter(|k| !k.is_empty() && !k.contains('\0')) {
        if words.iter().any(|w| w.eq_ignore_ascii_case(word)) {
            continue;
        }
        chars += word.chars().count() + 2;
        if chars > PROMPT_CHARS {
            break;
        }
        words.push(word);
    }
    if words.is_empty() {
        return String::new();
    }
    words.reverse();
    format!("{}.", words.join(", "))
}

impl Engine for Whisper {
    fn transcribe(&mut self, pcm: &[f32], options: &Options) -> anyhow::Result<String> {
        let mut state = self.context.create_state().context("whisper state")?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(options.language.unwrap_or("auto")));
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
        params.set_n_threads(i32::try_from(threads).unwrap_or(4));
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_no_timestamps(true);
        params.set_suppress_nst(true);
        let prompt = prompt(&options.keywords);
        if !prompt.is_empty() {
            params.set_initial_prompt(&prompt);
        }
        state.full(params, pcm).context("whisper")?;
        let mut text = String::new();
        for segment in state.as_iter() {
            // Whisper's guess at silence is a segment it marks as such.
            if segment.no_speech_probability() > 0.8 {
                continue;
            }
            text.push_str(&segment.to_str_lossy().unwrap_or_default());
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_keeps_the_last_words_and_no_nul() {
        let words = |w: &[&str]| w.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(prompt(&words(&["term", " ", "Silo", "otto", "silo", "nul\0"])), "term, otto, silo.");
        assert_eq!(prompt(&[]), "");
        let many: Vec<String> = (0..200).map(|i| format!("word{i}")).collect();
        let p = prompt(&many);
        assert!(p.chars().count() <= PROMPT_CHARS + 1 && p.ends_with("word199."), "{p}");
    }
}
