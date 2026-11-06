//! fal: images through `POST https://fal.run/<model>` with `Authorization: Key …` (links in `images[].url`);
//! videos through the queue (`POST https://queue.fal.run/<model>`, poll `status_url`, read `response_url`).
use crate::catalog::{EDITS, TEXT, model};
use crate::http::{self, send};
use crate::poll::{self, Job, Stage};
use crate::{Call, Endpoint, ImageRequest, MediaError, MediaKind, MediaProvider, ModelEntry, Output, Reference, VideoRequest};
use serde_json::{Value, json};
use std::time::Duration;

pub struct Fal {
    run: String,
    queue: String,
    api: String,
    rest: String,
    poll: Duration,
}

impl Default for Fal {
    fn default() -> Self {
        Self::new("https://fal.run", "https://queue.fal.run", "https://api.fal.ai")
    }
}

impl Fal {
    /// `run` takes images, `queue` videos and `generate_media`, `api` lists models and their schemas; uploads go to
    /// `rest.fal.ai` ([`Self::storage`]).
    #[must_use]
    pub fn new(run: impl Into<String>, queue: impl Into<String>, api: impl Into<String>) -> Self {
        let trim = |s: String| s.trim_end_matches('/').to_owned();
        let rest = "https://rest.fal.ai".into();
        Self { run: trim(run.into()), queue: trim(queue.into()), api: trim(api.into()), rest, poll: super::POLL }
    }
    /// Where files are uploaded (`POST /storage/upload/initiate`).
    #[must_use]
    pub fn storage(mut self, rest: impl Into<String>) -> Self {
        self.rest = rest.into().trim_end_matches('/').to_owned();
        self
    }
    /// The first wait between a video's status checks.
    #[must_use]
    pub fn polling(mut self, first: Duration) -> Self {
        self.poll = first;
        self
    }
}

const KLING: &str = "kling/v4/text-to-video";

/// The edit endpoint of a text-to-image model, for references.
fn edit_model(model: &str) -> String {
    match model {
        "blackforestlabs/flux-3/text-to-image" => "blackforestlabs/flux-3/edit-image".into(),
        "blackforestlabs/flux-3/klein/text-to-image" => "blackforestlabs/flux-3/klein/edit-image".into(),
        "google/nano-banana-2.1" => "google/nano-banana-2.1/edit".into(),
        _ => model.strip_suffix("/text-to-image").map_or_else(|| model.to_owned(), |m| format!("{m}/edit")),
    }
}

