//! Opt-in per-frame timing for the render path, off by default.
//!
//! Set `TERMINAL_PROFILE=1` in the environment of the Terminal process to
//! get one stderr line per [`REPORT_FRAMES`] painted frames, splitting the
//! cost between the preparation thread (row planning, sprite geometry, text
//! shaping) and the UI thread (the GPUI paint calls). Nothing is measured,
//! allocated or printed when the variable is absent, so the check is a
//! single relaxed atomic load on the hot path.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Frames between reports; roughly two seconds of a busy TUI.
const REPORT_FRAMES: u64 = 120;

#[must_use]
pub(crate) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TERMINAL_PROFILE").is_some_and(|value| value == "1"))
}

/// `Instant::now()` only while profiling.
#[must_use]
pub(crate) fn start() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// Elapsed time since an optional start, accumulated into `total`.
pub(crate) fn add(total: &mut Duration, started: Option<Instant>) {
    if let Some(started) = started {
        *total += started.elapsed();
    }
}

/// What one prepared frame cost, off the UI thread.
#[derive(Default)]
pub(crate) struct Prepared {
    pub plan: Duration,
    pub sprites: Duration,
    pub shape: Duration,
    pub total: Duration,
    /// Runs shaped by the text system this frame.
    pub shaped: u64,
    /// Runs served from the preparation layout cache.
    pub reused: u64,
}

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        $(static $name: AtomicU64 = AtomicU64::new(0);)*
    };
}

counters!(PREPARE, PLAN, SPRITES, SHAPE, SHAPED, REUSED, PAINT, QUADS, GLYPHS, PREPARES, FRAMES,);

fn add_ns(counter: &AtomicU64, value: Duration) {
    counter.fetch_add(u64::try_from(value.as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed);
}

pub(crate) fn record_prepared(sample: &Prepared) {
    if !enabled() {
        return;
    }
    add_ns(&PREPARE, sample.total);
    add_ns(&PLAN, sample.plan);
    add_ns(&SPRITES, sample.sprites);
    add_ns(&SHAPE, sample.shape);
    SHAPED.fetch_add(sample.shaped, Ordering::Relaxed);
    REUSED.fetch_add(sample.reused, Ordering::Relaxed);
    PREPARES.fetch_add(1, Ordering::Relaxed);
}

/// One painted frame on the UI thread. Reports and resets every
/// [`REPORT_FRAMES`] frames.
pub(crate) fn record_painted(started: Option<Instant>, quads: u64, glyphs: u64) {
    let Some(started) = started else {
        return;
    };
    add_ns(&PAINT, started.elapsed());
    QUADS.fetch_add(quads, Ordering::Relaxed);
    GLYPHS.fetch_add(glyphs, Ordering::Relaxed);
    if FRAMES.fetch_add(1, Ordering::Relaxed) + 1 < REPORT_FRAMES {
        return;
    }
    report();
}

#[allow(clippy::cast_precision_loss, reason = "Profiling report: nanosecond sums and counts as floats")]
fn report() {
    let frames = FRAMES.swap(0, Ordering::Relaxed).max(1);
    let prepares = PREPARES.swap(0, Ordering::Relaxed).max(1);
    let per = |counter: &AtomicU64, over: u64| counter.swap(0, Ordering::Relaxed) as f64 / 1_000_000.0 / over as f64;
    let mean = |counter: &AtomicU64, over: u64| counter.swap(0, Ordering::Relaxed) / over;
    eprintln!(
        "terminal profile: {frames} painted frames, UI-thread paint {:.3} ms/frame ({} quads, {} glyphs); \
{prepares} prepared frames off-thread {:.3} ms each (plan {:.3}, sprites {:.3}, shaping {:.3}; {} runs shaped, {} reused)",
        per(&PAINT, frames),
        mean(&QUADS, frames),
        mean(&GLYPHS, frames),
        per(&PREPARE, prepares),
        per(&PLAN, prepares),
        per(&SPRITES, prepares),
        per(&SHAPE, prepares),
        mean(&SHAPED, prepares),
        mean(&REUSED, prepares),
    );
}
