//! `generate_media` (docs/MEDIA.md §3.2): any model with its own inputs, checked against its schema; media fields
//! take project paths, `att_…` ids or links, local files uploaded to the provider first.
use super::pick;
use super::{Launch, background, choose, launch, run, text};
use crate::models::Model;
use crate::schema::{self, Media as Field};
use crate::{Media, MediaKind, Reference};
use otto_tools::{Tool, ToolCx, ToolError, ToolOutput, ToolSpec, attachments};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) struct GenerateMedia {
    media: Arc<Media>,
    spec: ToolSpec,
}

impl GenerateMedia {
    pub(crate) fn new(media: Arc<Media>) -> Self {
        let spec = super::spec(
            "generate_media",
            "Run any media model with all its options: video edit and extend, motion transfer, object swap, restyle, multi-reference video (images + videos + audio), keyframes, model-specific settings. Call media_model_schema {model, mode} first and pass input exactly as its schema says. Media fields take project paths, att_ ids (the user's files, earlier results) or https links; local files are uploaded for you. Each result's att_ id can be an input of the next call: chain them (an image → image-to-video → that video into motion transfer). Results come back as [generated att_…] lines like generate_image's; you see images. Videos and music run in the background by default: tell the user and keep talking; the result arrives as a message. With the provider not connected, the user is asked to connect it.",
            json!({
                "type": "object",
                "properties": {
                    "model": {"type": "string", "description": "A handle from list_media_models (seedance-2.5), provider/id or an endpoint id"},
                    "mode": {"type": "string", "description": "One of its modes (reference-to-video, video-edit, motion-transfer…); needed when it has several"},
                    "input": {"type": "object", "description": "The mode's input, as media_model_schema gives it"},
                    "path": {"type": "string", "description": "Also save into the project here, relative to the working directory"},
                    "background": {"type": "boolean", "description": "Run as a background job (default: true for video and music)"}
                },
                "required": ["model", "input"],
                "additionalProperties": false
            }),
        );
        Self { media, spec }
    }
}

#[async_trait::async_trait]
impl Tool for GenerateMedia {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let name = text(&input, "model").ok_or_else(|| ToolError::invalid("model must be a model handle or id"))?;
        let mut body = input.get("input").filter(|v| v.is_object()).cloned().ok_or_else(|| ToolError::invalid("input must be an object"))?;
        let (mut model, mut named) = pick::model(&self.media, name).await?;
        let key = match self.media.key(model.provider.as_ref()) {
            Some((key, _)) => key,
            None => {
                let (provider, key) = choose(&self.media, &cx, model.kind, None, Some(model.provider.id())).await?;
                if provider.id() != model.provider.id() {
                    return Err(ToolError::failed(format!(
                        "{} is a {} model; connect {} to use it",
                        model.name,
                        model.provider.name(),
                        model.provider.name()
                    )));
                }
                // Its live catalog may need the key.
                self.media.refresh(Some(provider.id()), true).await;
                (model, named) = pick::model(&self.media, name).await?;
                key
            }
        };
        let path = text(&input, "path").map(str::to_owned);
        if model.modes.is_empty() {
            return self.quick(&model, body, &input, cx).await;
        }
        let i = match pick::mode(&model, text(&input, "mode").or(named.as_deref()))? {
            Some(i) => i,
            None => fitting(&self.media, &model, &body).await?,
        };
        let endpoint = pick::with_schema(&self.media, &model, i).await?;
        let mode = model.modes[i].0.clone();
        let schema = endpoint.schema.clone().unwrap_or(Value::Null);
        schema::validate(&schema, &body).map_err(|e| {
            ToolError::invalid(format!(
                "{} {mode} input: {e}. media_model_schema {{model: \"{}\", mode: \"{mode}\"}} shows its fields",
                model.name, model.handle
            ))
        })?;
        let files = local_files(&cx, &body, &schema).await?;
        let music = model.kind == MediaKind::Audio && format!("{} {}", endpoint.id, endpoint.name).to_lowercase().contains("music");
        let background = background(&input, model.kind == MediaKind::Video || music)?;
        let prompt = text(&body, "prompt").or_else(|| text(&body, "text")).unwrap_or(&model.name).to_owned();
        let shown = if mode.is_empty() { model.name.clone() } else { format!("{} · {mode}", model.name) };
        let what = Launch {
            kind: model.kind,
            label: format!("generate_media: {prompt}"),
            provider: model.provider.clone(),
            model: endpoint.id.clone(),
            name: Some(shown),
            extra: json!({"mode": mode, "handle": model.handle}),
        };
        let (media, provider, kind) = (self.media.clone(), model.provider.clone(), model.kind);
        launch(&cx, background, what, move |cx| async move {
            let maker = provider.clone();
            let output = run(&media, &cx, provider.as_ref(), |call| async move {
                let mut sent: Vec<(String, String)> = Vec::new();
                for (at, source, file) in files {
                    let link = match sent.iter().find(|(s, _)| *s == source) {
                        Some((_, link)) => link.clone(),
                        None => maker.upload(&key, &file, &call).await?,
                    };
                    sent.push((source, link.clone()));
                    if let Some(slot) = body.pointer_mut(&at) {
                        *slot = Value::String(link);
                    }
                }
                maker.generate(&key, &endpoint.id, body, &call).await
            })
            .await?;
            let mut out = super::result::result(&cx, kind, provider.as_ref(), &prompt, output, path.as_deref()).await?;
            out.metadata["mode"] = json!(mode);
            Ok(out)
        })
        .await
    }
}