#[async_trait::async_trait]
impl MediaProvider for Fal {
    fn id(&self) -> &'static str {
        "fal"
    }
    fn name(&self) -> &'static str {
        "fal"
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Image, MediaKind::Video]
    }
    fn models(&self) -> Vec<ModelEntry> {
        let image = MediaKind::Image;
        vec![
            model(
                image,
                "blackforestlabs/flux-3/text-to-image",
                "FLUX 3",
                "Detailed, well-composed images, precise layout, good text; edits through flux-3/edit-image",
                EDITS,
                "1K to 4K, 15 aspect ratios",
            )
            .default(),
            model(image, "blackforestlabs/flux-3/klein/text-to-image", "FLUX.3 Klein", "Fast and cheap FLUX", EDITS, "configurable size"),
            model(image, "krea/v3/text-to-image", "Krea 3", "Aesthetic, photographic looks", TEXT, "configurable size"),
            model(
                image,
                "bytedance/seedream/v5/pro/text-to-image",
                "Seedream 5.0 Pro",
                "Dense layouts and structured designs, text in 14 languages",
                EDITS,
                "up to 4K",
            ),
            model(image, "ideogram/v4.5", "Ideogram 4.5", "Posters and logos with accurate text", TEXT, "low, medium or high quality"),
            model(image, "fal-ai/recraft/v4.1/text-to-image", "Recraft V4.1", "Brand systems and editorial design", TEXT, "raster"),
            model(image, "google/nano-banana-2.1", "Nano Banana 2.1", "Google's workhorse model through fal", EDITS, "up to 4K"),
            model(
                MediaKind::Video,
                KLING,
                "Kling 4.0",
                "Kling's newest: cinematic motion, multi-shot; frames through kling/v4/image-to-video",
                &["text", "first/last frame"],
                "3–30 s; 720p or 1080p; 16:9, 9:16, 1:1 or 21:9",
            )
            .default(),
            model(MediaKind::Video, "fal-ai/kling-video/v3/pro/text-to-video", "Kling 3.0 Pro", "Cinematic visuals, native audio", TEXT, "sound"),
            model(MediaKind::Video, "minimax/h3/text-to-video", "MiniMax H3", "Frontier video model", TEXT, ""),
            model(
                MediaKind::Video,
                "bytedance/seedance-2.5/us/text-to-video",
                "Seedance 2.5",
                "Cinematic video with synchronized audio",
                TEXT,
                "sound",
            ),
        ]
    }
    fn env(&self) -> &'static [&'static str] {
        &["FAL_KEY"]
    }
    /// `GET {api}/v1/models?category=…` for each media category (public), 40 each.
    async fn endpoints(&self, http: &reqwest::Client, key: Option<&str>) -> Option<Vec<Endpoint>> {
        let lists = futures::future::join_all(
            CATEGORIES.iter().map(|category| self.listing(http, key, format!("category={category}&status=active&limit=40"))),
        )
        .await;
        let mut endpoints: Vec<Endpoint> = Vec::new();
        for (_, e) in lists.into_iter().flatten().flatten() {
            if !endpoints.iter().any(|o| o.id == e.id) {
                endpoints.push(e);
            }
        }
        (!endpoints.is_empty()).then_some(endpoints)
    }
    /// `GET {api}/v1/models?endpoint_id=…&expand=openapi-3.0`: the endpoint with its input schema.
    async fn endpoint(&self, http: &reqwest::Client, key: Option<&str>, id: &str) -> Option<Endpoint> {
        let query = format!("endpoint_id={}&expand=openapi-3.0", urlencode(id));
        let (raw, mut endpoint) = self.listing(http, key, query).await?.into_iter().find(|(_, e)| e.id == id)?;
        let openapi = &raw["openapi"];
        let input = &openapi["paths"][format!("/{id}")]["post"]["requestBody"]["content"]["application/json"]["schema"];
        endpoint.schema = Some(crate::schema::trim(input, openapi)).filter(|s| s["properties"].is_object());
        Some(endpoint)
    }
    /// fal storage: `POST {rest}/storage/upload/initiate` names the file, a `PUT` sends it, its `file_url` is the link.
    async fn upload(&self, key: &str, file: &Reference, call: &Call) -> Result<String, MediaError> {
        let name = format!("input.{}", file.mime.rsplit('/').next().unwrap_or("bin"));
        let request = call
            .http
            .post(format!("{}/storage/upload/initiate?storage_type=fal-cdn-v3", self.rest))
            .header("Authorization", format!("Key {key}"))
            .json(&json!({"content_type": file.mime, "file_name": name}));
        let slot = send(call, self.name(), key, request).await?;
        let (Some(upload), Some(link)) = (slot["upload_url"].as_str(), slot["file_url"].as_str()) else {
            return Err(MediaError::failed("fal gave no upload link"));
        };
        let put = call.http.put(upload).header("Content-Type", &file.mime).body(file.bytes.clone());
        let response = tokio::select! {
            r = put.send() => r.map_err(|_| MediaError::failed("fal: the upload failed"))?,
            () = call.cancel.cancelled() => return Err(MediaError::cancelled()),
        };
        if !response.status().is_success() {
            return Err(MediaError::failed(format!("fal: the upload failed (HTTP {})", response.status().as_u16())));
        }
        Ok(link.to_owned())
    }
    async fn generate(&self, key: &str, id: &str, input: Value, call: &Call) -> Result<Output, MediaError> {
        let result = self.queued(key, id, &input, call).await?;
        let media = http::outputs(call, self.name(), &result).await?;
        Ok(Output { media, model: id.to_owned(), revised_prompt: None, notes: vec![] })
    }
    async fn generate_image(&self, key: &str, r: ImageRequest, call: &Call) -> Result<Output, MediaError> {
        let model = if r.references.is_empty() { r.model.clone() } else { edit_model(&r.model) };
        let mut body = json!({"prompt": r.prompt, "output_format": "png"});
        if let Some(aspect) = &r.aspect_ratio {
            body["aspect_ratio"] = aspect.as_str().into();
        }
        if !r.references.is_empty() {
            body["image_urls"] = r.references.iter().map(crate::Reference::data_uri).collect();
        }
        let url = format!("{}/{model}", self.run);
        let auth = format!("Key {key}");
        let media = http::times(r.n, || async {
            let value = send(call, self.name(), key, call.http.post(&url).header("Authorization", &auth).json(&body)).await?;
            let mut media = Vec::new();
            for image in value["images"].as_array().into_iter().flatten() {
                let link = image["url"].as_str().ok_or_else(|| MediaError::failed("fal returned an image without a link"))?;
                media.push(http::download(call, self.name(), link).await?);
            }
            if media.is_empty() {
                return Err(MediaError::failed("fal returned no image"));
            }
            Ok(media)
        })
        .await?;
        let mut notes = super::unsupported(self.name(), &r);
        if r.transparent {
            notes.push(format!("{model} on fal may not make transparent backgrounds."));
        }
        Ok(Output { media, model, revised_prompt: None, notes })
    }
    async fn generate_video(&self, key: &str, r: VideoRequest, call: &Call) -> Result<Output, MediaError> {
        let mut notes = Vec::new();
        let frames = r.first_frame.is_some() || r.last_frame.is_some();
        let model = if frames { r.model.replace("/text-to-video", "/image-to-video") } else { r.model.clone() };
        let kling = model.starts_with("kling/v4/");
        let mut body = json!({"prompt": r.prompt});
        let range = if kling { (3, 30) } else { (1, 60) };
        if let Some(seconds) = super::seconds(r.duration, range, &model, &mut notes) {
            body["duration"] = seconds.into();
        }
        for (field, value) in [("aspect_ratio", &r.aspect_ratio), ("resolution", &r.resolution)] {
            if let Some(value) = value {
                body[field] = value.as_str().into();
            }
        }
        match (r.sound, kling) {
            (Some(_), true) => notes.push("Kling 4.0 on fal sets its own sound.".into()),
            (Some(on), false) => body["generate_audio"] = on.into(),
            (None, _) => {}
        }
        // Kling's frames are start/end images; most others' image/end image.
        let (first, last) = if model.contains("kling") { ("start_image_url", "end_image_url") } else { ("image_url", "end_image_url") };
        for (field, frame) in [(first, &r.first_frame), (last, &r.last_frame)] {
            if let Some(frame) = frame {
                body[field] = frame.data_uri().into();
            }
        }
        if !r.references.is_empty() {
            if model.ends_with("/reference-to-video") {
                body["image_urls"] = r.references.iter().map(crate::Reference::data_uri).collect();
            } else {
                notes.push(format!("{model} takes no reference images; they weren't sent (a …/reference-to-video model does)."));
            }
        }
        let result = self.queued(key, &model, &body, call).await?;
        let video = result["video"]["url"].as_str().ok_or_else(|| MediaError::failed("fal returned no video"))?;
        let media = vec![http::download(call, self.name(), video).await?];
        Ok(Output { media, model, revised_prompt: None, notes })
    }
}

