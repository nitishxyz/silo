//! One dictation: the microphone thread records and analyses (levels for
//! the pill, live words every second for engines that show them), then the
//! engine thread transcribes the whole clip. The owner polls
//! [`Session::snapshot`] each frame.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    time::Duration,
};

use crate::{
    bands::{Analyzer, BANDS},
    engine::{self, Engine, Options},
    prefs::EngineKind,
    resample::{RATE, Resampler},
};

/// Recording stops by itself after this long (a forgotten hands-free one).
pub const MAX_SECONDS: u32 = 300;
/// Live words are asked for every this many new samples (one second).
const LIVE_EVERY: usize = RATE as usize;
/// Shorter clips are nothing said.
const MIN_SAMPLES: usize = RATE as usize / 4;

const RUN: u8 = 0;
const STOP: u8 = 1;
const CANCEL: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Recording,
    /// Recorded; the model is still downloading.
    WaitingForModel,
    Transcribing,
    /// The text to insert; empty when nothing was heard.
    Done(String),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub phase: Phase,
    pub levels: [f32; BANDS],
    /// Live words so far (Fast engine).
    pub partial: String,
    pub seconds: f32,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub engine: EngineKind,
    /// The engine's model file; may still be downloading.
    pub model: PathBuf,
    pub model_bytes: u64,
    pub options: Options,
    /// An input device's name; empty for the system's.
    pub microphone: String,
}

pub struct Session {
    shared: Arc<Mutex<Snapshot>>,
    control: Arc<AtomicU8>,
}

fn set(shared: &Mutex<Snapshot>, f: impl FnOnce(&mut Snapshot)) {
    if let Ok(mut s) = shared.lock() {
        f(&mut s);
    }
}

fn model_ready(config: &Config) -> bool {
    std::fs::metadata(&config.model).is_ok_and(|m| m.len() == config.model_bytes)
}

