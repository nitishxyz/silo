//! `generate_image` (docs/MEDIA.md §3).
use super::{Launch, background, choice, choose, images, launch, model, run, strings, text};
use crate::{ImageRequest, Media, MediaKind};
use otto_tools::{Tool, ToolCx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;

const ASPECTS: &[&str] = &["1:1", "16:9", "9:16", "4:3", "3:4", "3:2", "2:3"];
const QUALITIES: &[&str] = &["low", "medium", "high"];
const FORMATS: &[&str] = &["png", "jpeg", "webp", "svg", "gif"];

pub(crate) struct GenerateImage {
    media: Arc<Media>,
    spec: ToolSpec,
}

impl GenerateImage {
    pub(crate) fn new(media: Arc<Media>) -> Self {
        let spec = super::spec(
            "generate_image",
            "Make images with the user's image provider (OpenAI, Gemini, xAI, Higgsfield, fal). Each image is kept with the session: a [generated att_…] line per image (read takes the id, copy_attachment saves it), and you see the images to check them. references: images to edit or follow (project paths, att_ ids or https links); markup: a transparent PNG annotation layer (circles, arrows, sketches) over the first reference, for regional edits on an image the user marked up; format: svg for logos, icons and editable vector art, gif for short animated loops (frame_count, frame_duration_ms), both one image without references; complexity: low for focused edits, high (the default) for dense text and layouts. These four are GPT Image 2.6's (markup also 2.5); elsewhere the result says they were ignored. path: also save into the project (replaces what is there; n > 1 adds -1, -2). Picking a model: if the user named a model or provider, use it (list_media_models first when the id isn't obvious); if the choice matters (cost, a specific style) and they didn't say, list_media_models, then pick and say why in one line, or ask_user with 2–4 options when they differ a lot in price or look; otherwise leave provider and model out for the default. For anything beyond this (a model's own modes and options, video or audio inputs, video edit/extend, motion transfer): media_model_schema, then generate_media. With no provider connected, the user is asked to connect one. background: true for slow batches: the call returns at once; tell the user it's generating and keep talking, don't poll; the result arrives as a message.",
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
            "prompt": {"type": "string", "description": "What to make; for edits, what to change"},
            "references": {"type": "array", "items": {"type": "string"}, "description": "Images to edit or follow: project paths or attachment ids (att_…)"},
            "aspect_ratio": {"type": "string", "enum": ASPECTS},
            "quality": {"type": "string", "enum": QUALITIES},
            "transparent": {"type": "boolean", "description": "Transparent background (providers that can)"},
            "complexity": {"type": "string", "enum": QUALITIES, "description": "How much the model plans first; low for focused edits, high (default) for dense text and layouts"},
            "format": {"type": "string", "enum": FORMATS, "description": "svg: editable vector art (logos, icons); gif: a short animated loop. Both: n 1, no references"},
            "frame_count": {"type": "integer", "minimum": 2, "maximum": 64, "description": "gif: frames (default 9)"},
            "frame_duration_ms": {"type": "integer", "minimum": 20, "maximum": 2000, "description": "gif: milliseconds per frame (default 200)"},
            "markup": {"type": "string", "description": "A transparent PNG annotation layer, the first reference's size, marking where to change it: a project path or att_ id"},
            "n": {"type": "integer", "minimum": 1, "maximum": 4, "description": "How many images (default 1)"},
            "path": {"type": "string", "description": "Also save into the project here, relative to the working directory"},
            "provider": {"type": "string", "description": "openai, google, xai, higgsfield or fal; else the default"},
            "model": {"type": "string", "description": "A model handle or id (list_media_models); else the default"},
            "background": {"type": "boolean", "description": "Run as a background job (default false): returns at once, the images arrive as a message"}
        },
        "required": ["prompt"],
        "additionalProperties": false
    })
}

