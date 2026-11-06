//! What `media_model_schema` and `generate_media` (docs/MEDIA.md §3.2) run: a model by handle, `provider/id` or
//! endpoint id, one of its modes, and that mode's input schema.
use crate::models::Model;
use crate::{Endpoint, Media, MediaKind};
use otto_tools::{ToolError, ToolErrorKind};
use serde_json::{Value, json};

/// The model `name` names: a handle, an id, an endpoint id, each optionally `provider/…` or `provider:…`;
/// connected providers first. An unknown endpoint id is looked up at providers that look them up (fal).
pub(crate) async fn model(media: &Media, name: &str) -> Result<(Model, Option<String>), ToolError> {
    let name = name.trim();
    media.refresh_catalogs(false).await;
    let (only, rest) = match name.split_once(['/', ':']).filter(|(p, _)| media.provider(p).is_some_and(|m| m.id().eq_ignore_ascii_case(p))) {
        Some((p, rest)) => (media.provider(p), rest),
        None => (None, name),
    };
    let find = || {
        let mut found: Vec<Model> =
            media.catalog().into_iter().filter(|m| only.as_ref().is_none_or(|o| o.id() == m.provider.id()) && m.named(rest)).collect();
        found.sort_by_key(|m| media.key(m.provider.as_ref()).is_none());
        found.into_iter().next()
    };
    let model = match find() {
        Some(model) => model,
        None if rest.contains('/') => {
            let providers: Vec<_> = media.providers().iter().filter(|p| only.as_ref().is_none_or(|o| o.id() == p.id())).cloned().collect();
            let mut looked = None;
            for p in providers {
                let key = media.key(p.as_ref()).map(|(k, _)| k);
                if let Some(endpoint) = p.endpoint(&media.http, key.as_deref(), rest).await {
                    media.put_endpoint(p.id(), endpoint);
                    looked = find();
                    break;
                }
            }
            looked.ok_or_else(|| unknown(name))?
        }
        None => return Err(unknown(name)),
    };
    let endpoint = model.modes.iter().find(|(_, e)| e.id.eq_ignore_ascii_case(rest)).map(|(m, _)| m.clone());
    Ok((model, endpoint))
}

fn unknown(name: &str) -> ToolError {
    ToolError::new(ToolErrorKind::NotFound, format!("No media model {name}; list_media_models lists them"))
}

/// The mode `wanted` names (its name, a unique part of it, or its endpoint id), else the only one; `None` when
/// the model has several and none was named.
pub(crate) fn mode(model: &Model, wanted: Option<&str>) -> Result<Option<usize>, ToolError> {
    let Some(wanted) = wanted.map(str::trim).filter(|w| !w.is_empty()) else {
        return Ok((model.modes.len() == 1).then_some(0));
    };
    let modes = &model.modes;
    let exact = modes.iter().position(|(m, e)| m.eq_ignore_ascii_case(wanted) || e.id.eq_ignore_ascii_case(wanted));
    let parts: Vec<usize> = (0..modes.len()).filter(|&i| modes[i].0.to_lowercase().contains(&wanted.to_lowercase())).collect();
    match (exact, parts.as_slice()) {
        (Some(i), _) | (None, &[i]) => Ok(Some(i)),
        _ => Err(ToolError::invalid(format!("{} has no mode {wanted}; its modes: {}", model.name, names(model)))),
    }
}

pub(crate) fn names(model: &Model) -> String {
    model.modes.iter().map(|(m, e)| if m.is_empty() { e.id.as_str() } else { m.as_str() }).collect::<Vec<_>>().join(", ")
}

/// Mode `i`'s endpoint with its schema, fetched (and cached) when the listing had none.
pub(crate) async fn with_schema(media: &Media, model: &Model, i: usize) -> Result<Endpoint, ToolError> {
    let (_, endpoint) = &model.modes[i];
    if endpoint.schema.is_some() {
        return Ok(endpoint.clone());
    }
    let key = media.key(model.provider.as_ref()).map(|(k, _)| k);
    let fetched = model.provider.endpoint(&media.http, key.as_deref(), &endpoint.id).await.filter(|e| e.schema.is_some());
    let fetched = fetched.ok_or_else(|| ToolError::failed(format!("Can't read {}'s input schema from {}", endpoint.id, model.provider.name())))?;
    media.put_endpoint(model.provider.id(), fetched.clone());
    Ok(fetched)
}

/// The built-in path's schema: `generate_<kind>`'s without its routing fields, media fields marked, and only the
/// provider's options (GPT Image 2.6's for OpenAI; its audio types).
pub(crate) fn quick_schema(model: &Model) -> Value {
    let mut schema = match model.kind {
        MediaKind::Image => super::generate::schema(),
        MediaKind::Video => super::video::schema(),
        MediaKind::Audio => super::audio::schema(),
    };
    let props = schema["properties"].as_object_mut().expect("an object schema");
    for key in ["provider", "model", "path", "background"] {
        props.remove(key);
    }
    if model.provider.id() != "openai" {
        for key in ["complexity", "format", "frame_count", "frame_duration_ms", "markup"] {
            props.remove(key);
        }
    }
    for key in ["first_frame", "last_frame", "markup"] {
        if let Some(field) = props.get_mut(key) {
            field["format"] = json!("uri");
        }
    }
    if let Some(field) = props.get_mut("references") {
        field["items"]["format"] = json!("uri");
    }
    if model.kind == MediaKind::Audio {
        let types: Vec<&str> = model.provider.audio_types().iter().map(|t| super::audio::type_name(*t)).collect();
        props["type"]["enum"] = json!(types);
    }
    schema
}
