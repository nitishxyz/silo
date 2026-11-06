//! The media providers (docs/MEDIA.md §2), in the order the setup question lists them.
mod elevenlabs;
mod fal;
mod google;
mod higgsfield;
mod openai;
mod xai;

pub use elevenlabs::ElevenLabs;
pub use fal::Fal;
pub use google::Google;
pub use higgsfield::Higgsfield;
pub use openai::OpenAi;
pub use xai::Xai;

use crate::MediaProvider;
use std::sync::Arc;

/// Every provider at its public endpoint, or where `OTTO_MEDIA_<ID>_BASE_URL` points (a proxy, a gateway; fal's
/// `OTTO_MEDIA_FAL_BASE_URL` serves its run, queue, API and storage hosts).
#[must_use]
pub fn all() -> Vec<Arc<dyn MediaProvider>> {
    with_bases(&|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

/// [`all`] with base overrides read through `var`.
pub fn with_bases(var: &dyn Fn(&str) -> Option<String>) -> Vec<Arc<dyn MediaProvider>> {
    let base = |id: &str| var(&format!("OTTO_MEDIA_{}_BASE_URL", id.to_ascii_uppercase()));
    vec![
        Arc::new(base("openai").map_or_else(OpenAi::default, OpenAi::new)),
        Arc::new(base("google").map_or_else(Google::default, Google::new)),
        Arc::new(base("xai").map_or_else(Xai::default, Xai::new)),
        Arc::new(base("higgsfield").map_or_else(Higgsfield::default, |b| Higgsfield::new(b, std::time::Duration::from_secs(2)))),
        Arc::new(base("fal").map_or_else(Fal::default, |b| Fal::new(b.clone(), b.clone(), b.clone()).storage(b))),
        Arc::new(base("elevenlabs").map_or_else(ElevenLabs::default, ElevenLabs::new)),
    ]
}

/// The first wait between status checks of a video.
pub(crate) const POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// `asked` seconds as a model takes them: rounded into `min..=max`, with a note when that changed them.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn seconds(asked: Option<f64>, (min, max): (u32, u32), model: &str, notes: &mut Vec<String>) -> Option<u32> {
    let asked = asked?;
    let made = asked.round().clamp(f64::from(min), f64::from(max)) as u32;
    if (f64::from(made) - asked).abs() > 0.01 {
        notes.push(format!("{model} makes {min}–{max} s videos in whole seconds: made {made} s."));
    }
    Some(made)
}

/// What an image provider without GPT Image 2.6's settings ignores, one note each.
pub(crate) fn unsupported(name: &str, r: &crate::ImageRequest) -> Vec<String> {
    let mut notes = Vec::new();
    if r.complexity.is_some() {
        notes.push(format!("complexity is not supported by {name}; ignored."));
    }
    if let Some(format) = &r.format {
        notes.push(format!("format {format} is not supported by {name}; ignored, made its usual format."));
    }
    if r.markup.is_some() {
        notes.push(format!("markup is not supported by {name}; ignored."));
    }
    notes
}

/// `1024x1024`, `1536x1024` or `1024x1536` for an aspect ratio (OpenAI's sizes).
pub(crate) fn openai_size(aspect: &str) -> &'static str {
    match aspect {
        "16:9" | "3:2" | "4:3" => "1536x1024",
        "9:16" | "2:3" | "3:4" => "1024x1536",
        _ => "1024x1024",
    }
}
