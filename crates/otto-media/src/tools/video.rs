//! `generate_video` (docs/MEDIA.md §6.1).
use super::{Launch, background, choice, choose, images, launch, model, run, seconds, strings, text};
use crate::{Media, MediaKind, VideoRequest};
use otto_tools::{Tool, ToolCx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;

const ASPECTS: &[&str] = &["16:9", "9:16", "1:1"];
const RESOLUTIONS: &[&str] = &["480p", "720p", "1080p"];

pub(crate) struct GenerateVideo {
    media: Arc<Media>,
    spec: ToolSpec,
}

impl GenerateVideo {
    pub(crate) fn new(media: Arc<Media>) -> Self {
        let spec = super::spec(
            "generate_video",
            "Make a video with the user's video provider (Gemini, xAI, Higgsfield, fal). It takes a minute or several. The video is kept with the session: a [generated att_…] line with its size and length (copy_attachment saves it); you don't see it. first_frame/last_frame: images to start or end on (project paths, att_ ids or https links); references: style or character images, where the model takes them; duration: seconds, each model's nearest allowed length (the result says what it made and what it couldn't do); path: also save into the project (replaces what is there). Picking a model: if the user named a model or provider, use it (list_media_models first when the id isn't obvious); video models differ a lot in price, length and look, so when the user didn't say, list_media_models, then pick and say why in one line, or ask_user with 2–4 options when they differ a lot in price or look; for a quick try leave provider and model out for the default. For anything beyond this (a model's own modes and options, video or audio inputs, video edit/extend, motion transfer): media_model_schema, then generate_media. With no provider connected, the user is asked to connect one. It runs in the background by default: the call returns at once; tell the user it's generating and keep talking, don't wait or poll; the result arrives as a message (background: false holds the turn).",
            schema(),
        );
        Self { media, spec }
    }
}

/// Its input schema (also `generate_media`'s for built-in models, §3.2).
pub(crate) fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string", "description": "What happens: subject, motion, camera, light, sound"},
            "first_frame": {"type": "string", "description": "Image to start on: a project path or attachment id (att_…)"},
            "last_frame": {"type": "string", "description": "Image to end on, where the model takes one"},
            "references": {"type": "array", "items": {"type": "string"}, "description": "Style or character images, where the model takes them"},
            "duration": {"type": "number", "description": "Seconds; the model's nearest allowed length"},
            "aspect_ratio": {"type": "string", "enum": ASPECTS},
            "resolution": {"type": "string", "enum": RESOLUTIONS},
            "sound": {"type": "boolean", "description": "With sound, where the model can choose"},
            "path": {"type": "string", "description": "Also save into the project here, relative to the working directory"},
            "provider": {"type": "string", "description": "google, xai, higgsfield or fal; else the default"},
            "model": {"type": "string", "description": "A model handle or id (list_media_models); else the default"},
            "background": {"type": "boolean", "description": "Run as a background job (default true): returns at once, the video arrives as a message"}
        },
        "required": ["prompt"],
        "additionalProperties": false
    })
}

#[async_trait::async_trait]
impl Tool for GenerateVideo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let prompt = text(&input, "prompt").ok_or_else(|| ToolError::invalid("prompt must be a non-empty string"))?.to_owned();
        let aspect_ratio = choice(&input, "aspect_ratio", ASPECTS)?.map(str::to_owned);
        let resolution = choice(&input, "resolution", RESOLUTIONS)?.map(str::to_owned);
        let duration = seconds(&input, "duration")?;
        let sound = match input.get("sound") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_bool().ok_or_else(|| ToolError::invalid("sound must be true or false"))?),
        };
        let background = background(&input, true)?;
        let frames: Vec<String> = ["first_frame", "last_frame"].iter().map(|k| text(&input, k).unwrap_or_default().to_owned()).collect();
        let mut loaded = images(&self.media, &cx, frames.iter().filter(|f| !f.is_empty()).cloned().collect()).await?.into_iter();
        let mut frame = |i: usize| if frames[i].is_empty() { None } else { loaded.next() };
        let (first_frame, last_frame) = (frame(0), frame(1));
        let references = images(&self.media, &cx, strings(&input, "references")?).await?;
        let (provider, key) =
            choose(&self.media, &cx, MediaKind::Video, None, super::requested(&self.media, &input, MediaKind::Video).as_deref()).await?;
        let model = model(&self.media, provider.as_ref(), MediaKind::Video, &input)
            .or_else(|| provider.default_model(MediaKind::Video))
            .ok_or_else(|| ToolError::invalid(format!("{} has no video model; pass model", provider.name())))?;
        let request = VideoRequest {
            prompt: prompt.clone(),
            model: model.clone(),
            first_frame,
            last_frame,
            references,
            duration,
            aspect_ratio: aspect_ratio.clone(),
            resolution,
            sound,
        };
        let what = Launch {
            kind: MediaKind::Video,
            label: format!("generate_video: {prompt}"),
            provider: provider.clone(),
            model,
            name: None,
            extra: json!({"aspect_ratio": aspect_ratio}),
        };
        let (media, path) = (self.media.clone(), text(&input, "path").map(str::to_owned));
        launch(&cx, background, what, move |cx| async move {
            let maker = provider.clone();
            let output = run(&media, &cx, provider.as_ref(), |call| async move { maker.generate_video(&key, request, &call).await }).await?;
            super::result::result(&cx, MediaKind::Video, provider.as_ref(), &prompt, output, path.as_deref()).await
        })
        .await
    }
}
