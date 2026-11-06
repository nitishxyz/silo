//! Gemini through the Interactions API (`POST /v1beta/interactions`): images (`response_format: {type: image}`)
//! and Gemini Omni videos, run in the background and polled at `GET /v1beta/interactions/{id}`.
use crate::catalog::{EDITS, model};
use crate::http::{self, send};
use crate::poll::{self, Job, Stage};
use crate::{Call, Generated, ImageRequest, MediaError, MediaKind, MediaProvider, ModelEntry, Output, VideoRequest};
use serde_json::{Value, json};
use std::time::Duration;

pub struct Google {
    base: String,
    poll: Duration,
}

impl Default for Google {
    fn default() -> Self {
        Self::new("https://generativelanguage.googleapis.com/v1beta")
    }
}

impl Google {
    /// `base` with the API version (`…/v1beta`).
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into().trim_end_matches('/').to_owned(), poll: super::POLL }
    }
    /// The first wait between a video's status checks.
    #[must_use]
    pub fn polling(mut self, first: Duration) -> Self {
        self.poll = first;
        self
    }
}

#[async_trait::async_trait]
impl MediaProvider for Google {
    fn id(&self) -> &'static str {
        "google"
    }
    fn name(&self) -> &'static str {
        "Gemini"
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Image, MediaKind::Video]
    }
    fn models(&self) -> Vec<ModelEntry> {
        let out = "1K, 2K or 4K; 14 aspect ratios";
        vec![
            model(
                MediaKind::Image,
                "gemini-nano-banana-2.1",
                "Nano Banana 2.1",
                "Fast workhorse: quality, text rendering, consistent multi-image edits (up to 14 references)",
                EDITS,
                out,
            )
            .default(),
            model(
                MediaKind::Image,
                "gemini-3-pro-image",
                "Nano Banana Pro",
                "Thinks first: complex instructions, brand consistency, high-fidelity text",
                EDITS,
                out,
            ),
            model(MediaKind::Image, "gemini-3.1-flash-lite-image", "Nano Banana 2 Lite", "Fastest and cheapest; few references", EDITS, "1K"),
            model(
                MediaKind::Video,
                "gemini-omni-1.1-flash",
                "Gemini Omni Flash",
                "Fast video with sound, world knowledge, readable text; first/last frame and subject references",
                &["text", "first/last frame", "reference images"],
                "16:9 or 9:16; 360p, 720p, 1080p or 4K (upscaled); sound; length from the prompt",
            )
            .default(),
        ]
    }
    fn env(&self) -> &'static [&'static str] {
        &["GEMINI_API_KEY", "GOOGLE_API_KEY"]
    }
    fn shares_chat_key(&self) -> bool {
        true
    }
    async fn generate_image(&self, key: &str, r: ImageRequest, call: &Call) -> Result<Output, MediaError> {
        let mut input = vec![json!({"type": "text", "text": r.prompt})];
        input.extend(r.references.iter().map(|i| json!({"type": "image", "data": otto_tools::attachments::encode(&i.bytes), "mime_type": i.mime})));
        let mut format = json!({"type": "image"});
        if let Some(aspect) = &r.aspect_ratio {
            format["aspect_ratio"] = aspect.as_str().into();
        }
        if r.quality.as_deref() == Some("high") {
            format["image_size"] = "2K".into();
        }
        let body = json!({"model": r.model, "input": input, "response_format": format});
        let url = format!("{}/interactions", self.base);
        // One image a request.
        let media = http::times(r.n, || async {
            let value = send(call, self.name(), key, call.http.post(&url).header("x-goog-api-key", key).json(&body)).await?;
            images(self.name(), &value)
        })
        .await?;
        let mut notes = super::unsupported(self.name(), &r);
        if r.transparent {
            notes.push("Gemini can't make transparent backgrounds.".into());
        }
        Ok(Output { media, model: r.model, revised_prompt: None, notes })
    }
    async fn generate_video(&self, key: &str, r: VideoRequest, call: &Call) -> Result<Output, MediaError> {
        self.video(key, r, call).await
    }
}

