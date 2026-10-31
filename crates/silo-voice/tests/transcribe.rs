//! Speech to text end to end, with real models: `say` speaks a sentence,
//! each engine must hear it. Ignored by default (downloads models): run
//! with `--run-ignored only`; `SILO_VOICE_HOME` reuses a folder holding
//! `voice/<model>` instead of a fresh download.
#![cfg(target_os = "macos")]

use std::{path::PathBuf, process::Command};

use silo_voice::{
    EngineKind,
    engine::{self, Options},
};

const SENTENCE: &str = "Open the settings file and run cargo test";

/// 16 kHz mono samples of `SENTENCE` from macOS's speech synthesizer.
fn speech(dir: &std::path::Path) -> Vec<f32> {
    let (aiff, wav) = (dir.join("say.aiff"), dir.join("say.wav"));
    assert!(Command::new("say").arg("-o").arg(&aiff).arg(SENTENCE).status().unwrap().success());
    assert!(Command::new("afconvert").args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"]).arg(&aiff).arg(&wav).status().unwrap().success());
    let bytes = std::fs::read(&wav).unwrap();
    // The data chunk: after its "data" tag and length.
    let at = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
    bytes[at..].as_chunks::<2>().0.iter().map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.0).collect()
}

fn home() -> PathBuf {
    std::env::var_os("SILO_VOICE_HOME").map_or_else(|| std::env::temp_dir().join("silo-voice-it"), PathBuf::from)
}

fn hears(kind: EngineKind) {
    if !engine::available(kind) {
        eprintln!("{kind:?} is not in this build");
        return;
    }
    let home = home();
    let model = kind.model();
    if !model.installed(&home) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(model.download(&home, |_, _| {})).unwrap();
    }
    let pcm = speech(&std::env::temp_dir());
    let mut engine = engine::load(kind, &model.path(&home)).unwrap();
    let options = Options { language: Some("en"), keywords: vec!["cargo".into()] };
    let text = engine::tidy(&engine.transcribe(&pcm, &options).unwrap()).to_lowercase();
    eprintln!("{kind:?}: {text}");
    for word in ["settings", "cargo", "test"] {
        assert!(text.contains(word), "{kind:?} missed {word:?}: {text}");
    }
    // Live words (Fast): about a second at a time.
    if kind.live() {
        let mut last = String::new();
        for chunk in pcm.chunks(16_000) {
            if let Some(partial) = engine.stream(chunk, &options).unwrap() {
                last = partial.text();
            }
        }
        engine.end_stream();
        eprintln!("{kind:?} live: {last}");
        assert!(last.to_lowercase().contains("settings"), "{last}");
    }
}

#[test]
#[ignore = "downloads speech models"]
fn fast_hears_speech() {
    hears(EngineKind::Fast);
}

#[test]
#[ignore = "downloads a 574 MB model"]
fn multilingual_hears_speech() {
    hears(EngineKind::Multilingual);
}

#[test]
#[ignore = "downloads a 190 MB model"]
fn balanced_hears_speech() {
    hears(EngineKind::Balanced);
}
