//! `media_model_schema` (docs/MEDIA.md §3.2): a model's modes with their input schemas, read-only.
use super::pick;
use super::text;
use crate::models::Model;
use crate::{Media, schema};
use otto_tools::{Effects, Tool, ToolCx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;

/// Modes shown with their schemas when none is named; more are only listed.
const SHOWN: usize = 6;

pub(crate) struct MediaModelSchema {
    media: Arc<Media>,
    spec: ToolSpec,
}

impl MediaModelSchema {
    pub(crate) fn new(media: Arc<Media>) -> Self {
        let spec = ToolSpec {
            name: "media_model_schema".into(),
            description: "A media model's full input (JSON Schema) for generate_media, per mode: every option it has, which fields are media (they take project paths, att_ ids or https links), required fields, enums and limits. Call it before generate_media whenever the request needs more than a prompt.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "model": {"type": "string", "description": "A handle from list_media_models, provider/id or an endpoint id"},
                    "mode": {"type": "string", "description": "One mode; default: all of them"}
                },
                "required": ["model"],
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

#[async_trait::async_trait]
impl Tool for MediaModelSchema {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, _cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let name = text(&input, "model").ok_or_else(|| ToolError::invalid("model must be a model handle or id"))?;
        let (model, named) = pick::model(&self.media, name).await?;
        let connected = if self.media.key(model.provider.as_ref()).is_some() { "connected" } else { "not connected" };
        let mut text = vec![format!(
            "{} ({}) · {} {} ({connected}) · makes {}",
            model.name,
            model.handle,
            model.provider.name(),
            model.provider.id(),
            super::noun(model.kind)
        )];
        let call = format!("generate_media {{model: \"{}\"", model.handle);
        if model.modes.is_empty() {
            text.push(format!("Run: {call}, input}}."));
            text.extend(described(&pick::quick_schema(&model)));
            return Ok(ToolOutput::text(text.join("\n")));
        }
        let picked: Vec<usize> = match pick::mode(&model, text_or(&input, named.as_deref()))? {
            Some(i) => vec![i],
            None => (0..model.modes.len()).collect(),
        };
        if picked.len() > 1 {
            text.push(format!("Modes: {}. Pass mode to generate_media.", pick::names(&model)));
        }
        for &i in picked.iter().take(SHOWN) {
            let endpoint = pick::with_schema(&self.media, &model, i).await?;
            let mode = &model.modes[i].0;
            let price = endpoint.price.as_deref().map_or_else(String::new, |p| format!(" · {p}"));
            text.push(String::new());
            text.push(format!("Mode {} — endpoint {}{price}. Run: {call}, mode: \"{mode}\", input}}.", shown(mode, &endpoint.id), endpoint.id));
            text.extend(described(endpoint.schema.as_ref().unwrap_or(&Value::Null)));
        }
        if picked.len() > SHOWN {
            text.push(format!("{} more modes: ask for one by mode.", picked.len() - SHOWN));
        }
        Ok(ToolOutput::text(text.join("\n")).with_metadata(json!({"model": model.handle, "provider": model.provider.id()})))
    }
}

fn text_or<'a>(input: &'a Value, named: Option<&'a str>) -> Option<&'a str> {
    text(input, "mode").or(named)
}

fn shown<'a>(mode: &'a str, id: &'a str) -> &'a str {
    if mode.is_empty() { id } else { mode }
}

/// A schema's media fields, then the schema itself (compact).
fn described(schema: &Value) -> Vec<String> {
    let fields = schema::fields(schema);
    let mut out = Vec::new();
    if !fields.is_empty() {
        let list: Vec<String> = fields.iter().map(|f| format!("{} ({})", f.short(), f.media.noun())).collect();
        out.push(format!("Media fields (project paths, att_ ids or https links): {}", list.join(", ")));
    }
    out.push(format!("Input schema: {schema}"));
    out
}

/// One line per mode for `list_media_models`: `reference-to-video (image_urls ≤9?, video_urls ≤3?)`.
pub(crate) fn mode_line(model: &Model) -> Vec<String> {
    model
        .modes
        .iter()
        .map(|(mode, e)| {
            let fields: Vec<String> = e.schema.as_ref().map(schema::fields).unwrap_or_default().iter().map(schema::Field::short).collect();
            let name = shown(mode, &e.id);
            if fields.is_empty() { name.to_owned() } else { format!("{name} ({})", fields.join(", ")) }
        })
        .collect()
}
