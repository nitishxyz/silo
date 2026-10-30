//! Interface sounds: soft, short cues for what the user does (switching
//! tabs, opening and closing panes, flipping a switch, dropping a tab) and
//! for what a program reports (done, needs you, failed).
//!
//! Every sound is synthesized here, not loaded, and is one or two "dups":
//! a low sine (165–330 Hz) whose pitch falls a little into place, there
//! at once (a 3 ms rise, too short to hear yet long enough not to click)
//! and gone within ~0.1 s. One quiet overtone, a low-pass and a faint
//! echo keep it round; nothing is bright or ringing, and loudness stays
//! well under full scale.
//!
//! Playback is one output stream on a dedicated thread that mixes voices.
//! It is opened by the first sound and released after a quiet spell, so an
//! idle Silo holds no audio device. Settings › Appearance › Sounds turns it
//! all off ([`set_enabled`]); it is on by default.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// One interface sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sound {
    /// Another tab or session: one dup.
    Select,
    /// A palette, switcher or other overlay appears: one dup, a step higher.
    Pop,
    /// A new tab, pane or session: dup-dup, stepping up.
    Open,
    /// A tab, pane or session closes: the same, stepping down.
    Close,
    /// A folder opens in a tree: one small dup, a little higher.
    Expand,
    /// A folder closes: one small dup, a little lower.
    Collapse,
    /// A switch turned on: dup-dup up a fourth.
    ToggleOn,
    /// A switch turned off: dup-dup down.
    ToggleOff,
    /// A tab dropped in a new place: one lower, rounder dup.
    Drop,
    /// A tab dropped into another: that dup, then one higher.
    Merge,
    /// A program finished: dup-dup up a third, the second a touch longer.
    Success,
    /// A program needs you: dup-dup on one note.
    Attention,
    /// Something failed: dup-dup falling a fourth, low.
    Error,
    /// Voice typing listens: one soft dup rising into a held, higher one.
    ListenStart,
    /// Voice typing stops listening: the same, falling.
    ListenStop,
}

impl Sound {
    pub const ALL: [Self; 15] = [
        Self::Select,
        Self::Pop,
        Self::Open,
        Self::Close,
        Self::Expand,
        Self::Collapse,
        Self::ToggleOn,
        Self::ToggleOff,
        Self::Drop,
        Self::Merge,
        Self::Success,
        Self::Attention,
        Self::Error,
        Self::ListenStart,
        Self::ListenStop,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).expect("listed")
    }

    /// A switch's sound for its new state.
    #[must_use]
    pub fn toggle(on: bool) -> Self {
        if on { Self::ToggleOn } else { Self::ToggleOff }
    }
}

static ENABLED: AtomicBool = AtomicBool::new(true);

/// Settings › Sounds. Turned on, the audio output is readied in the
/// background so the first sound is not late.
pub fn set_enabled(on: bool) {
    let was = ENABLED.swap(on, Ordering::Relaxed);
    if on && (!was || !WARMED.swap(true, Ordering::Relaxed)) {
        engine::warm();
    }
}

static WARMED: AtomicBool = AtomicBool::new(false);

#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Play `sound` when sounds are on.
pub fn play(sound: Sound) {
    if enabled() {
        play_now(sound);
    }
}

/// The same sound again within this is dropped: a key held down or a
/// burst of events makes one sound, not a buzz.
const REPEAT: Duration = Duration::from_millis(45);

fn play_now(sound: Sound) {
    static LAST: std::sync::Mutex<[Option<Instant>; Sound::ALL.len()]> = std::sync::Mutex::new([None; Sound::ALL.len()]);
    let now = Instant::now();
    {
        let Ok(mut last) = LAST.lock() else { return };
        let slot = &mut last[sound.index()];
        if slot.is_some_and(|t| now.duration_since(t) < REPEAT) {
            return;
        }
        *slot = Some(now);
    }
    engine::play(sound);
}

