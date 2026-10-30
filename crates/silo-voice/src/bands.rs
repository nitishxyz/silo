//! The pill's visualizer maths: microphone audio (16 kHz mono) to a few
//! log-spaced voice bands (80 Hz–4 kHz) through a real FFT, smoothed with a
//! quick attack and a slow decay so the bars breathe, then laid out as
//! mirrored bars (the lowest band in the middle) with an idle pulse when
//! quiet. Pure functions; the capture thread runs [`Analyzer`].

use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex32};

/// Bands per side; the pill shows them mirrored (twice as many bars).
pub const BANDS: usize = 10;
/// The FFT's size at 16 kHz: a 64 ms window, 15.6 Hz bins.
pub const FFT_SIZE: usize = 1024;
/// New levels every this many samples: 16 ms, about 60 a second.
pub const HOP: usize = 256;
pub const RATE: f32 = 16_000.0;
pub const LOW_HZ: f32 = 80.0;
pub const HIGH_HZ: f32 = 4_000.0;
/// A band at this loudness (dBFS) shows nothing, at [`CEIL_DB`] full.
const FLOOR_DB: f32 = -68.0;
const CEIL_DB: f32 = -22.0;
/// Speech loses ~6 dB an octave up high; this much of it is given back.
const TILT_DB_PER_OCTAVE: f32 = 3.0;
/// Smoothing time constants, seconds: rise fast, fall slowly.
pub const ATTACK: f32 = 0.025;
pub const DECAY: f32 = 0.16;

/// `bands + 1` edges from `low` to `high` Hz, evenly spaced in log frequency.
#[must_use]
pub fn band_edges(bands: usize, low: f32, high: f32) -> Vec<f32> {
    #[allow(clippy::cast_precision_loss)]
    (0..=bands).map(|k| low * (high / low).powf(k as f32 / bands as f32)).collect()
}

/// Each band's level (0..=1) from one FFT's bin magnitudes (amplitude,
/// already normalised so a full-scale sine reads 1.0): the mean power of
/// the bins centred in the band (the nearest bin when none is), in dB with
/// a gentle treble lift, mapped from [`FLOOR_DB`]..[`CEIL_DB`].
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn band_levels(magnitudes: &[f32], rate: f32, fft_size: usize, edges: &[f32]) -> Vec<f32> {
    let bin_hz = rate / fft_size as f32;
    let last = magnitudes.len().saturating_sub(1);
    edges
        .windows(2)
        .map(|edge| {
            let (lo, hi) = (edge[0], edge[1]);
            let first = ((lo / bin_hz).ceil() as usize).min(last);
            let end = ((hi / bin_hz).ceil() as usize).min(last + 1).max(first + 1);
            let power = magnitudes[first..end].iter().map(|m| m * m).sum::<f32>() / (end - first) as f32;
            let db = 10.0 * (power + 1e-12).log10() + TILT_DB_PER_OCTAVE * (lo / LOW_HZ).log2();
            ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0)
        })
        .collect()
}

/// One step of `current` toward `target` over `dt` seconds: time constant
/// `attack` going up, `decay` going down. Never overshoots.
#[must_use]
pub fn smooth(current: f32, target: f32, dt: f32, attack: f32, decay: f32) -> f32 {
    let tc = if target > current { attack } else { decay };
    let k = 1.0 - (-dt / tc.max(1e-4)).exp();
    current + (target - current) * k
}

/// Hann-windowed FFTs over the latest [`FFT_SIZE`] samples, every [`HOP`].
pub struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Gain making a full-scale sine's bin read 1.0.
    norm: f32,
    edges: Vec<f32>,
    history: Vec<f32>,
    pending: usize,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    magnitudes: Vec<f32>,
    levels: [f32; BANDS],
}

impl Default for Analyzer {
    fn default() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        #[allow(clippy::cast_precision_loss)]
        let window: Vec<f32> = (0..FFT_SIZE).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_SIZE as f32).cos()).collect();
        let norm = 2.0 / window.iter().sum::<f32>();
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        Self {
            fft,
            window,
            norm,
            edges: band_edges(BANDS, LOW_HZ, HIGH_HZ),
            history: vec![0.0; FFT_SIZE],
            pending: 0,
            buf: vec![Complex32::default(); FFT_SIZE],
            scratch,
            magnitudes: vec![0.0; FFT_SIZE / 2],
            levels: [0.0; BANDS],
        }
    }
}

