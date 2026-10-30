//! Whisper (whisper.cpp through whisper-rs) on Metal.

use std::path::Path;

use anyhow::{Context as _, anyhow};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::{Engine, Options};

pub struct Whisper {
    context: WhisperContext,
}

impl Whisper {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        static HOOKS: std::sync::Once = std::sync::Once::new();
        // whisper.cpp's own logging into tracing, not stderr.
        HOOKS.call_once(whisper_rs::install_logging_hooks);
        let path = path.to_str().ok_or_else(|| anyhow!("model path is not UTF-8"))?;
        let context = WhisperContext::new_with_params(path, WhisperContextParameters::default()).context("load the Multilingual model")?;
        Ok(Self { context })
    }
}

/// Keywords as the prompt Whisper continues from: it favours their spelling.
fn prompt(keywords: &[String]) -> String {
    keywords.iter().map(|k| k.trim()).filter(|k| !k.is_empty()).collect::<Vec<_>>().join(", ")
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