impl Session {
    /// Start recording.
    #[must_use]
    pub fn start(config: Config) -> Self {
        let shared = Arc::new(Mutex::new(Snapshot { phase: Phase::Recording, levels: [0.0; BANDS], partial: String::new(), seconds: 0.0 }));
        let control = Arc::new(AtomicU8::new(RUN));
        let (s, c) = (shared.clone(), control.clone());
        let spawned = std::thread::Builder::new().name("silo-voice-mic".into()).spawn(move || record(&config, &s, &c));
        if let Err(error) = spawned {
            tracing::warn!(%error, "voice: no microphone thread");
            set(&shared, |s| s.phase = Phase::Failed("Couldn't start recording".into()));
        }
        Self { shared, control }
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.shared.lock().map(|s| s.clone()).unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Stop recording and transcribe.
    pub fn stop(&self) {
        let _ = self.control.compare_exchange(RUN, STOP, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Stop and drop it.
    pub fn cancel(&self) {
        self.control.store(CANCEL, Ordering::Release);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(target_os = "macos")]
fn record(config: &Config, shared: &Arc<Mutex<Snapshot>>, control: &Arc<AtomicU8>) {
    let (tx, rx) = mpsc::channel();
    let input = match crate::capture::open(&config.microphone, tx) {
        Ok(input) => input,
        Err(error) => {
            tracing::warn!("voice: {error:#}");
            set(shared, |s| s.phase = Phase::Failed("Couldn't use the microphone".into()));
            return;
        }
    };
    let mut resampler = Resampler::new(input.rate, RATE);
    let mut analyzer = Analyzer::default();
    let mut audio: Vec<f32> = Vec::with_capacity(RATE as usize * 30);
    let mut streamed = 0;
    let max = (RATE * MAX_SECONDS) as usize;
    let mut take = |chunk: Vec<f32>, audio: &mut Vec<f32>, analyzer: &mut Analyzer| {
        let start = audio.len();
        resampler.push(&chunk, audio);
        analyzer.push(&audio[start..]);
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(15)) {
            Ok(chunk) => take(chunk, &mut audio, &mut analyzer),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        #[allow(clippy::cast_precision_loss)]
        let seconds = audio.len() as f32 / RATE as f32;
        let levels = analyzer.levels();
        set(shared, |s| (s.levels, s.seconds) = (levels, seconds));
        if config.engine.live() && audio.len() - streamed >= LIVE_EVERY && model_ready(config) {
            submit(Job { work: Work::Stream(audio[streamed..].to_vec()), config: config.clone(), shared: shared.clone(), control: control.clone() });
            streamed = audio.len();
        }
        if control.load(Ordering::Acquire) != RUN || audio.len() >= max {
            break;
        }
    }
    drop(input);
    while let Ok(chunk) = rx.try_recv() {
        take(chunk, &mut audio, &mut analyzer);
    }
    set(shared, |s| s.levels = [0.0; BANDS]);
    let cancelled = control.load(Ordering::Acquire) == CANCEL;
    if !cancelled && audio.len() < MIN_SAMPLES {
        set(shared, |s| s.phase = Phase::Done(String::new()));
        return;
    }
    let work = if cancelled { Work::End } else { Work::Finish(audio) };
    set(shared, |s| s.phase = if model_ready(config) { Phase::Transcribing } else { Phase::WaitingForModel });
    submit(Job { work, config: config.clone(), shared: shared.clone(), control: control.clone() });
}

#[cfg(not(target_os = "macos"))]
fn record(_config: &Config, shared: &Arc<Mutex<Snapshot>>, _control: &Arc<AtomicU8>) {
    set(shared, |s| s.phase = Phase::Failed("Voice typing needs a Mac".into()));
}

enum Work {
    /// About a second more of the live clip.
    Stream(Vec<f32>),
    /// The whole clip, to transcribe.
    Finish(Vec<f32>),
    /// Cancelled: end the live clip.
    End,
}

struct Job {
    work: Work,
    config: Config,
    shared: Arc<Mutex<Snapshot>>,
    control: Arc<AtomicU8>,
}

/// Queue `job` on the engine thread, started on first use; it keeps the
/// loaded engines for the app's life.
fn submit(job: Job) {
    static TX: OnceLock<Option<Mutex<mpsc::Sender<Job>>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("silo-voice-engine".into())
            .spawn(move || {
                let mut engines: HashMap<EngineKind, Box<dyn Engine>> = HashMap::new();
                while let Ok(job) = rx.recv() {
                    run(job, &mut engines);
                }
            })
            .map_err(|e| tracing::warn!(%e, "voice: no engine thread"))
            .ok()
            .map(|_| Mutex::new(tx))
    });
    match tx.as_ref().and_then(|tx| tx.lock().ok()) {
        Some(tx) => drop(tx.send(job)),
        None => set(&job.shared, |s| s.phase = Phase::Failed("Couldn't start transcribing".into())),
    }
}

fn engine<'a>(engines: &'a mut HashMap<EngineKind, Box<dyn Engine>>, config: &Config) -> Result<&'a mut Box<dyn Engine>, String> {
    match engines.entry(config.engine) {
        std::collections::hash_map::Entry::Occupied(e) => Ok(e.into_mut()),
        std::collections::hash_map::Entry::Vacant(slot) => {
            let started = std::time::Instant::now();
            let loaded = engine::load(config.engine, &config.model).map_err(|e| {
                tracing::warn!("voice: {e:#}");
                "Couldn't load the voice model".to_owned()
            })?;
            tracing::info!(engine = config.engine.label(), ms = started.elapsed().as_millis(), "voice: engine loaded");
            Ok(slot.insert(loaded))
        }
    }
}

fn run(job: Job, engines: &mut HashMap<EngineKind, Box<dyn Engine>>) {
    let Job { work, config, shared, control } = job;
    let cancelled = || control.load(Ordering::Acquire) == CANCEL;
    match work {
        Work::Stream(pcm) => {
            if cancelled() {
                return;
            }
            let partial = engine(engines, &config).and_then(|e| e.stream(&pcm, &config.options).map_err(|e| e.to_string()));
            match partial {
                Ok(Some(partial)) => set(&shared, |s| s.partial = partial.text()),
                Ok(None) => {}
                Err(error) => tracing::debug!("voice: live words: {error}"),
            }
        }
        Work::End => {
            if let Some(e) = engines.get_mut(&config.engine) {
                e.end_stream();
            }
        }
        Work::Finish(pcm) => {
            while !model_ready(&config) {
                if cancelled() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            set(&shared, |s| s.phase = Phase::Transcribing);
            let started = std::time::Instant::now();
            let text = engine(engines, &config).and_then(|e| {
                e.end_stream();
                e.transcribe(&pcm, &config.options).map_err(|error| {
                    tracing::warn!("voice: transcribe: {error:#}");
                    "Couldn't transcribe".to_owned()
                })
            });
            #[allow(clippy::cast_precision_loss)]
            let seconds = pcm.len() as f32 / RATE as f32;
            tracing::info!(engine = config.engine.label(), seconds, ms = started.elapsed().as_millis(), "voice: transcribed");
            if !cancelled() {
                set(&shared, |s| s.phase = text.map_or_else(Phase::Failed, |t| Phase::Done(engine::tidy(&t))));
            }
        }
    }
}
