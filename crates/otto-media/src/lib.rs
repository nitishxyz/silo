//! Media generation (docs/MEDIA.md): providers that make images, videos and audio with the user's own API keys,
//! the [`Media`] service behind `media.*`, and the tools: `generate_image`, `generate_video`, `generate_audio`, and
//! for any model with all its options `list_media_models`, `media_model_schema` and `generate_media` (§3.2).
mod catalog;
mod endpoints;
mod http;
mod models;
mod poll;
mod probe;
pub mod providers;
mod schema;
mod service;
mod tools;

pub use catalog::{ModelEntry, Voice, handles, slug};
pub use endpoints::{Endpoint, Family, families};
pub use otto_proto::{MediaKeySource, MediaKind};
pub use service::Media;

use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaErrorKind {
    /// Bad input, or an unknown provider or model.
    Invalid,
    /// The provider refused the key.
    Auth,
    Cancelled,
    Failed,
}

/// Never carries a key: provider messages are scrubbed of the key they were sent with.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct MediaError {
    pub kind: MediaErrorKind,
    pub message: String,
}

impl MediaError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self { kind: MediaErrorKind::Invalid, message: message.into() }
    }
    pub fn failed(message: impl Into<String>) -> Self {
        Self { kind: MediaErrorKind::Failed, message: message.into() }
    }
    #[must_use]
    pub fn cancelled() -> Self {
        Self { kind: MediaErrorKind::Cancelled, message: "Cancelled".into() }
    }
}

/// An input file, ready to send: an image (`references`, PNG or JPEG under the providers' limits), or a video or
/// audio file for `generate_media`.
#[derive(Clone, Debug)]
pub struct Reference {
    pub bytes: Vec<u8>,
    pub mime: String,
}

impl Reference {
    /// `data:<mime>;base64,…`.
    #[must_use]
    pub fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.mime, otto_tools::attachments::encode(&self.bytes))
    }
}

#[derive(Clone, Debug)]
pub struct ImageRequest {
    pub prompt: String,
    /// Always set: the call's, the kind's default or the provider's default.
    pub model: String,
    pub references: Vec<Reference>,
    pub aspect_ratio: Option<String>,
    /// `low`, `medium` or `high`.
    pub quality: Option<String>,
    pub transparent: bool,
    pub n: u8,
    /// How much the model plans first: `low`, `medium` or `high` (GPT Image 2.6).
    pub complexity: Option<String>,
    /// `png`, `jpeg`, `webp`, `svg` or `gif`; `None` is the provider's default.
    pub format: Option<String>,
    /// GIF: frames (2–64) and each frame's milliseconds (20–2000).
    pub frame_count: Option<u8>,
    pub frame_duration_ms: Option<u32>,
    /// A transparent PNG annotation layer over the first reference, its size.
    pub markup: Option<Reference>,
}

#[derive(Clone, Debug)]
pub struct VideoRequest {
    pub prompt: String,
    /// Always set, as for images.
    pub model: String,
    pub first_frame: Option<Reference>,
    pub last_frame: Option<Reference>,
    /// Style or character images.
    pub references: Vec<Reference>,
    /// Seconds as asked; each provider takes its model's nearest allowed length and says so.
    pub duration: Option<f64>,
    /// `16:9`, `9:16` or `1:1`.
    pub aspect_ratio: Option<String>,
    /// `480p`, `720p` or `1080p`.
    pub resolution: Option<String>,
    /// With or without sound; `None` is the model's default.
    pub sound: Option<bool>,
}

/// What `generate_audio` makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioType {
    Speech,
    Sound,
    Music,
}