impl Analyzer {
    /// Feed 16 kHz mono samples; true when the levels moved on.
    pub fn push(&mut self, samples: &[f32]) -> bool {
        let mut stepped = false;
        for chunk in samples.chunks(HOP) {
            self.history.drain(..chunk.len());
            self.history.extend_from_slice(chunk);
            self.pending += chunk.len();
            while self.pending >= HOP {
                self.pending -= HOP;
                self.step();
                stepped = true;
            }
        }
        stepped
    }

    fn step(&mut self) {
        for ((out, s), w) in self.buf.iter_mut().zip(&self.history).zip(&self.window) {
            *out = Complex32::new(s * w, 0.0);
        }
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        for (m, c) in self.magnitudes.iter_mut().zip(&self.buf) {
            *m = c.norm() * self.norm;
        }
        let targets = band_levels(&self.magnitudes, RATE, FFT_SIZE, &self.edges);
        #[allow(clippy::cast_precision_loss)]
        let dt = HOP as f32 / RATE;
        for (level, target) in self.levels.iter_mut().zip(targets) {
            *level = smooth(*level, target, dt, ATTACK, DECAY);
        }
    }

    #[must_use]
    pub fn levels(&self) -> [f32; BANDS] {
        self.levels
    }
}

/// Bars shown: each band twice, mirrored about the middle.
pub const BARS: usize = BANDS * 2;
/// The quiet bars' resting height and how far the idle pulse moves them.
const IDLE: f32 = 0.10;
const IDLE_SWING: f32 = 0.05;

/// Bar `bar`'s distance from the middle in bars (0 for the two middle ones).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn from_middle(bar: usize) -> f32 {
    ((bar as f32) - (BARS as f32 - 1.0) / 2.0).abs() - 0.5
}

/// The band bar `bar` shows: the lowest in the middle, the highest at the edges.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn band_of(bar: usize) -> usize {
    (from_middle(bar).round() as usize).min(BANDS - 1)
}

/// The idle pulse at `t` seconds: a slow wave travelling outward from the
/// middle; still with reduced motion.
#[must_use]
pub fn idle(bar: usize, t: f32, reduce_motion: bool) -> f32 {
    if reduce_motion {
        return IDLE;
    }
    IDLE + IDLE_SWING * (std::f32::consts::TAU * 0.7 * t - 0.55 * from_middle(bar)).sin()
}

/// Every bar's height (0..=1) for band `levels` at `t` seconds: the voice,
/// never below the idle pulse.
#[must_use]
pub fn bar_heights(levels: &[f32; BANDS], t: f32, reduce_motion: bool) -> [f32; BARS] {
    std::array::from_fn(|bar| levels[band_of(bar)].max(idle(bar, t, reduce_motion)).clamp(0.0, 1.0))
}

/// Bar `bar`'s opacity: full in the middle, fading toward the edges.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn bar_opacity(bar: usize) -> f32 {
    let edge = (BARS as f32 / 2.0 - 1.0).max(1.0);
    1.0 - 0.6 * (from_middle(bar) / edge).powf(1.4)
}

