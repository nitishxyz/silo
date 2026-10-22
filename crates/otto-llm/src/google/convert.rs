//! Canonical requests to Interactions steps. `store=false`: every request
//! carries the full history, with Gemini's own steps (thought signatures,
//! function calls, server tool steps) replayed byte-exact.
use crate::{CallKind, Effort, ErrorClass, LlmError, Media, MediaSource, Part, Reasoning, Request, ResultPart, Role, ToolChoice, ToolKind};
use serde_json::{Value, json};
use std::collections::HashMap;

pub const PROVIDER: &str = "google";

fn media(media: &Media) -> Result<Value, LlmError> {
    let kind = match media.mime.split('/').next() {
        Some("image") => "image",
        Some("audio") => "audio",
        Some("video") => "video",
        _ => "document",
    };
    let mut value = json!({"type":kind,"mime_type":media.mime});
    match &media.source {
        MediaSource::Base64 { data } => value["data"] = json!(data),
        MediaSource::Url { url } => value["uri"] = json!(url),
        MediaSource::File { provider, id } if provider.as_str() == PROVIDER => value["uri"] = json!(id),
        MediaSource::File { .. } => return Err(LlmError::new(ErrorClass::InvalidRequest, "cannot send another provider's file to Gemini")),
    }
    Ok(value)
}

#[must_use]
pub fn thinking_level(reasoning: Reasoning) -> &'static str {
    match reasoning {
        Reasoning::Effort(Effort::None | Effort::Minimal) | Reasoning::Budget(0..=1024) => "minimal",
        Reasoning::Effort(Effort::Low) | Reasoning::Budget(1025..=4096) => "low",
        Reasoning::Effort(Effort::Medium) | Reasoning::Budget(4097..=16384) => "medium",
        _ => "high",
    }
}

/// Interactions input steps for `items`.
///
/// # Errors
/// Media this API cannot take.
pub fn steps(items: &[crate::Item]) -> Result<Vec<Value>, LlmError> {
    let mut steps = Vec::new();
    let mut names: HashMap<&str, &str> = HashMap::new();
    for item in items {
        let mut content = Vec::new();
        let flush = |content: &mut Vec<Value>, steps: &mut Vec<Value>, kind: &str| {
            if !content.is_empty() {
                steps.push(json!({"type":kind,"content":std::mem::take(content)}));
            }
        };
        let text_kind = if item.role == Role::Assistant { "model_output" } else { "user_input" };
        for part in &item.parts {
            let own = |o: &Option<crate::Opaque>| o.as_ref().filter(|o| o.matches_provider(PROVIDER)).map(|o| o.data.clone());
            match part {
                Part::Text { text, .. } => content.push(json!({"type":"text","text":text})),
                Part::System { text } => content.push(json!({"type":"text","text":format!("<system-reminder>\n{text}\n</system-reminder>")})),
                Part::ToolsChanged { added, removed } => {
                    content.push(json!({"type":"text","text":format!("Tools added: {}; removed: {}", added.join(", "), removed.join(", "))}))
                }
                Part::Image(m) | Part::Document(m) | Part::Audio(m) => content.push(media(m)?),
                // Another provider's compaction: its readable summary, if any.
                Part::Compaction { summary: Some(text), .. } => content.push(json!({"type":"text","text":text})),
                Part::Reasoning { opaque, .. } => {
                    if let Some(step) = own(opaque) {
                        flush(&mut content, &mut steps, text_kind);
                        steps.push(step);
                    }
                }
                Part::ToolCall { call_id, name, input, kind, opaque } => {
                    names.insert(call_id.as_str(), name.as_str());
                    flush(&mut content, &mut steps, text_kind);
                    match (own(opaque), kind) {
                        (Some(step), _) => steps.push(step),
                        (None, CallKind::Hosted) => {}
                        (None, _) => steps.push(json!({"type":"function_call","id":call_id,"name":name,"arguments":input})),
                    }
                }
                Part::HostedResult { opaque, payload, tool, .. } => match own(opaque) {
                    Some(step) => {
                        flush(&mut content, &mut steps, text_kind);
                        steps.push(step);
                    }
                    None => content.push(json!({"type":"text","text":format!("{tool} result: {payload}")})),
                },
                Part::ToolResult { call_id, content: result, is_error } => {
                    flush(&mut content, &mut steps, text_kind);
                    let result = result
                        .iter()
                        .map(|p| match p {
                            ResultPart::Text { text } => Ok(json!({"type":"text","text":text})),
                            ResultPart::Image(m) | ResultPart::Document(m) => media(m),
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut step = json!({"type":"function_result","call_id":call_id,"result":result});
                    if let Some(name) = names.get(call_id.as_str()) {
                        step["name"] = json!(name);
                    }
                    if *is_error {
                        step["is_error"] = json!(true);
                    }
                    steps.push(step);
                }
                Part::Compaction { .. } => {}
            }
        }
        flush(&mut content, &mut steps, text_kind);
    }
    Ok(steps)
}

/// The `POST /v1beta/interactions` body.
///
/// # Errors
/// Freeform custom tools and unsupported media.
pub fn request(req: &Request) -> Result<Value, LlmError> {
    let tools = req
        .tools
        .iter()
        .map(|tool| match &tool.kind {
            ToolKind::Function | ToolKind::Strict => {
                Ok(json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.input_schema}))
            }
            ToolKind::Hosted { tool, config } | ToolKind::Native { tool, config } => {
                let mut value = json!({"type":tool});
                if config.is_object() {
                    crate::openai::convert::merge(&mut value, config);
                }
                Ok(value)
            }
            ToolKind::Custom { .. } => Err(LlmError::new(ErrorClass::InvalidRequest, "Gemini does not support freeform custom tools")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut body = json!({"model":req.model,"input":steps(&req.items)?,"stream":true,"store":false});
    if !req.system.is_empty() {
        body["system_instruction"] = json!(req.system.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = match &req.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("any"),
            ToolChoice::Tool(name) => json!({"allowed_tools":{"mode":"any","tools":[name]}}),
        };
    }
    let mut config = json!({});
    if let Some(n) = req.max_output {
        config["max_output_tokens"] = json!(n);
    }
    if !req.stop.is_empty() {
        config["stop_sequences"] = json!(req.stop);
    }
    if let Some(reasoning) = req.reasoning {
        config["thinking_level"] = json!(thinking_level(reasoning));
        config["thinking_summaries"] = json!("auto");
    }
    if config.as_object().is_some_and(|c| !c.is_empty()) {
        body["generation_config"] = config;
    }
    if let Some(schema) = &req.output_schema {
        body["response_format"] = json!({"type":"text","mime_type":"application/json","schema":schema});
    }
    if let Some(extra) = &req.extra {
        crate::openai::convert::merge(&mut body, extra);
    }
    Ok(body)
}
