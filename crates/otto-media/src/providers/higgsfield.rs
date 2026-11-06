//! Higgsfield: one key (`id:secret`) for 50+ image and video models. Asynchronous: submit to
//! `POST /<model>`, poll the status URL ([`Higgsfield::wait`]), download the links.
use crate::catalog::{EDITS, TEXT, model};
use crate::http::{self, send};
use crate::poll::{self, Job, Stage};
use crate::{Call, Endpoint, ImageRequest, MediaError, MediaKind, MediaProvider, ModelEntry, Output, Reference, VideoRequest};
use serde_json::{Value, json};
use std::time::Duration;

pub struct Higgsfield {
    base: String,
    /// The first wait between status checks; it grows to 10 s.
    poll: Duration,
}

impl Default for Higgsfield {
    fn default() -> Self {
        Self::new("https://api.higgsfield.ai", Duration::from_secs(2))
    }
}

impl Higgsfield {
    #[must_use]
    pub fn new(base: impl Into<String>, poll: Duration) -> Self {
        Self { base: base.into().trim_end_matches('/').to_owned(), poll }
    }
}

const FRAMES: &[&str] = &["text", "first/last frame"];
const ALL: &[&str] = &["text", "first/last frame", "reference images"];

const SOUL: &str = "higgsfield-ai/soul/v2/standard";
const GROK: &str = "xai/grok-imagine-image-2.0";

/// The model that takes references in place of `model`.
fn edit_model(model: &str) -> &str {
    match model {
        m if m.starts_with("higgsfield-ai/soul") => GROK,
        "alibaba/qwen-image-3/text-to-image" => "alibaba/qwen-image-3/edit",
        m => m,
    }
}

/// `model` (`…/text-to-video`) as its sibling `workflow` endpoint; other ids as they are.
fn workflow(model: &str, workflow: &str) -> String {
    model
        .rsplit_once('/')
        .filter(|(_, last)| matches!(*last, "text-to-video" | "image-to-video" | "reference-to-video"))
        .map_or_else(|| model.to_owned(), |(family, _)| format!("{family}/{workflow}"))
}

/// How a video model takes its sound setting.
enum Sound {
    /// `sound: on|off` (Kling).
    OnOff,
    /// `generate_audio: bool`.
    Flag,
    /// None to set.
    Own,
}

/// What a video model family takes (from Higgsfield's model docs).
struct Family {
    name: &'static str,
    seconds: Option<(u32, u32)>,
    /// `aspect_ratio` with a first frame too.
    frame_aspect: bool,
    resolution: bool,
    sound: Sound,
    /// The last frame's field.
    last: &'static str,
    /// Has a `reference-to-video` workflow taking `image_urls`.
    references: bool,
}

impl Family {
    fn of(model: &str) -> Self {
        let family = |name, seconds, frame_aspect, resolution, sound, last, references| Self {
            name,
            seconds: Some(seconds),
            frame_aspect,
            resolution,
            sound,
            last,
            references,
        };
        match model {
            m if m.starts_with("kling-video/v3.0") => family("Kling 3.0", (3, 15), false, false, Sound::OnOff, "last_image_url", false),
            m if m.starts_with("bytedance/seedance-2.5/") => family("Seedance 2.5", (4, 30), false, true, Sound::Flag, "end_image_url", true),
            m if m.starts_with("alibaba/wan-3.0/") => family("Wan 3.0", (2, 30), true, true, Sound::Flag, "end_image_url", true),
            m if m.starts_with("minimax/h3/") => family("MiniMax H3", (5, 15), true, false, Sound::Own, "end_image_url", true),
            _ => Self {
                name: "This model",
                seconds: None,
                frame_aspect: true,
                resolution: true,
                sound: Sound::Flag,
                last: "end_image_url",
                references: true,
            },
        }
    }
}