impl AudioType {
    /// `speech`, `sound effects`, `music`.
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Self::Speech => "speech",
            Self::Sound => "sound effects",
            Self::Music => "music",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AudioRequest {
    pub kind: AudioType,
    /// What to say, or the sound or music prompt.
    pub text: String,
    /// The call's or the kind's default; else the provider's default for `kind`.
    pub model: Option<String>,
    /// Speech: a voice id or name.
    pub voice: Option<String>,
    /// Sound and music: seconds.
    pub duration: Option<f64>,
}

/// One file a provider made.
#[derive(Clone, Debug)]
pub struct Generated {
    pub bytes: Vec<u8>,
    pub mime: String,
}

#[derive(Clone, Debug, Default)]
pub struct Output {
    pub media: Vec<Generated>,
    /// The model that made it (a provider may switch to its edit model for references).
    pub model: String,
    pub revised_prompt: Option<String>,
    /// What the provider couldn't honour (`fal can't make transparent backgrounds`), one line each.
    pub notes: Vec<String>,
}

/// What a generation gets besides its request.
#[derive(Clone)]
pub struct Call {
    pub http: reqwest::Client,
    pub cancel: CancellationToken,
    /// Waiting news for the user (`Generating with Higgsfield… in progress, 12 s`).
    pub progress: Arc<dyn Fn(String) + Send + Sync>,
}

/// A media provider: its models, where its key comes from, and what it makes.
#[async_trait::async_trait]
pub trait MediaProvider: Send + Sync {
    fn id(&self) -> &'static str;
    /// As people say it (`OpenAI`, `Gemini`).
    fn name(&self) -> &'static str;
    fn kinds(&self) -> &'static [MediaKind];
    /// Known models, the default of each kind first among its kind.
    fn models(&self) -> Vec<ModelEntry>;
    /// Environment variables holding its key.
    fn env(&self) -> &'static [&'static str];
    /// The key from the environment.
    fn env_key(&self, env: &(dyn Fn(&str) -> Option<String> + Send + Sync)) -> Option<String> {
        self.env().iter().find_map(|name| env(name).filter(|v| !v.trim().is_empty()))
    }
    /// Its chat provider's API key counts (`openai`, `google`, `xai`).
    fn shares_chat_key(&self) -> bool {
        false
    }
    /// A key as typed, checked before it is stored (Higgsfield's `id:secret`).
    fn check_key(&self, _key: &str) -> Result<(), MediaError> {
        Ok(())
    }
    /// Its live catalog (aggregators): every endpoint, with its input schema where the listing has it; `None`
    /// without one or when it can't be read.
    async fn endpoints(&self, _http: &reqwest::Client, _key: Option<&str>) -> Option<Vec<Endpoint>> {
        None
    }
    /// One endpoint with its schema, looked up on demand (fal: any endpoint id).
    async fn endpoint(&self, _http: &reqwest::Client, _key: Option<&str>, _id: &str) -> Option<Endpoint> {
        None
    }
    /// `file` where the provider's models can read it: its storage's link, else a data URI.
    ///
    /// # Errors
    /// The upload failed or was cancelled.
    async fn upload(&self, _key: &str, file: &Reference, _call: &Call) -> Result<String, MediaError> {
        Ok(file.data_uri())
    }
    /// Run endpoint `id` of the live catalog with `input` as its schema has it (media fields already links).
    ///
    /// # Errors
    /// As [`Self::generate_image`].
    async fn generate(&self, _key: &str, _id: &str, _input: serde_json::Value, _call: &Call) -> Result<Output, MediaError> {
        Err(MediaError::invalid(format!("{} has no live catalog to run", self.name())))
    }
    /// # Errors
    /// The provider refused or failed, or the call was cancelled.
    async fn generate_image(&self, _key: &str, _request: ImageRequest, _call: &Call) -> Result<Output, MediaError> {
        Err(MediaError::invalid(format!("{} can't make images", self.name())))
    }
    /// # Errors
    /// As [`Self::generate_image`].
    async fn generate_video(&self, _key: &str, _request: VideoRequest, _call: &Call) -> Result<Output, MediaError> {
        Err(MediaError::invalid(format!("{} can't make videos", self.name())))
    }
    /// What [`Self::generate_audio`] makes.
    fn audio_types(&self) -> &'static [AudioType] {
        &[]
    }
    /// # Errors
    /// As [`Self::generate_image`].
    async fn generate_audio(&self, _key: &str, _request: AudioRequest, _call: &Call) -> Result<Output, MediaError> {
        Err(MediaError::invalid(format!("{} can't make audio", self.name())))
    }
    /// Its speech voices (live with a key where it has a voice library), the default marked.
    async fn voices(&self, _http: &reqwest::Client, _key: Option<&str>) -> Vec<Voice> {
        vec![]
    }
}

impl dyn MediaProvider {
    /// Its default model for `kind`.
    #[must_use]
    pub fn default_model(&self, kind: MediaKind) -> Option<String> {
        self.models().into_iter().find(|m| m.kind == kind && m.default).map(|m| m.id)
    }
}
