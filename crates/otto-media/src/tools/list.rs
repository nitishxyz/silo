//! `list_media_models` (docs/MEDIA.md §3.0, §3.2): one line per model, a family's modes under it.
use super::{capitalized, noun, pick};
use crate::models::Model;
use crate::{Media, MediaKind, schema};
use otto_tools::{Effects, Tool, ToolCx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;

/// Live models shown per provider and kind past the built-in ones.
const LIVE: usize = 25;
/// Voices shown per provider.
const VOICES: usize = 60;

pub(crate) struct ListMediaModels {
    media: Arc<Media>,
    spec: ToolSpec,
}

impl ListMediaModels {
    pub(crate) fn new(media: Arc<Media>) -> Self {
        let spec = ToolSpec {
            name: "list_media_models".into(),
            description: "List media models (connected providers or not): handle, name, what each is good at, its modes (text-to-video, image-to-video, reference-to-video, video-edit, motion-transfer…) with the media each takes, price when known, and the default. query narrows by name or ability (\"motion transfer\", \"kling\"); input: models taking that media (a video to edit, an audio track). Use it when the user names a model whose handle isn't obvious, wants something beyond a prompt, or when the choice matters (cost, a style, video) and they didn't say: then pick and say why in one line, or ask_user with 2–4 options when they differ a lot in price or look. Otherwise generate with the default.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["image", "video", "audio"], "description": "What it makes; default: every kind"},
                    "query": {"type": "string", "description": "Words to match in names, descriptions and modes"},
                    "input": {"type": "string", "enum": ["image", "video", "audio"], "description": "Only models taking this media as input"},
                    "provider": {"type": "string", "description": "Only this provider (id or name)"},
                    "refresh": {"type": "boolean", "description": "Fetch the live catalogs again (they are kept 6 h)"}
                },
                "additionalProperties": false
            }),
            output_schema: None,
            effects: Effects { read: true, network: true, ..Effects::NONE },
            sensitive: vec![],
            loading: otto_tools::Loading::Eager,
            mode: otto_tools::CallMode::Sync,
            presentations: vec![],
            source: otto_tools::Source::Builtin,
            exclusive: false,
        };
        Self { media, spec }
    }
}

/// The media a model takes: its modes' media fields, or the built-in path's.
fn takes(model: &Model) -> Vec<schema::Media> {
    let schemas: Vec<Value> =
        if model.modes.is_empty() { vec![pick::quick_schema(model)] } else { model.modes.iter().filter_map(|(_, e)| e.schema.clone()).collect() };
    let mut out: Vec<schema::Media> = schemas.iter().flat_map(|s| schema::fields(s).into_iter().map(|f| f.media)).collect();
    out.sort();
    out.dedup();
    out
}

/// Every word of `query` in the model's handle, name, ids, description or modes.
fn matches(model: &Model, query: &str) -> bool {
    let mut hay = format!("{} {} {} {}", model.handle, model.name, model.id, model.about);
    for (mode, e) in &model.modes {
        hay.push_str(&format!(" {mode} {} {} {}", e.id, e.name, e.operations.join(" ")));
    }
    let hay = hay.to_lowercase().replace(['-', '_', '/'], " ");
    query.to_lowercase().replace(['-', '_', '/'], " ").split_whitespace().all(|w| hay.contains(w))
}

/// `- seedance-2.5 — Seedance 2.5: about. Inputs: …. Modes: text-to-video, image-to-video (image_url, …). Price: ….`
fn line(model: &Model) -> String {
    let mut line = format!("- {}{} — {}", model.handle, if model.default { " (default)" } else { "" }, model.name);
    if !model.about.is_empty() {
        line.push_str(&format!(": {}", model.about.trim_end_matches('.')));
    }
    line.push('.');
    if let Some(entry) = &model.entry {
        if !entry.inputs.is_empty() {
            line.push_str(&format!(" Inputs: {}.", entry.inputs.join(", ")));
        }
        if !entry.outputs.is_empty() {
            line.push_str(&format!(" Outputs: {}.", entry.outputs));
        }
    }
    if !model.modes.is_empty() {
        line.push_str(&format!(" Modes: {}.", super::model_schema::mode_line(model).join(", ")));
    }
    if let Some(price) = model.price() {
        line.push_str(&format!(" Price: {price}."));
    }
    line
}