#[async_trait::async_trait]
impl MediaProvider for Higgsfield {
    fn id(&self) -> &'static str {
        "higgsfield"
    }
    fn name(&self) -> &'static str {
        "Higgsfield"
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Image, MediaKind::Video]
    }
    fn models(&self) -> Vec<ModelEntry> {
        let (image, video) = (MediaKind::Image, MediaKind::Video);
        vec![
            model(
                image,
                SOUL,
                "Soul 2",
                "Portraits, fashion and editorial photos in Soul styles; references go to Grok Image 2.0",
                TEXT,
                "720p or 1080p, 7 aspect ratios",
            )
            .default(),
            model(image, "higgsfield-ai/soul/cinema", "Soul Cinema", "Cinema-inspired stills", TEXT, ""),
            model(image, GROK, "Grok Image 2.0", "Generate and edit with up to 10 references", EDITS, "1K or 2K"),
            model(image, "alibaba/qwen-image-3/text-to-image", "Qwen Image 3", "Complex text rendering; edits 1–3 references", EDITS, "up to 2K"),
            model(image, "ideogram/v4.0", "Ideogram 4.0", "Posters and logos with accurate text", &["text", "one reference image"], ""),
            model(image, "recraft/v4.1/text-to-image", "Recraft V4.1", "Design work with palette and background controls", TEXT, "1K"),
            model(image, "z-image/turbo", "Z-Image Turbo", "Fast and cheap", TEXT, "1K or 2K"),
            model(
                video,
                "kling-video/v3.0/pro/text-to-video",
                "Kling 3.0 Pro",
                "Cinematic motion, strong prompt following",
                FRAMES,
                "3–15 s; 16:9, 9:16 or 1:1; optional sound",
            )
            .default(),
            model(video, "kling-video/v3.0/std/text-to-video", "Kling 3.0 Standard", "Cheaper Kling 3.0", FRAMES, "3–15 s, optional sound"),
            model(
                video,
                "bytedance/seedance-2.5/text-to-video",
                "Seedance 2.5",
                "Generate, edit and extend with multimodal references",
                ALL,
                "4–30 s, 480p to 1080p, sound",
            ),
            model(video, "alibaba/wan-3.0/text-to-video", "Wan 3.0", "Text, frames or multimodal references", ALL, "2–30 s, up to 1080p, sound"),
            model(video, "minimax/h3/text-to-video", "MiniMax H3", "Text, frames or multimodal references", ALL, "5–15 s, 2K"),
        ]
    }
    fn env(&self) -> &'static [&'static str] {
        &["HF_CREDENTIALS", "HF_API_KEY_ID", "HF_API_KEY_SECRET"]
    }
    /// `HF_CREDENTIALS` (`id:secret`), else `HF_API_KEY_ID` and `HF_API_KEY_SECRET`.
    fn env_key(&self, env: &(dyn Fn(&str) -> Option<String> + Send + Sync)) -> Option<String> {
        let set = |name: &str| env(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        set("HF_CREDENTIALS")
            .filter(|k| self.check_key(k).is_ok())
            .or_else(|| Some(format!("{}:{}", set("HF_API_KEY_ID")?, set("HF_API_KEY_SECRET")?)))
    }
    fn check_key(&self, key: &str) -> Result<(), MediaError> {
        match key.split_once(':') {
            Some((id, secret)) if !id.trim().is_empty() && !secret.trim().is_empty() => Ok(()),
            _ => Err(MediaError::invalid("A Higgsfield key is its ID and secret as id:secret")),
        }
    }
    async fn generate_image(&self, key: &str, r: ImageRequest, call: &Call) -> Result<Output, MediaError> {
        let model = if r.references.is_empty() { r.model.as_str() } else { edit_model(&r.model) }.to_owned();
        let mut body = json!({"prompt": r.prompt});
        if let Some(aspect) = &r.aspect_ratio {
            body["aspect_ratio"] = aspect.as_str().into();
        }
        match (model.as_str(), r.quality.as_deref()) {
            (m, Some("high")) if m.starts_with("higgsfield-ai/soul") => body["resolution"] = "1080p".into(),
            (GROK, Some("high")) => body["resolution"] = "2k".into(),
            (GROK, Some(quality)) => body["quality"] = quality.into(),
            _ => {}
        }
        if !r.references.is_empty() {
            let mut urls = Vec::new();
            for reference in &r.references {
                urls.push(Value::String(self.store(key, reference, call).await?));
            }
            body["image_urls"] = Value::Array(urls);
        }
        let media = http::times(r.n, || async {
            let value = self.run(key, &model, &body, call, Duration::from_secs(900)).await?;
            let mut media = Vec::new();
            for link in value["images"].as_array().into_iter().flatten().filter_map(|i| i["url"].as_str()) {
                media.push(http::download(call, self.name(), link).await?);
            }
            if media.is_empty() {
                return Err(MediaError::failed("Higgsfield returned no image"));
            }
            Ok(media)
        })
        .await?;
        let mut notes = super::unsupported(self.name(), &r);
        if r.transparent {
            notes.push("Higgsfield can't make transparent backgrounds.".into());
        }
        Ok(Output { media, model, revised_prompt: None, notes })
    }
    async fn generate_video(&self, key: &str, r: VideoRequest, call: &Call) -> Result<Output, MediaError> {
        let family = Family::of(&r.model);
        let mut notes = Vec::new();
        let frames = r.first_frame.is_some() || r.last_frame.is_some();
        let model = match (frames, r.references.is_empty()) {
            (true, _) => workflow(&r.model, "image-to-video"),
            (false, false) if family.references => workflow(&r.model, "reference-to-video"),
            _ => r.model.clone(),
        };
        let mut body = json!({"prompt": r.prompt});
        if let Some(seconds) = r.duration {
            body["duration"] = match family.seconds {
                Some(range) => super::seconds(Some(seconds), range, family.name, &mut notes).into(),
                None => super::seconds(Some(seconds), (1, 60), family.name, &mut notes).into(),
            };
        }
        match (&r.aspect_ratio, frames && !family.frame_aspect) {
            (Some(_), true) => notes.push(format!("{} follows the first frame's shape.", family.name)),
            (Some(aspect), false) => body["aspect_ratio"] = aspect.as_str().into(),
            (None, _) => {}
        }
        match (&r.resolution, family.resolution) {
            (Some(resolution), true) => body["resolution"] = resolution.as_str().into(),
            (Some(_), false) => notes.push(format!("{} picks its own resolution.", family.name)),
            (None, _) => {}
        }
        match (r.sound, family.sound) {
            (Some(on), Sound::OnOff) => body["sound"] = if on { "on" } else { "off" }.into(),
            (Some(on), Sound::Flag) => body["generate_audio"] = on.into(),
            (Some(_), Sound::Own) => notes.push(format!("{} sets its own sound.", family.name)),
            (None, _) => {}
        }
        match (&r.first_frame, &r.last_frame) {
            (Some(first), last) => {
                body["image_url"] = self.store(key, first, call).await?.into();
                if let Some(last) = last {
                    body[family.last] = self.store(key, last, call).await?.into();
                }
            }
            (None, Some(_)) => return Err(MediaError::invalid("Higgsfield needs first_frame to use last_frame")),
            (None, None) => {}
        }
        if !r.references.is_empty() {
            if frames || !family.references {
                notes.push(format!("{model} takes no reference images; they weren't sent."));
            } else {
                let mut urls = Vec::new();
                for reference in &r.references {
                    urls.push(Value::String(self.store(key, reference, call).await?));
                }
                body["image_urls"] = Value::Array(urls);
            }
        }
        let value = self.run(key, &model, &body, call, poll::VIDEO_TIMEOUT).await?;
        let link = value["video"]["url"].as_str().ok_or_else(|| MediaError::failed("Higgsfield returned no video"))?;
        let media = vec![http::download(call, self.name(), link).await?];
        Ok(Output { media, model, revised_prompt: None, notes })
    }
    /// `GET /models?include_schema=true`: every model with its input schema (a key is needed).
    async fn endpoints(&self, http: &reqwest::Client, key: Option<&str>) -> Option<Vec<Endpoint>> {
        let request = http.get(format!("{}/models?include_schema=true", self.base)).header("Authorization", format!("Key {}", key?));
        let response = request.timeout(Duration::from_secs(20)).send().await.ok()?;
        let endpoints = catalog(&response.error_for_status().ok()?.json().await.ok()?);
        (!endpoints.is_empty()).then_some(endpoints)
    }
    async fn upload(&self, key: &str, file: &Reference, call: &Call) -> Result<String, MediaError> {
        self.store(key, file, call).await
    }
    async fn generate(&self, key: &str, id: &str, input: Value, call: &Call) -> Result<Output, MediaError> {
        let done = self.run(key, id, &input, call, poll::VIDEO_TIMEOUT).await?;
        let media = http::outputs(call, self.name(), &done).await?;
        Ok(Output { media, model: id.to_owned(), revised_prompt: None, notes: vec![] })
    }
}