/// The sound of a program's `block.attention`: heard while Silo is
/// frontmost, or with the notification that goes out for it elsewhere
/// (Silo's notifications carry no sound of their own). A program's own
/// notification sounds only with its banner.
#[must_use]
pub fn for_attention(reason: silo_proto::control::AttentionReason, frontmost: bool, notified: bool) -> Option<Sound> {
    use silo_proto::control::AttentionReason as R;
    if !(frontmost || notified) {
        return None;
    }
    match reason {
        R::Done => Some(Sound::Success),
        R::Blocked => Some(Sound::Attention),
        R::Error => Some(Sound::Error),
        R::Notification if notified => Some(Sound::Attention),
        R::Notification | R::Unknown => None,
    }
}

// ---- synthesis ----

/// Note frequencies (Hz), equal temperament: a low register, where a
/// short note is a muted "dup" rather than a ping.
mod pitch {
    pub const E3: f32 = 164.81;
    pub const G3: f32 = 196.00;
    pub const A3: f32 = 220.00;
    pub const B3: f32 = 246.94;
    pub const C4: f32 = 261.63;
    pub const D4: f32 = 293.66;
    pub const E4: f32 = 329.63;
}

/// The level every sound is scaled by: quiet next to system sounds.
const MASTER: f32 = 0.5;
/// Nothing above this reaches the speaker (a soft knee, not a clip).
const CEILING: f32 = 0.35;

/// One note.
#[derive(Clone, Copy)]
struct Note {
    freq: f32,
    /// Start, seconds after the sound starts.
    at: f32,
    gain: f32,
    /// Rise to full level (raised cosine): long enough never to click.
    attack: f32,
    /// Exponential decay time constant, seconds.
    decay: f32,
    /// The pitch starts at `freq * scoop` and settles on `freq` within
    /// ~30 ms: a rounded "bloop" instead of a struck edge. 1.0 = none.
    scoop: f32,
}

const fn note(freq: f32, at: f32, gain: f32, decay: f32) -> Note {
    Note { freq, at, gain, attack: 0.003, decay, scoop: 1.0 }
}

impl Note {
    const fn scoop(mut self, scoop: f32) -> Self {
        self.scoop = scoop;
        self
    }

    /// Seconds until it is inaudible (-60 dB).
    fn end(&self) -> f32 {
        self.at + self.attack + self.decay * 6.9
    }
}

fn notes(sound: Sound) -> Vec<Note> {
    use pitch::*;
    // Every note is a "dup": low, gone in a few tens of milliseconds, its
    // pitch falling a little into place like a soft knock on wood.
    let dup = |freq, at, gain| note(freq, at, gain, 0.028).scoop(1.3);
    match sound {
        Sound::Select => vec![dup(C4, 0.0, 0.2)],
        Sound::Pop => vec![dup(D4, 0.0, 0.18)],
        Sound::Open => vec![dup(A3, 0.0, 0.2), dup(D4, 0.075, 0.18)],
        Sound::Close => vec![dup(D4, 0.0, 0.17), dup(A3, 0.075, 0.19)],
        Sound::Expand => vec![dup(D4, 0.0, 0.13)],
        Sound::Collapse => vec![dup(B3, 0.0, 0.13)],
        Sound::ToggleOn => vec![dup(B3, 0.0, 0.18), dup(E4, 0.065, 0.16)],
        Sound::ToggleOff => vec![dup(E4, 0.0, 0.15), dup(B3, 0.065, 0.18)],
        Sound::Drop => vec![note(G3, 0.0, 0.24, 0.035).scoop(1.4)],
        Sound::Merge => vec![note(G3, 0.0, 0.24, 0.035).scoop(1.4), dup(C4, 0.085, 0.18)],
        Sound::Success => vec![dup(C4, 0.0, 0.18), note(E4, 0.09, 0.16, 0.06).scoop(1.3)],
        Sound::Attention => vec![dup(D4, 0.0, 0.18), dup(D4, 0.12, 0.18)],
        Sound::Error => vec![dup(A3, 0.0, 0.22), note(E3, 0.09, 0.24, 0.05).scoop(1.3)],
        Sound::ListenStart => vec![note(G3, 0.0, 0.14, 0.03).scoop(0.85), note(D4, 0.06, 0.13, 0.05).scoop(0.9)],
        Sound::ListenStop => vec![note(D4, 0.0, 0.12, 0.03).scoop(1.15), note(G3, 0.06, 0.15, 0.05).scoop(1.2)],
    }
}