/// The media categories listed, and what each makes.
const CATEGORIES: &[&str] =
    &["text-to-image", "image-to-image", "text-to-video", "image-to-video", "video-to-video", "text-to-audio", "text-to-speech"];

fn category_kind(category: &str) -> Option<MediaKind> {
    let made = category.rsplit("-to-").next()?;
    match made {
        "image" => Some(MediaKind::Image),
        "video" => Some(MediaKind::Video),
        "audio" | "speech" | "music" => Some(MediaKind::Audio),
        _ => None,
    }
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_./~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

impl Fal {
    /// `GET {api}/v1/models?<query>`: each media model with its raw entry.
    async fn listing(&self, http: &reqwest::Client, key: Option<&str>, query: String) -> Option<Vec<(Value, Endpoint)>> {
        let mut request = http.get(format!("{}/v1/models?{query}", self.api));
        if let Some(key) = key {
            request = request.header("Authorization", format!("Key {key}"));
        }
        let response = request.timeout(Duration::from_secs(20)).send().await.ok()?;
        let value: Value = response.error_for_status().ok()?.json().await.ok()?;
        let models = value["models"].as_array()?.iter().filter_map(|m| {
            let id = m["endpoint_id"].as_str()?.to_owned();
            let meta = &m["metadata"];
            let about = meta["description"].as_str().unwrap_or("").split(". ").next().unwrap_or("").trim().trim_end_matches('.').to_owned();
            let endpoint = Endpoint {
                name: meta["display_name"].as_str().unwrap_or(&id).to_owned(),
                about,
                kind: category_kind(meta["category"].as_str()?)?,
                group: meta["group"]["key"].as_str().map(str::to_owned),
                operations: meta["category"].as_str().map(str::to_owned).into_iter().collect(),
                price: None,
                schema: None,
                id,
            };
            Some((m.clone(), endpoint))
        });
        Some(models.collect())
    }

    /// `body` through the queue: submit, poll `status_url`, read `response_url`; a cancel withdraws it.
    async fn queued(&self, key: &str, model: &str, body: &Value, call: &Call) -> Result<Value, MediaError> {
        let name = self.name();
        let auth = format!("Key {key}");
        let submitted = send(call, name, key, call.http.post(format!("{}/{model}", self.queue)).header("Authorization", &auth).json(body)).await?;
        let link = |field: &str| submitted[field].as_str().map(str::to_owned).ok_or_else(|| MediaError::failed(format!("fal returned no {field}")));
        let (status_url, response_url, cancel_url) = (link("status_url")?, link("response_url")?, link("cancel_url")?);
        let job = Job {
            name,
            key,
            first: self.poll,
            timeout: poll::VIDEO_TIMEOUT,
            cancel: Some(call.http.put(&cancel_url).header("Authorization", &auth)),
        };
        let stage = |value: &Value| match value["status"].as_str() {
            Some("COMPLETED") => match value["error"].as_str() {
                Some(error) => Err(MediaError::failed(format!("{name}: {error}"))),
                None => Ok(Stage::Done(value.clone())),
            },
            Some("IN_PROGRESS") => Ok(Stage::Waiting("in progress".into())),
            _ => Ok(Stage::Waiting(match value["queue_position"].as_u64() {
                Some(0) | None => "queued".into(),
                Some(n) => format!("queued, {n} ahead"),
            })),
        };
        let first_status = json!({"status": "IN_QUEUE", "queue_position": submitted["queue_position"]});
        poll::wait(call, job, first_status, stage, || call.http.get(&status_url).header("Authorization", &auth)).await?;
        send(call, name, key, call.http.get(&response_url).header("Authorization", &auth)).await
    }
}