/// While transcribing: the bars, `collapse` (0..=1) of the way down to a
/// low line, with a soft highlight sweeping across them.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn shimmer(last: &[f32; BARS], collapse: f32, t: f32, reduce_motion: bool) -> [f32; BARS] {
    let c = collapse.clamp(0.0, 1.0);
    std::array::from_fn(|bar| {
        let line = if reduce_motion {
            0.12
        } else {
            let phase = (t * 1.1).fract() * (BARS as f32 + 6.0) - 3.0;
            0.10 + 0.12 * (-(bar as f32 - phase).powi(2) / 4.0).exp()
        };
        last[bar] * (1.0 - c) + line * c
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        #[allow(clippy::cast_precision_loss)]
        (0..n).map(|i| amp * (std::f32::consts::TAU * freq * i as f32 / RATE).sin()).collect()
    }

    #[test]
    fn edges_are_log_spaced_over_the_voice_range() {
        let edges = band_edges(BANDS, LOW_HZ, HIGH_HZ);
        assert_eq!(edges.len(), BANDS + 1);
        assert!((edges[0] - LOW_HZ).abs() < 1e-3 && (edges[BANDS] - HIGH_HZ).abs() < 0.1);
        let ratios: Vec<f32> = edges.windows(2).map(|w| w[1] / w[0]).collect();
        assert!(ratios.iter().all(|r| (r - ratios[0]).abs() < 1e-3), "{ratios:?}");
    }

    #[test]
    fn a_tone_lights_its_own_band() {
        let edges = band_edges(BANDS, LOW_HZ, HIGH_HZ);
        for freq in [150.0, 440.0, 1000.0, 2500.0] {
            let mut a = Analyzer::default();
            a.push(&sine(freq, 0.3, 16_000));
            let levels = a.levels();
            let own = edges.windows(2).position(|w| freq >= w[0] && freq < w[1]).unwrap();
            let loudest = (0..BANDS).max_by(|x, y| levels[*x].total_cmp(&levels[*y])).unwrap();
            assert_eq!(loudest, own, "{freq} Hz: {levels:?}");
            assert!(levels[own] > 0.8, "{freq} Hz is loud: {levels:?}");
        }
    }

    #[test]
    fn silence_shows_nothing_and_quiet_little() {
        let mut a = Analyzer::default();
        assert!(a.push(&vec![0.0; 4096]));
        assert!(a.levels().iter().all(|l| *l == 0.0));
        a.push(&sine(440.0, 0.0005, 16_000));
        assert!(a.levels().iter().all(|l| *l < 0.35), "{:?}", a.levels());
        assert!(!Analyzer::default().push(&[0.0; HOP - 1]), "no new levels before a hop");
    }

    #[test]
    fn levels_rise_fast_and_fall_slowly() {
        let up = smooth(0.0, 1.0, 0.016, ATTACK, DECAY);
        let down = 1.0 - smooth(1.0, 0.0, 0.016, ATTACK, DECAY);
        assert!(up > 0.4 && down < 0.15 && up > 3.0 * down, "up {up} down {down}");
        let mut v = 0.0;
        for _ in 0..200 {
            v = smooth(v, 0.7, 0.016, ATTACK, DECAY);
            assert!(v <= 0.7);
        }
        assert!((v - 0.7).abs() < 1e-3);
    }

    #[test]
    fn bars_mirror_low_bands_in_the_middle() {
        assert_eq!((band_of(BARS / 2 - 1), band_of(BARS / 2)), (0, 0));
        assert_eq!((band_of(0), band_of(BARS - 1)), (BANDS - 1, BANDS - 1));
        let mut levels = [0.0; BANDS];
        levels[0] = 0.9;
        levels[BANDS - 1] = 0.5;
        let h = bar_heights(&levels, 1.3, false);
        for bar in 0..BARS {
            assert!((h[bar] - h[BARS - 1 - bar]).abs() < 1e-6, "symmetric");
            assert!(h[bar] > 0.0, "never flat");
        }
        assert!((h[BARS / 2] - 0.9).abs() < 1e-6 && (h[0] - 0.5).abs() < 1e-6);
        assert!(bar_opacity(BARS / 2) > bar_opacity(0) && bar_opacity(0) > 0.39);
    }

    #[test]
    fn quiet_bars_pulse_unless_motion_is_reduced() {
        let at = |t| bar_heights(&[0.0; BANDS], t, false)[BARS / 2];
        assert!((at(0.0) - at(0.35)).abs() > 0.01, "moves");
        assert!((0.0..0.2).contains(&at(0.2)));
        let still = |t| bar_heights(&[0.0; BANDS], t, true);
        assert_eq!(still(0.0), still(0.5));
    }

    #[test]
    fn transcribing_collapses_into_a_shimmer() {
        let last = [0.8; BARS];
        assert_eq!(shimmer(&last, 0.0, 0.3, false), last);
        let done = shimmer(&last, 1.0, 0.3, false);
        assert!(done.iter().all(|h| *h < 0.25), "{done:?}");
        assert!(shimmer(&last, 1.0, 0.3, false) != shimmer(&last, 1.0, 0.6, false), "it sweeps");
        assert_eq!(shimmer(&last, 1.0, 0.3, true), shimmer(&last, 1.0, 0.6, true));
    }
}