/// A `GET /models` listing as endpoints: `{items: [{slug, title, description, operation_type, output_type,
/// base_credits, input_schema}]}`.
pub(crate) fn catalog(value: &Value) -> Vec<Endpoint> {
    let kind = |t: &str| match t {
        "image" => Some(MediaKind::Image),
        "video" => Some(MediaKind::Video),
        "audio" => Some(MediaKind::Audio),
        _ => None,
    };
    let items = value["items"].as_array().into_iter().flatten();
    items
        .filter_map(|m| {
            let id = m["slug"].as_str()?.to_owned();
            let about = m["description"].as_str().unwrap_or("").split(". ").next().unwrap_or("").trim().trim_end_matches('.').to_owned();
            let credits = m["base_credits"].as_str().and_then(|c| c.parse::<f64>().ok()).or_else(|| m["base_credits"].as_f64());
            Some(Endpoint {
                name: m["title"].as_str().unwrap_or(&id).to_owned(),
                about,
                kind: kind(m["output_type"].as_str()?)?,
                group: None,
                operations: m["operation_type"].as_array().into_iter().flatten().filter_map(|o| o.as_str().map(str::to_owned)).collect(),
                price: credits.filter(|c| *c > 0.0).map(|c| format!("{c} credits")),
                schema: m["input_schema"].as_object().map(|s| crate::schema::trim(&Value::Object(s.clone()), &Value::Null)),
                id,
            })
        })
        .collect()
}