/// Overtones: (ratio, level, decay factor). The octave is quiet and gone
/// first; it only lets small laptop speakers carry such a low note.
const PARTIALS: [(f32, f32, f32); 2] = [(1.0, 1.0, 1.0), (2.0, 0.18, 0.4)];
/// Above this, the low-pass rolls everything off.
const CUTOFF: f32 = 1600.0;
/// The room: (delay s, level). Dark (it feeds from the low-passed signal).
const ROOM: [(f32, f32); 1] = [(0.023, 0.08)];
/// Every sound ends on a short fade, so nothing stops with a click.
const TAIL_FADE: f32 = 0.006;

/// `sound` as mono samples at `rate` Hz.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn render(sound: Sound, rate: u32) -> Vec<f32> {
    let sr = rate as f32;
    let notes = notes(sound);
    let room = ROOM.iter().map(|(d, _)| *d).fold(0.0, f32::max);
    let length = notes.iter().map(Note::end).fold(0.0, f32::max) + room;
    let len = (length * sr).ceil() as usize;
    let mut dry = vec![0.0f32; len];

    for n in &notes {
        let start = (n.at * sr) as usize;
        let samples = ((n.end() - n.at) * sr).ceil() as usize;
        let mut phase = [0.0f32; PARTIALS.len()];
        for i in 0..samples.min(len.saturating_sub(start)) {
            let t = i as f32 / sr;
            // Pitch scoop settles exponentially (time constant 10 ms).
            let freq = n.freq * (1.0 + (n.scoop - 1.0) * (-t / 0.010).exp());
            let rise = if t < n.attack { 0.5 - 0.5 * (std::f32::consts::PI * t / n.attack).cos() } else { 1.0 };
            let after = (t - n.attack).max(0.0);
            let mut v = 0.0;
            for (k, (ratio, level, decay)) in PARTIALS.iter().enumerate() {
                phase[k] = (phase[k] + freq * ratio / sr).fract();
                v += level * (std::f32::consts::TAU * phase[k]).sin() * (-after / (n.decay * decay)).exp();
            }
            dry[start + i] += n.gain * rise * v;
        }
    }

    // One-pole low-pass: takes the edge off the overtones.
    let alpha = 1.0 - (-std::f32::consts::TAU * CUTOFF / sr).exp();
    let mut y = 0.0;
    for s in &mut dry {
        y += alpha * (*s - y);
        *s = y;
    }

    let mut out = dry.clone();
    for (delay, level) in ROOM {
        let d = (delay * sr) as usize;
        for i in d..len {
            out[i] += level * dry[i - d];
        }
    }

    let fade = (TAIL_FADE * sr) as usize;
    for (i, s) in out.iter_mut().enumerate() {
        let left = len - i;
        let fade_out = if left < fade { left as f32 / fade as f32 } else { 1.0 };
        *s = soft_limit(*s * MASTER * fade_out);
    }
    out
}

/// Linear below the knee, then bends smoothly toward [`CEILING`].
fn soft_limit(x: f32) -> f32 {
    let knee = CEILING * 0.7;
    let a = x.abs();
    if a <= knee {
        return x;
    }
    let room = CEILING - knee;
    x.signum() * (knee + room * ((a - knee) / room).tanh())
}

// ---- playback ----

