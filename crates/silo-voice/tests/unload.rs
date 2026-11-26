//! A process that used whisper.cpp exits cleanly after
//! `session::unload`. Without it, whisper.cpp's Metal device (a C++ static)
//! is torn down by `exit()` under the loaded model and aborts the process,
//! which is how the app crashed when it quit. A file of its own: the check
//! is the test binary's own exit. Ignored by default (a 190 MB model): run
//! with `--ignored`; `SILO_VOICE_HOME` reuses a folder holding
//! `voice/<model>`.
#![cfg(target_os = "macos")]

use std::{path::PathBuf, time::Duration};

use silo_voice::{
    EngineKind,
    engine::Options,
    session::{self, Config, Phase, Session},
};

#[test]
#[ignore = "downloads a speech model"]
fn the_process_exits_cleanly_after_unload() {
    let kind = EngineKind::Multilingual;
    if !silo_voice::engine::available(kind) {
        return;
    }
    let home = std::env::var_os("SILO_VOICE_HOME").map_or_else(|| std::env::temp_dir().join("silo-voice-it"), PathBuf::from);
    let model = kind.model();
    if !model.installed(&home) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(model.download(&home, |_, _| {})).unwrap();
    }
    let config = Config {
        engine: kind,
        model: model.path(&home),
        model_bytes: model.bytes,
        options: Options { language: Some("en"), keywords: Vec::new() },
        microphone: String::new(),
    };
    // A second of silence loads the model on the engine thread.
    let clip = Session::transcribe(config, vec![0.0; 16_000]);
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        match clip.snapshot().phase {
            Phase::Done(_) => break,
            Phase::Failed(error) => panic!("{error}"),
            _ => {}
        }
        assert!(std::time::Instant::now() < deadline, "never transcribed");
        std::thread::sleep(Duration::from_millis(50));
    }
    if std::env::var_os("SILO_VOICE_NO_UNLOAD").is_none() {
        session::unload(Duration::from_secs(5));
    }
}
