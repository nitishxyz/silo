//! OpenAI images: `POST /v1/images/generations`, or `/v1/images/edits` (multipart `image[]`) with references;
//! speech: `POST /v1/audio/speech`.
use crate::catalog::{EDITS, TEXT, model};
use crate::http::{self, send};
use crate::{AudioRequest, AudioType, Call, ImageRequest, MediaError, MediaKind, MediaProvider, ModelEntry, Output, Voice};
use serde_json::{Value, json};

pub struct OpenAi {
    base: String,
}

impl Default for OpenAi {
    fn default() -> Self {
        Self::new("https://api.openai.com")
    }
}

impl OpenAi {
    /// `base` without `/v1`.
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into().trim_end_matches('/').to_owned() }
    }
}

/// The Speech API's model (its successor, `gpt-realtime-2.1-mini`, is Realtime-only).
const SPEECH: &str = "gpt-4o-mini-tts";

/// Built-in voices, the recommended first.
const VOICES: &[(&str, &str)] = &[
    ("marin", "recommended, natural"),
    ("cedar", "recommended, natural"),
    ("alloy", ""),
    ("ash", ""),
    ("ballad", ""),
    ("coral", ""),
    ("echo", ""),
    ("fable", ""),
    ("nova", ""),
    ("onyx", ""),
    ("sage", ""),
    ("shimmer", ""),
    ("verse", ""),
];