#[cfg(target_os = "macos")]
mod engine {
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicBool, Ordering},
            mpsc::{self, RecvTimeoutError, Sender},
        },
        time::{Duration, Instant},
    };

    use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};

    use super::{Sound, render};

    /// With nothing playing for this long the stream stops (the device can
    /// sleep); the next sound starts it again in a few milliseconds.
    const PAUSE: Duration = Duration::from_secs(3);
    /// Stopped this long, the stream is released altogether.
    const RELEASE: Duration = Duration::from_secs(10 * 60);
    /// A sound that waited longer than this (the audio system still
    /// starting up) is dropped: late feedback is worse than none.
    const STALE: Duration = Duration::from_millis(250);
    /// At most this many voices at once; the oldest makes way.
    const VOICES: usize = 12;

    enum Msg {
        /// Open the output ahead of the first sound: the first use of the
        /// audio system in a process can take seconds.
        Warm,
        Play(Sound, Instant),
    }

    struct Voice {
        samples: Arc<[f32]>,
        at: usize,
    }

    #[derive(Default)]
    pub(super) struct Mixer {
        voices: Vec<Voice>,
    }

    pub(super) struct Output {
        stream: cpal::Stream,
        pub(super) rate: u32,
        /// The device it plays on, to follow a new default output.
        device: Option<String>,
        playing: bool,
        /// The stream reported an error (device gone): rebuild it.
        broken: Arc<AtomicBool>,
    }

    pub(super) fn warm() {
        send(Msg::Warm);
    }

    pub(super) fn play(sound: Sound) {
        send(Msg::Play(sound, Instant::now()));
    }

    fn send(msg: Msg) {
        // Tests stay silent, except those that ask to be heard.
        #[cfg(test)]
        if !super::tests::AUDIBLE.load(Ordering::Relaxed) {
            return;
        }
        static TX: OnceLock<Option<Mutex<Sender<Msg>>>> = OnceLock::new();
        let tx = TX.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("silo-sound".into())
                .spawn(move || run(&rx))
                .map_err(|e| tracing::warn!(%e, "sound: no thread"))
                .ok()
                .map(|_| Mutex::new(tx))
        });
        if let Some(tx) = tx
            && let Ok(tx) = tx.lock()
        {
            let _ = tx.send(msg);
        }
    }

    fn default_device() -> Option<(cpal::Device, Option<String>)> {
        let device = cpal::default_host().default_output_device()?;
        let name = device.name().ok();
        Some((device, name))
    }

    fn run(rx: &mpsc::Receiver<Msg>) {
        let mixer = Arc::new(Mutex::new(Mixer::default()));
        let mut output: Option<Output> = None;
        let mut cache: HashMap<(Sound, u32), Arc<[f32]>> = HashMap::new();
        loop {
            let wait = match &output {
                Some(o) if o.playing => PAUSE,
                Some(_) => RELEASE,
                None => Duration::MAX,
            };
            let msg = match rx.recv_timeout(wait) {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => {
                    let busy = mixer.lock().is_ok_and(|m| !m.voices.is_empty());
                    match &mut output {
                        Some(o) if o.playing && !busy => {
                            let _ = o.stream.pause();
                            o.playing = false;
                        }
                        Some(o) if !o.playing => output = None,
                        _ => {}
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            };
            // Gone, broken, or no longer the default output: open anew.
            let current = default_device();
            let stale_device = |o: &Output| o.broken.load(Ordering::Relaxed) || current.as_ref().is_some_and(|(_, name)| *name != o.device);
            if output.as_ref().is_some_and(stale_device) {
                output = None;
            }
            if output.is_none() {
                let started = Instant::now();
                output = current.and_then(|(device, name)| open_on(&device, name, &mixer));
                tracing::debug!(elapsed = ?started.elapsed(), opened = output.is_some(), "sound: output");
            }
            let Msg::Play(sound, at) = msg else { continue };
            let Some(out) = &mut output else { continue };
            if at.elapsed() > STALE {
                tracing::trace!(?sound, "sound: too late, dropped");
                continue;
            }
            let samples = cache.entry((sound, out.rate)).or_insert_with(|| render(sound, out.rate).into()).clone();
            if let Ok(mut m) = mixer.lock() {
                if m.voices.len() >= VOICES {
                    m.voices.remove(0);
                }
                m.voices.push(Voice { samples, at: 0 });
            }
            if !out.playing {
                match out.stream.play() {
                    Ok(()) => out.playing = true,
                    Err(e) => {
                        tracing::debug!(%e, "sound: play");
                        output = None;
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn open(mixer: &Arc<Mutex<Mixer>>) -> Option<Output> {
        let (device, name) = default_device()?;
        open_on(&device, name, mixer)
    }

    /// A stopped stream on `device` mixing `mixer`'s voices.
    fn open_on(device: &cpal::Device, name: Option<String>, mixer: &Arc<Mutex<Mixer>>) -> Option<Output> {
        let supported = device.default_output_config().map_err(|e| tracing::debug!(%e, "sound: no output config")).ok()?;
        if supported.sample_format() != cpal::SampleFormat::F32 {
            tracing::debug!(format = ?supported.sample_format(), "sound: unsupported sample format");
            return None;
        }
        let config: cpal::StreamConfig = supported.config();
        let channels = usize::from(config.channels.max(1));
        let rate = config.sample_rate.0;
        let mixer = mixer.clone();
        let broken = Arc::new(AtomicBool::new(false));
        let flag = broken.clone();
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| fill(&mixer, data, channels),
                move |e| {
                    tracing::debug!(%e, "sound: stream error");
                    flag.store(true, Ordering::Relaxed);
                },
                None,
            )
            .map_err(|e| tracing::debug!(%e, "sound: no output stream"))
            .ok()?;
        // Built streams may start running; stay stopped until a sound.
        let _ = stream.pause();
        Some(Output { stream, rate, device: name, playing: false, broken })
    }

    /// The audio callback: never waits for the lock (silence instead).
    fn fill(mixer: &Mutex<Mixer>, data: &mut [f32], channels: usize) {
        data.fill(0.0);
        let Ok(mut m) = mixer.try_lock() else { return };
        if m.voices.is_empty() {
            return;
        }
        for frame in data.chunks_mut(channels) {
            let mut v = 0.0;
            for voice in &mut m.voices {
                if let Some(s) = voice.samples.get(voice.at) {
                    v += s;
                    voice.at += 1;
                }
            }
            frame.fill(v.clamp(-1.0, 1.0));
        }
        m.voices.retain(|voice| voice.at < voice.samples.len());
    }
}

#[cfg(not(target_os = "macos"))]
mod engine {
    pub(super) fn warm() {}
    pub(super) fn play(_sound: super::Sound) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a test plays through the speaker ([`listen`]).
    pub(super) static AUDIBLE: AtomicBool = AtomicBool::new(false);

    const RATE: u32 = 48_000;

    #[test]
    fn every_sound_is_short_quiet_and_ends_in_silence() {
        for sound in Sound::ALL {
            let samples = render(sound, RATE);
            #[allow(clippy::cast_precision_loss)]
            let seconds = samples.len() as f32 / RATE as f32;
            assert!(seconds > 0.04 && seconds < 4.5, "{sound:?} lasts {seconds}s");
            let peak = samples.iter().fold(0.0f32, |p, s| p.max(s.abs()));
            assert!(peak > 0.01, "{sound:?} is audible ({peak})");
            assert!(peak <= CEILING, "{sound:?} stays under the ceiling ({peak})");
            assert!(samples.first().is_some_and(|s| s.abs() < 1e-3), "{sound:?} starts from silence");
            assert!(samples.last().is_some_and(|s| s.abs() < 1e-3), "{sound:?} ends in silence");
        }
    }

    #[test]
    fn no_sound_starts_with_a_click() {
        // Heard at once, but rising: the first half millisecond is still
        // quiet, and no step between samples jumps (a jump is a click).
        for sound in Sound::ALL {
            let samples = render(sound, RATE);
            let peak = samples.iter().fold(0.0f32, |p, s| p.max(s.abs()));
            let onset = samples[..24].iter().fold(0.0f32, |p, s| p.max(s.abs()));
            assert!(onset < peak * 0.1, "{sound:?} onset {onset} vs peak {peak}");
            let jump = samples.windows(2).fold(0.0f32, |j, w| j.max((w[1] - w[0]).abs()));
            assert!(jump < peak * 0.1, "{sound:?} jumps {jump} vs peak {peak}");
        }
    }

    #[test]
    fn sounds_are_mellow_not_bright() {
        // Sample-to-sample change is small next to the level: little
        // energy up high, where sounds turn sharp.
        for sound in Sound::ALL {
            let samples = render(sound, RATE);
            let energy: f32 = samples.iter().map(|s| s * s).sum();
            let edge: f32 = samples.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum();
            assert!(edge / energy < 0.06, "{sound:?} brightness {}", edge / energy);
        }
    }

    #[test]
    fn soft_limit_is_linear_below_the_knee_and_bounded_above() {
        assert!((soft_limit(0.1) - 0.1).abs() < f32::EPSILON);
        assert!(soft_limit(5.0) <= CEILING && soft_limit(-5.0) >= -CEILING);
        assert!(soft_limit(0.3) < 0.3 && soft_limit(0.3) > soft_limit(0.25));
    }

    #[test]
    fn programs_are_heard_in_silo_or_with_their_notification() {
        use silo_proto::control::AttentionReason as R;
        assert_eq!(for_attention(R::Done, true, false), Some(Sound::Success));
        assert_eq!(for_attention(R::Blocked, true, false), Some(Sound::Attention));
        assert_eq!(for_attention(R::Error, false, true), Some(Sound::Error));
        assert_eq!(for_attention(R::Done, false, false), None, "away from Silo with no banner: silent");
        assert_eq!(for_attention(R::Notification, true, false), None, "a program's note sounds only with its banner");
        assert_eq!(for_attention(R::Notification, false, true), Some(Sound::Attention));
        assert_eq!(for_attention(R::Unknown, true, true), None);
    }

    #[test]
    #[ignore = "writes WAVs to listen to: SILO_SOUND_DIR=/tmp/silo-sounds cargo test -p silo-app sound::tests::preview -- --ignored"]
    fn preview() {
        let dir = std::path::PathBuf::from(std::env::var("SILO_SOUND_DIR").unwrap_or_else(|_| "/tmp/silo-sounds".into()));
        std::fs::create_dir_all(&dir).unwrap();
        for sound in Sound::ALL {
            let samples = render(sound, RATE);
            let mut wav = Vec::new();
            let data = u32::try_from(samples.len() * 2).unwrap();
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + data).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&RATE.to_le_bytes());
            wav.extend_from_slice(&(RATE * 2).to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&data.to_le_bytes());
            for s in samples {
                #[allow(clippy::cast_possible_truncation)]
                wav.extend_from_slice(&((s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16).to_le_bytes());
            }
            std::fs::write(dir.join(format!("{sound:?}.wav")), wav).unwrap();
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "opens the default audio output"]
    fn the_default_output_opens() {
        let mixer = std::sync::Arc::new(std::sync::Mutex::new(engine::Mixer::default()));
        assert!(engine::open(&mixer).is_some_and(|o| o.rate > 0));
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "plays every sound on the default output"]
    fn listen() {
        AUDIBLE.store(true, Ordering::Relaxed);
        set_enabled(true);
        std::thread::sleep(Duration::from_secs(12));
        for sound in Sound::ALL {
            let t = Instant::now();
            play(sound);
            eprintln!("{sound:?} queued in {:?}", t.elapsed());
            std::thread::sleep(Duration::from_millis(900));
        }
    }

    #[test]
    fn switches_sound_their_new_state() {
        assert_eq!(Sound::toggle(true), Sound::ToggleOn);
        assert_eq!(Sound::toggle(false), Sound::ToggleOff);
    }
}