impl GenerateMedia {
    /// A built-in model without a live catalog: its input checked, then run by `generate_<kind>`.
    async fn quick(&self, model: &Model, body: Value, input: &Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let schema = pick::quick_schema(model);
        schema::validate(&schema, &body).map_err(|e| {
            ToolError::invalid(format!("{} input: {e}. media_model_schema {{model: \"{}\"}} shows its fields", model.name, model.handle))
        })?;
        let mut call = body;
        call["provider"] = json!(model.provider.id());
        call["model"] = json!(model.id);
        for key in ["path", "background"] {
            if let Some(v) = input.get(key).filter(|v| !v.is_null()) {
                call[key] = v.clone();
            }
        }
        let tool: Box<dyn Tool> = match model.kind {
            MediaKind::Image => Box::new(super::generate::GenerateImage::new(self.media.clone())),
            MediaKind::Video => Box::new(super::video::GenerateVideo::new(self.media.clone())),
            MediaKind::Audio => Box::new(super::audio::GenerateAudio::new(self.media.clone())),
        };
        tool.call(call, cx).await
    }
}

/// No mode named and several: the first whose schema takes `body` (every field known, the required ones there).
async fn fitting(media: &Media, model: &Model, body: &Value) -> Result<usize, ToolError> {
    for i in 0..model.modes.len() {
        let Ok(endpoint) = pick::with_schema(media, model, i).await else { continue };
        let schema = endpoint.schema.unwrap_or(Value::Null);
        let known = body.as_object().into_iter().flatten().all(|(k, _)| schema["properties"].get(k).is_some());
        if known && schema::validate(&schema, body).is_ok() {
            return Ok(i);
        }
    }
    Err(ToolError::invalid(format!(
        "Pick a mode of {}: {}. media_model_schema {{model: \"{}\", mode}} shows each one's fields",
        model.name,
        pick::names(model),
        model.handle
    )))
}

/// The input's media fields holding project paths or `att_` ids: where each is, what it names, and the file read
/// (an image prepared as for references). Links pass as they are.
async fn local_files(cx: &ToolCx, body: &Value, schema: &Value) -> Result<Vec<(String, String, Reference)>, ToolError> {
    let mut wanted: Vec<(String, String, Field)> = schema::locate(body, schema)
        .into_iter()
        .filter_map(|(at, media)| {
            let source = body.pointer(&at)?.as_str()?.trim().to_owned();
            (!source.is_empty() && !super::is_link(&source) && !source.starts_with("data:")).then_some((at, source, media))
        })
        .collect();
    // Uploaded in field order.
    wanted.sort_by(|a, b| a.0.cmp(&b.0));
    let cx = cx.clone();
    tokio::task::spawn_blocking(move || {
        wanted
            .into_iter()
            .map(|(at, source, media)| {
                let bytes = super::save::read(&cx, &source)?;
                let mime = mime_of(&cx, &source, &bytes);
                let field = at.trim_start_matches('/').replace('/', ".");
                let fits = match media {
                    Field::File => true,
                    m => mime.starts_with(&format!("{}/", m.noun())),
                };
                if !fits {
                    return Err(ToolError::invalid(format!(
                        "{field}: {source} is {mime}; this field takes {} {}",
                        article(media.noun()),
                        media.noun()
                    )));
                }
                let file =
                    if media == Field::Image && mime != "image/gif" { super::save::image(&bytes, &source)? } else { Reference { bytes, mime } };
                Ok((at, source, file))
            })
            .collect()
    })
    .await
    .map_err(|e| ToolError::failed(e.to_string()))?
}

/// A file's type: an attachment's recorded one, else its bytes', else `application/octet-stream`.
fn mime_of(cx: &ToolCx, source: &str, bytes: &[u8]) -> String {
    let recorded = || {
        let root = cx.attachments.as_ref()?;
        attachments::is_id(source).then(|| attachments::find(root, &cx.sessions, source).ok()).flatten().map(|(a, _)| a.mime)
    };
    otto_tools::images::sniff(bytes)
        .or_else(|| crate::probe::sniff(bytes))
        .map(str::to_owned)
        .or_else(recorded)
        .unwrap_or_else(|| "application/octet-stream".into())
}

fn article(noun: &str) -> &'static str {
    if noun.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" }
}