#[async_trait::async_trait]
impl MediaProvider for OpenAi {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn name(&self) -> &'static str {
        "OpenAI"
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Image, MediaKind::Audio]
    }
    fn models(&self) -> Vec<ModelEntry> {
        let out = "1024×1024, 1536×1024 or 1024×1536 PNG, JPEG or WebP; transparent backgrounds";
        let out26 = "1024×1024, 1536×1024 or 1024×1536 PNG, JPEG, WebP, SVG or animated GIF; transparent backgrounds";
        vec![
            model(
                MediaKind::Image,
                "gpt-image-2.6-flare",
                "GPT Image 2.6 Flare",
                "Fast iteration; complexity control, markup-guided edits, SVG and animated GIF output",
                EDITS,
                out26,
            )
            .default(),
            model(
                MediaKind::Image,
                "gpt-image-2.6-sunburst",
                "GPT Image 2.6 Sunburst",
                "Detailed visual work; complexity control, markup-guided edits, SVG and animated GIF output",
                EDITS,
                out26,
            ),
            model(
                MediaKind::Image,
                "gpt-image-2.5-flare",
                "GPT Image 2.5 Flare",
                "Fast, natural lighting, rich textures, complex layouts and text; markup-guided edits",
                EDITS,
                out,
            ),
            model(
                MediaKind::Image,
                "gpt-image-2.5-sunburst",
                "GPT Image 2.5 Sunburst",
                "Precision: extra fidelity on intricate detail, slower; markup-guided edits",
                EDITS,
                out,
            ),
            model(MediaKind::Image, "gpt-image-2", "GPT Image 2", "The previous model: detailed images, fine typography", EDITS, out),
            model(
                MediaKind::Audio,
                SPEECH,
                "GPT-4o mini TTS",
                "Speech in 13 built-in voices, steerable tone; shuts down Jan 6 2027",
                TEXT,
                "speech, up to 4096 characters",
            )
            .default(),
            model(MediaKind::Audio, "gpt-4o-mini-tts-2025-12-15", "GPT-4o mini TTS (2025-12-15)", "The pinned snapshot", TEXT, "speech"),
        ]
    }
    fn env(&self) -> &'static [&'static str] {
        &["OPENAI_API_KEY"]
    }
    fn shares_chat_key(&self) -> bool {
        true
    }
    async fn generate_image(&self, key: &str, r: ImageRequest, call: &Call) -> Result<Output, MediaError> {
        let (v26, v25) = (r.model.starts_with("gpt-image-2.6"), r.model.starts_with("gpt-image-2.5"));
        let mut notes = Vec::new();
        let complexity = match &r.complexity {
            Some(c) if v26 => Some(c.as_str()),
            Some(_) => {
                notes.push(format!("complexity is not supported by {}; ignored.", r.model));
                None
            }
            None => None,
        };
        let format = match r.format.as_deref() {
            Some(f @ ("svg" | "gif")) if !v26 => {
                notes.push(format!("format {f} is not supported by {} (GPT Image 2.6 makes it); ignored, made PNG.", r.model));
                None
            }
            f => f,
        };
        let gif = format == Some("gif");
        let frames = [("frame_count", r.frame_count.map(u32::from)), ("frame_duration_ms", r.frame_duration_ms)].map(|(f, v)| (f, v.filter(|_| gif)));
        let size = r.aspect_ratio.as_deref().map(super::openai_size);
        let background = r.transparent.then_some("transparent");
        let fields =
            [("size", size), ("quality", r.quality.as_deref()), ("background", background), ("complexity", complexity), ("output_format", format)];
        let request = if r.references.is_empty() {
            let mut body = json!({"model": r.model, "prompt": r.prompt, "n": r.n.max(1)});
            for (field, value) in fields {
                if let Some(value) = value {
                    body[field] = value.into();
                }
            }
            for (field, value) in frames {
                if let Some(value) = value {
                    body[field] = value.into();
                }
            }
            call.http.post(format!("{}/v1/images/generations", self.base)).json(&body)
        } else {
            let mut form =
                reqwest::multipart::Form::new().text("model", r.model.clone()).text("prompt", r.prompt.clone()).text("n", r.n.max(1).to_string());
            for (field, value) in fields {
                if let Some(value) = value {
                    form = form.text(field, value.to_owned());
                }
            }
            let part = |reference: &crate::Reference, name: String| {
                reqwest::multipart::Part::bytes(reference.bytes.clone())
                    .file_name(name)
                    .mime_str(&reference.mime)
                    .map_err(|e| MediaError::invalid(e.to_string()))
            };
            for (i, reference) in r.references.iter().enumerate() {
                let ext = if reference.mime == "image/jpeg" { "jpg" } else { "png" };
                form = form.part("image[]", part(reference, format!("reference-{}.{ext}", i + 1))?);
            }
            match &r.markup {
                Some(markup) if v25 || v26 => form = form.part("markup", part(markup, "markup.png".into())?),
                Some(_) => notes.push(format!("markup is not supported by {} (GPT Image 2.5 and 2.6 take it); ignored.", r.model)),
                None => {}
            }
            call.http.post(format!("{}/v1/images/edits", self.base)).multipart(form)
        };
        let value = send(call, self.name(), key, request.bearer_auth(key)).await?;
        let mime = match format {
            Some("svg") => "image/svg+xml".to_owned(),
            Some("jpeg") => "image/jpeg".to_owned(),
            Some(f) => format!("image/{f}"),
            None => "image/png".to_owned(),
        };
        let mut output = openai_output(self.name(), &value, r.model, &mime)?;
        output.notes.extend(notes);
        Ok(output)
    }
    fn audio_types(&self) -> &'static [AudioType] {
        &[AudioType::Speech]
    }
    async fn generate_audio(&self, key: &str, r: AudioRequest, call: &Call) -> Result<Output, MediaError> {
        if r.kind != AudioType::Speech {
            return Err(MediaError::invalid(format!("OpenAI makes speech only, not {}; ElevenLabs makes them", r.kind.noun())));
        }
        if r.text.chars().count() > 4096 {
            return Err(MediaError::invalid("OpenAI speech takes up to 4096 characters; split the text"));
        }
        let model = r.model.unwrap_or_else(|| SPEECH.into());
        let voice = r.voice.map_or_else(|| VOICES[0].0.to_owned(), |v| v.to_lowercase());
        let body = json!({"model": model, "input": r.text, "voice": voice, "response_format": "mp3"});
        let request = call.http.post(format!("{}/v1/audio/speech", self.base)).bearer_auth(key).json(&body);
        let file = http::send_file(call, self.name(), key, request).await?;
        Ok(Output { media: vec![file], model, revised_prompt: None, notes: vec![format!("Voice {voice}.")] })
    }
    async fn voices(&self, _http: &reqwest::Client, _key: Option<&str>) -> Vec<Voice> {
        VOICES.iter().enumerate().map(|(i, (id, about))| Voice { default: i == 0, ..Voice::new(id, id, about) }).collect()
    }
}

/// `data[].b64_json` (OpenAI and xAI), of `mime` unless the bytes or `output_format` say otherwise.
pub(crate) fn openai_output(name: &str, value: &Value, model: String, mime: &str) -> Result<Output, MediaError> {
    let mut output = Output { model, ..Output::default() };
    let mime = match value["output_format"].as_str() {
        Some("svg") => "image/svg+xml".to_owned(),
        Some("jpg" | "jpeg") => "image/jpeg".to_owned(),
        Some(f) => format!("image/{f}"),
        None => mime.to_owned(),
    };
    for item in value["data"].as_array().into_iter().flatten() {
        let Some(data) = item["b64_json"].as_str() else { continue };
        output.media.push(http::decoded(name, data, &mime)?);
        if output.revised_prompt.is_none() {
            output.revised_prompt = item["revised_prompt"].as_str().map(str::to_owned);
        }
    }
    if output.media.is_empty() {
        return Err(MediaError::failed(format!("{name} returned no image")));
    }
    Ok(output)
}
