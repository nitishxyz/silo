//! The microphone's audio to what the engines take: mono, 16 kHz.

/// The engines' sample rate.
pub const RATE: u32 = 16_000;

/// Interleaved `frames` of `channels` as mono (their mean).
#[must_use]
pub fn mono(frames: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return frames.to_vec();
    }
    #[allow(clippy::cast_precision_loss)]
    let scale = 1.0 / channels as f32;
    frames.chunks_exact(channels).map(|f| f.iter().sum::<f32>() * scale).collect()
}

/// A streaming area-averaging resampler: each output sample is the mean of
/// the input it spans (a box low-pass, enough for speech), so chunks of any
/// size give the same output as one long buffer.
#[derive(Debug)]
pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    need: f64,
    acc: f64,
}

impl Resampler {
    #[must_use]
    pub fn new(from: u32, to: u32) -> Self {
        let step = f64::from(from.max(1)) / f64::from(to.max(1));
        Self { step, need: step, acc: 0.0 }
    }

    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for &x in input {
            let x = f64::from(x);
            let mut left: f64 = 1.0;
            while left > 1e-12 {
                let take = left.min(self.need);
                self.acc += x * take;
                self.need -= take;
                left -= take;
                if self.need <= 1e-9 {
                    #[allow(clippy::cast_possible_truncation)]
                    out.push((self.acc / self.step) as f32);
                    self.acc = 0.0;
                    self.need = self.step;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(from: u32, input: &[f32], chunk: usize) -> Vec<f32> {
        let mut r = Resampler::new(from, RATE);
        let mut out = Vec::new();
        for c in input.chunks(chunk) {
            r.push(c, &mut out);
        }
        out
    }

    #[test]
    fn rates_convert_to_16k_in_any_chunks() {
        let second = |rate: u32| vec![0.25f32; rate as usize];
        for rate in [48_000, 44_100, 24_000, 16_000, 8_000] {
            let out = run(rate, &second(rate), 512);
            assert!(out.len().abs_diff(16_000) <= 1, "{rate}: {}", out.len());
            assert!(out.iter().all(|s| (s - 0.25).abs() < 1e-4), "{rate}: level kept");
            assert_eq!(run(rate, &second(rate), 77), out, "{rate}: chunking changes nothing");
        }
    }

    #[test]
    fn speech_frequencies_pass() {
        #[allow(clippy::cast_precision_loss)]
        let tone: Vec<f32> = (0..48_000).map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin()).collect();
        let peak = run(48_000, &tone, 480).iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak > 0.85, "{peak}");
    }

    #[test]
    fn channels_mix_to_mono() {
        assert_eq!(mono(&[1.0, 0.0, 0.5, 0.5], 2), [0.5, 0.5]);
        assert_eq!(mono(&[0.1, 0.2], 1), [0.1, 0.2]);
    }
}