impl Google {
    /// A video interaction: frames first, then references, then the prompt; in the background, polled.
    async fn video(&self, key: &str, r: VideoRequest, call: &Call) -> Result<Output, MediaError> {
        let name = self.name();
        let mut notes = Vec::new();
        let image = |i: &crate::Reference| json!({"type": "image", "data": otto_tools::attachments::encode(&i.bytes), "mime_type": i.mime});
        let mut input: Vec<Value> = r.first_frame.iter().chain(&r.last_frame).chain(&r.references).map(image).collect();
        let mut prompt = r.prompt.clone();
        if let Some(seconds) = r.duration {
            // No length setting: Omni takes it from the prompt.
            prompt.push_str(&format!("\n\nThe video is {} seconds long.", seconds.round()));
            notes.push("Gemini takes the length from the prompt; it may differ.".into());
        }
        input.push(json!({"type": "text", "text": prompt}));
        let mut format = json!({"type": "video"});
        match r.aspect_ratio.as_deref() {
            Some(aspect @ ("16:9" | "9:16")) => format["aspect_ratio"] = aspect.into(),
            Some(aspect) => notes.push(format!("Gemini makes 16:9 or 9:16 videos, not {aspect}: made 16:9.")),
            None => {}
        }
        match r.resolution.as_deref() {
            Some("480p") => notes.push("Gemini has no 480p: made 720p.".into()),
            Some(resolution) => format["resolution"] = resolution.into(),
            None => {}
        }
        if r.sound == Some(false) {
            notes.push("Gemini always adds sound; ask for silence in the prompt.".into());
        }
        let mut body = json!({"model": r.model, "input": input, "response_format": format, "background": true});
        let task = match (r.first_frame.is_some() || r.last_frame.is_some(), r.references.is_empty()) {
            (true, _) => Some("image_to_video"),
            (false, false) => Some("reference_to_video"),
            (false, true) => None,
        };
        if let Some(task) = task {
            body["generation_config"] = json!({"video_config": {"task": task}});
        }
        let submitted =
            send(call, name, key, call.http.post(format!("{}/interactions", self.base)).header("x-goog-api-key", key).json(&body)).await?;
        let id = submitted["id"].as_str().unwrap_or_default().to_owned();
        let url = format!("{}/interactions/{id}", self.base);
        let cancel = (!id.is_empty()).then(|| call.http.post(format!("{url}/cancel")).header("x-goog-api-key", key));
        let job = Job { name, key, first: self.poll, timeout: poll::VIDEO_TIMEOUT, cancel };
        let stage = |value: &Value| match value["status"].as_str() {
            Some("completed") => Ok(Stage::Done(value.clone())),
            Some("failed") => Err(MediaError::failed(format!("{name}: {}", value["error"]["message"].as_str().unwrap_or("generation failed")))),
            Some("cancelled") => Err(MediaError::failed(format!("{name}: the request was cancelled"))),
            Some("requires_action") => Err(MediaError::failed(format!("{name}: the interaction asked for input this can't give"))),
            _ if id.is_empty() => Err(MediaError::failed(format!("{name} returned no interaction id"))),
            _ => Ok(Stage::Waiting(String::new())),
        };
        let done = poll::wait(call, job, submitted.clone(), stage, || call.http.get(&url).header("x-goog-api-key", key)).await?;
        let mut media = Vec::new();
        for block in outputs(&done).filter(|c| c["type"] == "video") {
            let mime = block["mime_type"].as_str().unwrap_or("video/mp4");
            if let Some(data) = block["data"].as_str() {
                media.push(http::decoded(name, data, mime)?);
            } else if let Some(uri) = block["uri"].as_str() {
                media.push(http::fetch(call, name, call.http.get(uri).header("x-goog-api-key", key)).await?);
            }
        }
        if media.is_empty() {
            return Err(MediaError::failed(nothing(name, "video", &done)));
        }
        Ok(Output { media, model: r.model, revised_prompt: None, notes })
    }
}

/// The content blocks of an interaction's `model_output` steps.
fn outputs(value: &Value) -> impl Iterator<Item = &Value> {
    value["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["type"] == "model_output")
        .flat_map(|s| s["content"].as_array().into_iter().flatten())
}

/// `Gemini returned no <what>`, with what the model said instead.
fn nothing(name: &str, what: &str, value: &Value) -> String {
    let said: Vec<&str> = value["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|s| s["content"].as_array().into_iter().flatten())
        .filter_map(|c| c["text"].as_str())
        .collect();
    let said = said.join(" ");
    let said = said.trim();
    if said.is_empty() { format!("{name} returned no {what}") } else { format!("{name} returned no {what}: {said}") }
}

/// The images of an interaction's `model_output` steps.
pub(crate) fn images(name: &str, value: &Value) -> Result<Vec<Generated>, MediaError> {
    if value["status"] == "failed" {
        let message = value["error"]["message"].as_str().unwrap_or("generation failed");
        return Err(MediaError::failed(format!("{name}: {message}")));
    }
    let mut media = Vec::new();
    for block in outputs(value).filter(|c| c["type"] == "image") {
        if let Some(data) = block["data"].as_str() {
            media.push(http::decoded(name, data, block["mime_type"].as_str().unwrap_or("image/png"))?);
        }
    }
    if media.is_empty() {
        return Err(MediaError::failed(nothing(name, "image", value)));
    }
    Ok(media)
}
