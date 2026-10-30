//! The microphone through CoreAudio (cpal): the system's input or a chosen
//! one, as mono chunks at the device's rate.

use std::sync::mpsc::Sender;

use anyhow::{Context as _, anyhow};
use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};

/// The input devices' names, the system's default first.
#[must_use]
pub fn microphones() -> Vec<String> {
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| d.name().ok());
    let mut names: Vec<String> = host.input_devices().map(|ds| ds.filter_map(|d| d.name().ok()).collect()).unwrap_or_default();
    names.sort_by_key(|n| Some(n) != default.as_ref());
    names.dedup();
    names
}

/// A running input stream; stops when dropped. Not `Send`: it lives on the
/// thread that opened it.
pub struct Input {
    _stream: cpal::Stream,
    pub rate: u32,
}

/// Open `name` (empty or gone: the system's input) and send its audio, as
/// mono at [`Input::rate`], to `tx`.
///
/// # Errors
/// No input device, or it can't be opened (the microphone permission).
pub fn open(name: &str, tx: Sender<Vec<f32>>) -> anyhow::Result<Input> {
    let host = cpal::default_host();
    let chosen = (!name.is_empty()).then(|| host.input_devices().ok()?.find(|d| d.name().is_ok_and(|n| n == name))).flatten();
    let device = chosen.or_else(|| host.default_input_device()).ok_or_else(|| anyhow!("No microphone"))?;
    let supported = device.default_input_config().context("microphone config")?;
    let config: cpal::StreamConfig = supported.config();
    let channels = usize::from(config.channels.max(1));
    let rate = config.sample_rate.0;
    let error = |e: cpal::StreamError| tracing::warn!(%e, "voice: microphone stream");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            device.build_input_stream(&config, move |data: &[f32], _| drop(tx.send(crate::resample::mono(data, channels))), error, None)
        }
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| {
                let floats: Vec<f32> = data.iter().map(|s| f32::from(*s) / 32768.0).collect();
                drop(tx.send(crate::resample::mono(&floats, channels)));
            },
            error,
            None,
        ),
        format => return Err(anyhow!("unsupported microphone format {format:?}")),
    }
    .context("open the microphone")?;
    stream.play().context("start the microphone")?;
    Ok(Input { _stream: stream, rate })
}