impl Higgsfield {
    /// Submit `body` to `model` and wait for it: the completed request (`images`, `video`, `audio`).
    async fn run(&self, key: &str, model: &str, body: &Value, call: &Call, timeout: Duration) -> Result<Value, MediaError> {
        let request = call
            .http
            .post(format!("{}/{model}", self.base))
            .header("Authorization", format!("Key {key}"))
            .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
            .json(body);
        let submitted = send(call, self.name(), key, request).await?;
        self.wait(key, submitted, call, timeout).await
    }

    /// Poll a submitted request until it ends ([`poll::wait`]); a cancel withdraws it while queued.
    pub(crate) async fn wait(&self, key: &str, value: Value, call: &Call, timeout: Duration) -> Result<Value, MediaError> {
        let name = self.name();
        let id = value["request_id"].as_str().unwrap_or_default().to_owned();
        let status_url = value["status_url"].as_str().map_or_else(|| format!("{}/requests/{id}/status", self.base), str::to_owned);
        let cancel_url = value["cancel_url"].as_str().map_or_else(|| format!("{}/requests/{id}/cancel", self.base), str::to_owned);
        let auth = format!("Key {key}");
        let job = Job { name, key, first: self.poll, timeout, cancel: Some(call.http.post(&cancel_url).header("Authorization", &auth)) };
        let stage = |value: &Value| match value["status"].as_str().unwrap_or("queued") {
            "completed" => Ok(Stage::Done(value.clone())),
            "failed" => {
                let message = value["error"].as_str().or_else(|| value["error"]["message"].as_str()).unwrap_or("generation failed");
                Err(MediaError::failed(http::scrub(&format!("{name}: {message}"), key)))
            }
            "nsfw" => Err(MediaError::failed(format!("{name}: rejected by content moderation"))),
            "canceled" => Err(MediaError::failed(format!("{name}: the request was canceled"))),
            status => Ok(Stage::Waiting(status.replace('_', " "))),
        };
        poll::wait(call, job, value, stage, || call.http.get(&status_url).header("Authorization", &auth)).await
    }

    /// `file` in Higgsfield's storage (models take links, not bytes): its public URL.
    async fn store(&self, key: &str, file: &Reference, call: &Call) -> Result<String, MediaError> {
        let request = call
            .http
            .post(format!("{}/files/generate-upload-url", self.base))
            .header("Authorization", format!("Key {key}"))
            .json(&json!({"content_type": file.mime}));
        let slot = send(call, self.name(), key, request).await?;
        let (Some(upload), Some(public)) = (slot["upload_url"].as_str(), slot["public_url"].as_str()) else {
            return Err(MediaError::failed("Higgsfield gave no upload link"));
        };
        let mut put = call.http.put(upload).body(file.bytes.clone());
        for (header, value) in slot["upload_headers"].as_object().into_iter().flatten() {
            put = put.header(header.as_str(), value.as_str().unwrap_or_default());
        }
        let response = tokio::select! {
            r = put.send() => r.map_err(|_| MediaError::failed("Higgsfield: the upload failed"))?,
            () = call.cancel.cancelled() => return Err(MediaError::cancelled()),
        };
        if !response.status().is_success() {
            return Err(MediaError::failed(format!("Higgsfield: the upload failed (HTTP {})", response.status().as_u16())));
        }
        Ok(public.to_owned())
    }
}
