//! xAI Grok Imagine: images through `POST /v1/images/generations` and `/v1/images/edits` (data-URI references,
//! OpenAI's response shape); videos through `POST /v1/videos/generations`, polled at `GET /v1/videos/{id}`.
use super::openai::openai_output;
use crate::catalog::{EDITS, model};
use crate::http::{self, send};
use crate::poll::{self, Job, Stage};
use crate::{Call, ImageRequest, MediaError, MediaKind, MediaProvider, ModelEntry, Output, VideoRequest};
use serde_json::{Value, json};
use std::time::Duration;

pub struct Xai {
    base: String,
    poll: Duration,
}

impl Default for Xai {
    fn default() -> Self {
        Self::new("https://api.x.ai")
    }
}

impl Xai {
    /// `base` without `/v1`.
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
impl MediaProvider for Xai {
    fn id(&self) -> &'static str {
        "xai"
    }
    fn name(&self) -> &'static str {
        "xAI"
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Image, MediaKind::Video]
    }
    fn models(&self) -> Vec<ModelEntry> {
        vec![
            model(
                MediaKind::Image,
                "grok-imagine-image-2.0",
                "Grok Imagine Image 2.0",
                "Highly aesthetic images; edits with up to 5 references",
                EDITS,
                "1K or 2K",
            )
            .default(),
            model(
                MediaKind::Video,
                "grok-imagine-video-1.5",
                "Grok Imagine Video 1.5",
                "Native 1080p, lip-synced speech; first/last frame and reference images",
                &["text", "first/last frame", "reference images"],
                "1–15 s, 480p to 1080p, sound",
            )
            .default(),
            model(
                MediaKind::Video,
                "grok-imagine-video-1.5-lite",
                "Grok Imagine Video 1.5 Lite",
                "Lowest price per second, for simple videos",
                &["text", "first frame", "reference images"],
                "1–15 s, up to 1080p (upscaled), sound",
            ),
        ]
    }
    fn env(&self) -> &'static [&'static str] {
        &["XAI_API_KEY"]
    }
    fn shares_chat_key(&self) -> bool {
        true
    }
    async fn generate_image(&self, key: &str, r: ImageRequest, call: &Call) -> Result<Output, MediaError> {
        let mut body = json!({"model": r.model, "prompt": r.prompt, "n": r.n.max(1), "response_format": "b64_json"});
        if let Some(aspect) = &r.aspect_ratio {
            body["aspect_ratio"] = aspect.as_str().into();
        }
        // Quality is `low` or `medium`; `high` is the larger resolution.
        match r.quality.as_deref() {
            Some("high") => {
                body["quality"] = "medium".into();
                body["resolution"] = "2k".into();
            }
            Some(quality) => body["quality"] = quality.into(),
            None => {}
        }
        let path = match r.references.as_slice() {
            [] => "generations",
            [one] => {
                body["image"] = json!({"url": one.data_uri(), "type": "image_url"});
                "edits"
            }
            many => {
                body["images"] = many.iter().map(|i| json!({"url": i.data_uri(), "type": "image_url"})).collect();
                "edits"
            }
        };
        let request = call.http.post(format!("{}/v1/images/{path}", self.base)).bearer_auth(key).json(&body);
        let value = send(call, self.name(), key, request).await?;
        let notes = super::unsupported(self.name(), &r);
        let mut output = openai_output(self.name(), &value, r.model, "image/png")?;
        output.notes = notes;
        if r.transparent {
            output.notes.push("xAI can't make transparent backgrounds.".into());
        }
        Ok(output)
    }
    async fn generate_video(&self, key: &str, r: VideoRequest, call: &Call) -> Result<Output, MediaError> {
        let name = self.name();
        let mut notes = Vec::new();
        let mut body = json!({"model": r.model, "prompt": r.prompt});
        if let Some(seconds) = super::seconds(r.duration, (1, 15), &r.model, &mut notes) {
            body["duration"] = seconds.into();
        }
        for (field, value) in [("aspect_ratio", &r.aspect_ratio), ("resolution", &r.resolution)] {
            if let Some(value) = value {
                body[field] = value.as_str().into();
            }
        }
        if r.sound == Some(false) {
            body["generate_audio"] = false.into();
        }
        if let Some(first) = &r.first_frame {
            body["image"] = json!({"url": first.data_uri()});
        }
        if let Some(last) = &r.last_frame {
            body["last_frame"] = json!({"url": last.data_uri()});
        }
        if !r.references.is_empty() {
            body["reference_images"] = r.references.iter().map(|i| json!({"url": i.data_uri()})).collect();
        }
        let request = call.http.post(format!("{}/v1/videos/generations", self.base)).bearer_auth(key).json(&body);
        let submitted = send(call, name, key, request).await?;
        let id = submitted["request_id"].as_str().ok_or_else(|| MediaError::failed("xAI returned no request id"))?.to_owned();
        // No cancel endpoint: a cancelled call just stops waiting.
        let job = Job { name, key, first: self.poll, timeout: poll::VIDEO_TIMEOUT, cancel: None };
        let stage = |value: &Value| match value["status"].as_str() {
            Some("done") => Ok(Stage::Done(value.clone())),
            Some("failed") => Err(MediaError::failed(format!("{name}: {}", value["error"]["message"].as_str().unwrap_or("generation failed")))),
            Some("expired") => Err(MediaError::failed(format!("{name}: the request expired"))),
            _ => Ok(Stage::Waiting(value["progress"].as_u64().map_or_else(String::new, |p| format!("{p} %")))),
        };
        let url = format!("{}/v1/videos/{id}", self.base);
        let done = poll::wait(call, job, json!({"status": "pending"}), stage, || call.http.get(&url).bearer_auth(key)).await?;
        if done["video"]["respect_moderation"] == false {
            return Err(MediaError::failed(format!("{name}: the video was filtered by moderation")));
        }
        let link = done["video"]["url"].as_str().ok_or_else(|| MediaError::failed("xAI returned no video"))?;
        let media = vec![http::download(call, name, link).await?];
        Ok(Output { media, model: done["model"].as_str().unwrap_or(&r.model).to_owned(), revised_prompt: None, notes })
    }
}