#[async_trait::async_trait]
impl Tool for GenerateImage {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let prompt = text(&input, "prompt").ok_or_else(|| ToolError::invalid("prompt must be a non-empty string"))?.to_owned();
        let aspect_ratio = choice(&input, "aspect_ratio", ASPECTS)?.map(str::to_owned);
        let quality = choice(&input, "quality", QUALITIES)?.map(str::to_owned);
        let n = match input.get("n") {
            None | Some(Value::Null) => 1,
            Some(v) => {
                v.as_u64().and_then(|n| u8::try_from(n).ok()).filter(|n| (1..=4).contains(n)).ok_or_else(|| ToolError::invalid("n must be 1 to 4"))?
            }
        };
        let complexity = choice(&input, "complexity", QUALITIES)?.map(str::to_owned);
        let format = choice(&input, "format", FORMATS)?.map(str::to_owned);
        let int = |key: &str, range: std::ops::RangeInclusive<u64>| match input.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .filter(|n| range.contains(n))
                .map(Some)
                .ok_or_else(|| ToolError::invalid(format!("{key} must be {} to {}", range.start(), range.end()))),
        };
        let frame_count = int("frame_count", 2..=64)?.map(|n| u8::try_from(n).unwrap_or(9));
        let frame_duration_ms = int("frame_duration_ms", 20..=2000)?.map(|n| u32::try_from(n).unwrap_or(200));
        let names = strings(&input, "references")?;
        let transparent = input["transparent"] == true;
        let markup = text(&input, "markup").map(str::to_owned);
        let background = background(&input, false)?;
        match format.as_deref() {
            Some("svg") if n > 1 => return Err(ToolError::invalid("format svg makes one image: n must be 1")),
            Some(f @ ("svg" | "gif")) if !names.is_empty() => {
                return Err(ToolError::invalid(format!("format {f} is for new images only: not with references")));
            }
            Some(f @ ("jpeg" | "svg" | "gif")) if transparent => {
                return Err(ToolError::invalid(format!("transparent needs format png or webp, not {f}")));
            }
            Some("gif") => {}
            _ if frame_count.is_some() || frame_duration_ms.is_some() => {
                return Err(ToolError::invalid("frame_count and frame_duration_ms are for format gif"));
            }
            _ => {}
        }
        let markup = match (markup, names.first()) {
            (Some(_), None) => return Err(ToolError::invalid("markup needs references: it marks up the first one")),
            (Some(_), Some(_)) if transparent => return Err(ToolError::invalid("markup can't be combined with transparent")),
            (Some(raw), Some(first)) => {
                let (mcx, first) = (cx.clone(), first.clone());
                Some(
                    tokio::task::spawn_blocking(move || super::save::markup(&mcx, &raw, &first))
                        .await
                        .map_err(|e| ToolError::failed(e.to_string()))??,
                )
            }
            (None, _) => None,
        };
        let references = images(&self.media, &cx, names).await?;
        let (provider, key) =
            choose(&self.media, &cx, MediaKind::Image, None, super::requested(&self.media, &input, MediaKind::Image).as_deref()).await?;
        let model = model(&self.media, provider.as_ref(), MediaKind::Image, &input)
            .or_else(|| provider.default_model(MediaKind::Image))
            .ok_or_else(|| ToolError::invalid(format!("{} has no image model; pass model", provider.name())))?;
        let request = ImageRequest {
            prompt: prompt.clone(),
            model: model.clone(),
            references,
            aspect_ratio: aspect_ratio.clone(),
            quality,
            transparent,
            n,
            complexity,
            format,
            frame_count,
            frame_duration_ms,
            markup,
        };
        let extra = json!({"n": n, "aspect_ratio": aspect_ratio});
        let what =
            Launch { kind: MediaKind::Image, label: format!("generate_image: {prompt}"), provider: provider.clone(), model, name: None, extra };
        let (media, path) = (self.media.clone(), text(&input, "path").map(str::to_owned));
        launch(&cx, background, what, move |cx| async move {
            let maker = provider.clone();
            let output = run(&media, &cx, provider.as_ref(), |call| async move { maker.generate_image(&key, request, &call).await }).await?;
            super::result::result(&cx, MediaKind::Image, provider.as_ref(), &prompt, output, path.as_deref()).await
        })
        .await
    }
}