#[async_trait::async_trait]
impl Tool for ListMediaModels {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, _cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let media_of = |key: &str| -> Result<Option<MediaKind>, ToolError> {
            match input.get(key).and_then(Value::as_str) {
                Some(k) => serde_json::from_value(json!(k)).map(Some).map_err(|_| ToolError::invalid(format!("{key} must be image, video or audio"))),
                None => Ok(None),
            }
        };
        let kinds = media_of("kind")?.map_or_else(|| vec![MediaKind::Image, MediaKind::Video, MediaKind::Audio], |k| vec![k]);
        let wanted = media_of("input")?;
        let only = match super::text(&input, "provider") {
            Some(id) => Some(self.media.provider(id).ok_or_else(|| ToolError::invalid(format!("Unknown media provider {id}")))?),
            None => None,
        };
        let query = super::text(&input, "query");
        self.media.refresh_catalogs(input["refresh"] == true).await;
        let catalog = self.media.catalog();
        let defaults = self.media.defaults();
        let fits = |m: &Model| {
            query.is_none_or(|q| matches(m, q))
                && wanted.is_none_or(|w| takes(m).iter().any(|t| t.noun() == noun(w)))
                && only.as_ref().is_none_or(|o| o.id() == m.provider.id())
        };
        let limit = if query.is_some() || wanted.is_some() { 2 * LIVE } else { LIVE };
        let (mut text, mut listed) = (Vec::new(), Vec::new());
        for kind in kinds {
            let providers: Vec<_> = self
                .media
                .providers()
                .iter()
                .filter(|p| p.kinds().contains(&kind) && only.as_ref().is_none_or(|o| o.id() == p.id()))
                .cloned()
                .collect();
            let mut head = vec![format!("{} models:", capitalized(noun(kind)))];
            if let Some(choice) = defaults.get(&kind).filter(|c| c.provider.is_some()) {
                head.push(format!("Default: {} {}", choice.provider.as_deref().unwrap_or(""), choice.model.as_deref().unwrap_or("(its default)")));
            }
            let mut body = Vec::new();
            for provider in providers {
                let models: Vec<&Model> = catalog.iter().filter(|m| m.kind == kind && m.provider.id() == provider.id() && fits(m)).collect();
                let audio = kind == MediaKind::Audio && query.is_none() && wanted.is_none();
                if models.is_empty() && !audio {
                    continue;
                }
                let connected = self.media.key(provider.as_ref()).is_some();
                body.push(format!("{} ({}) — {}", provider.name(), provider.id(), if connected { "connected" } else { "not connected" }));
                let built_in = models.iter().filter(|m| m.entry.is_some()).count();
                body.extend(models.iter().take(built_in + limit).map(|m| line(m)));
                if models.len() > built_in + limit {
                    body.push(format!("… {} more: pass query or input to narrow.", models.len() - built_in - limit));
                }
                let mut entry = json!({"provider": provider.id(), "kind": kind, "connected": connected, "models": models.iter().map(|m| m.wire()).collect::<Vec<_>>()});
                if audio {
                    let key = self.media.key(provider.as_ref()).map(|(k, _)| k);
                    let voices = provider.voices(&self.media.http, key.as_deref()).await;
                    if !voices.is_empty() {
                        let more = if voices.len() > VOICES { format!(" (first {VOICES} of {})", voices.len()) } else { String::new() };
                        body.push(format!("Voices{more}:"));
                        body.extend(voices.iter().take(VOICES).map(crate::Voice::line));
                        entry["voices"] = json!(voices);
                    }
                }
                listed.push(entry);
            }
            if !body.is_empty() {
                text.extend(head);
                text.extend(body);
            }
        }
        if text.is_empty() {
            return Ok(ToolOutput::text("No media model matches that."));
        }
        text.push("Quick path: generate_image/video/audio {model: <handle>}. Anything else (modes, media inputs, all options): media_model_schema {model, mode}, then generate_media {model, mode, input}.".into());
        Ok(ToolOutput::text(text.join("\n")).with_metadata(json!({"providers": listed})))
    }
}

/// `media.models`: the models `list_media_models` lists, as data (no row limit).
pub(crate) async fn infos(
    media: &Media,
    kind: Option<MediaKind>,
    query: Option<&str>,
    input: Option<MediaKind>,
    provider: Option<&str>,
    refresh: bool,
) -> Result<Vec<otto_proto::MediaModelInfo>, ToolError> {
    let only = match provider.map(str::trim).filter(|p| !p.is_empty()) {
        Some(id) => Some(media.provider(id).ok_or_else(|| ToolError::invalid(format!("Unknown media provider {id}")))?),
        None => None,
    };
    let query = query.map(str::trim).filter(|q| !q.is_empty());
    media.refresh_catalogs(refresh).await;
    let defaults = media.defaults();
    let out = media
        .catalog()
        .into_iter()
        .filter(|m| kind.is_none_or(|k| k == m.kind))
        .filter(|m| query.is_none_or(|q| matches(m, q)))
        .filter(|m| input.is_none_or(|w| takes(m).iter().any(|t| t.noun() == noun(w))))
        .filter(|m| only.as_ref().is_none_or(|o| o.id() == m.provider.id()))
        .map(|m| {
            let chosen = defaults.get(&m.kind).is_some_and(|c| {
                c.provider.as_deref() == Some(m.provider.id()) && c.model.as_deref().is_none_or(|id| m.named(id)) && (c.model.is_some() || m.default)
            });
            otto_proto::MediaModelInfo {
                handle: m.handle.clone(),
                id: m.id.clone(),
                name: m.name.clone(),
                about: m.about.clone(),
                kind: m.kind,
                provider: m.provider.id().into(),
                provider_name: m.provider.name().into(),
                connected: media.key(m.provider.as_ref()).is_some(),
                default: m.default,
                chosen,
                price: m.price().map(str::to_owned),
                inputs: m.entry.as_ref().map(|e| e.inputs.clone()).unwrap_or_default(),
                takes: takes(&m).iter().map(|t| t.noun().to_owned()).collect(),
                modes: m
                    .modes
                    .iter()
                    .map(|(mode, e)| otto_proto::MediaModeInfo {
                        mode: mode.clone(),
                        endpoint: e.id.clone(),
                        price: e.price.clone(),
                        takes: e.schema.as_ref().map(schema::fields).unwrap_or_default().iter().map(|f| f.media.noun().to_owned()).fold(
                            Vec::new(),
                            |mut a, t| {
                                if !a.contains(&t) {
                                    a.push(t);
                                }
                                a
                            },
                        ),
                    })
                    .collect(),
            }
        })
        .collect();
    Ok(out)
}
