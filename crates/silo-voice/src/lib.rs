//! On-device voice typing for Silo: everything but the UI. Audio never
//! leaves the Mac.
//!
//! - [`trigger`]: the Fn key's hold / double-press state machine;
//! - [`capture`] and [`resample`]: the microphone as 16 kHz mono;
//! - [`bands`]: the pill's visualizer levels (FFT voice bands);
//! - [`engine`]: Whistle ("Fast", live words) and Whisper ("Balanced", "Multilingual");
//! - [`models`]: their downloads; [`session`]: one dictation;
//! - [`hints`]: keyword hints from the focused block; [`prefs`]: settings;
//! - [`style`]: replacements, spoken commands and lists.

pub mod bands;
#[cfg(target_os = "macos")]
pub mod capture;
pub mod engine;
pub mod hints;
pub mod models;
#[cfg(target_os = "macos")]
pub mod permission;
pub mod prefs;
pub mod resample;
pub mod session;
pub mod style;
pub mod trigger;

pub use prefs::{EngineKind, Target, VoicePrefs};
