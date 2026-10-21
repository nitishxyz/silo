//! Canonical transcript to Responses input.
use crate::{CallKind, Effort, Media, MediaSource, Part, Reasoning, Request, ResultPart, Role, ToolChoice, ToolKind};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Normalize OpenAI strict object schemas, retaining optionality through null.
pub fn strictify(schema: &mut Value) {
    if let Value::Object(object) = schema {
        let required = object.get("required").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(Value::Object(properties)) = object.get_mut("properties") {
            for (name, property) in properties.iter_mut() {
                strictify(property);
                if !required.contains(&json!(name)) {
                    *property = json!({"anyOf": [property.clone(), {"type": "null"}]});
                }
            }
            let names: Vec<_> = properties.keys().cloned().collect();
            object.insert("required".into(), json!(names));
            object.insert("additionalProperties".into(), json!(false));
        } else if object.get("type") == Some(&json!("object")) {
            object.insert("additionalProperties".into(), json!(false));
        }
        for (key, child) in object {
            if key != "properties" && key != "required" {
                strictify(child);
            }
        }
    } else if let Value::Array(array) = schema {
        for child in array {
            strictify(child);
        }
    }
}

pub(super) fn stateless_reasoning(body: &mut Value) {
    if body["store"] != false {
        return;
    }
    // An output inclusion, not an effort setting: models without reasoning emit no such items.
    if !body["include"].is_array() {
        body["include"] = json!([]);
    }
    let include = body["include"].as_array_mut().expect("include array");
    let encrypted = json!("reasoning.encrypted_content");
    if !include.contains(&encrypted) {
        include.push(encrypted);
    }
    // A summary or server-side id alone cannot restore reasoning in a stateless continuation.
    if let Some(input) = body["input"].as_array_mut() {
        input.retain(|item| item["type"] != "reasoning" || item["encrypted_content"].as_str().is_some_and(|s| !s.is_empty()));
    }
}
pub fn merge(target: &mut Value, source: &Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            merge(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    } else {
        *target = source.clone();
    }
}
#[must_use]
pub fn effort(reasoning: Reasoning) -> &'static str {
    match reasoning {
        Reasoning::Effort(e) => match e {
            Effort::None => "none",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh | Effort::Max => "xhigh",
        },
        Reasoning::Budget(n) => match n {
            0 => "none",
            1..=1024 => "minimal",
            1025..=4096 => "low",
            4097..=16384 => "medium",
            16385..=32768 => "high",
            _ => "xhigh",
        },
    }
}
fn media(media: &Media, image: bool) -> Value {
    let mut value = json!({"type": if image { "input_image" } else { "input_file" }});
    match &media.source {
        MediaSource::Base64 { data } => {
            value[if image { "image_url" } else { "file_data" }] = json!(format!("data:{};base64,{data}", media.mime));
        }
        MediaSource::Url { url } => {
            value[if image { "image_url" } else { "file_url" }] = json!(url);
        }
        MediaSource::File { id, .. } => {
            value["file_id"] = json!(id);
        }
    }
    if image {
        value["detail"] = json!("auto");
    } else if let Some(name) = &media.name {
        value["filename"] = json!(name);
    }
    value
}
#[must_use]
pub fn request(req: &Request) -> Value {
    let mut input = Vec::new();
    let mut kinds = HashMap::new();
    for item in &req.items {
        for part in &item.parts {
            let role = match item.role {
                Role::System => "developer",
                Role::User | Role::Tool => "user",
                Role::Assistant => "assistant",
            };
            let content = match part {
                Part::Text { text, .. } | Part::System { text } => {
                    Some(json!({"type": if item.role == Role::Assistant { "output_text" } else { "input_text" }, "text":text}))
                }
                Part::Image(m) => Some(media(m, true)),
                Part::Document(m) => Some(media(m, false)),
                _ => None,
            };
            if let Some(content) = content {
                input.push(json!({"type":"message","role":role,"content":[content]}));
                continue;
            }
            match part {
                Part::Reasoning { opaque: Some(o), .. } | Part::Compaction { opaque: Some(o), .. } if o.matches_provider("openai") => {
                    input.push(o.data.clone())
                }
                Part::ToolCall { call_id, name, input: args, kind, opaque } => {
                    kinds.insert(call_id.as_str(), *kind);
                    match kind {
                        CallKind::Function => input.push(json!({"type":"function_call","call_id":call_id,"name":name,"arguments":args.to_string()})),
                        CallKind::Custom => {
                            input.push(json!({"type":"custom_tool_call","call_id":call_id,"name":name,"input":args.as_str().unwrap_or_default()}))
                        }
                        CallKind::Hosted => {
                            if let Some(o) = opaque.as_ref().filter(|o| o.matches_provider("openai")) {
                                input.push(o.data.clone());
                            }
                        }
                    }
                }
                Part::ToolResult { call_id, content, .. } => {
                    let content: Vec<_> = content
                        .iter()
                        .map(|p| match p {
                            ResultPart::Text { text } => json!({"type":"input_text","text":text}),
                            ResultPart::Image(m) => media(m, true),
                            ResultPart::Document(m) => media(m, false),
                        })
                        .collect();
                    input.push(json!({"type":if kinds.get(call_id.as_str()) == Some(&CallKind::Custom) { "custom_tool_call_output" } else { "function_call_output" }, "call_id":call_id,"output":content}));
                }
                _ => {}
            }
        }
    }
    let tools: Vec<_> = req
        .tools
        .iter()
        .map(|tool| match &tool.kind {
            ToolKind::Function | ToolKind::Strict => {
                let strict = tool.kind == ToolKind::Strict;
                let mut schema = tool.input_schema.clone();
                if strict {
                    strictify(&mut schema);
                }
                json!({"type":"function","name":tool.name,"description":tool.description,"parameters":schema,"strict":strict})
            }
            ToolKind::Custom { format } => {
                let mut value = json!({"type":"custom","name":tool.name,"description":tool.description});
                if let Some(format) = format {
                    value["format"] = format.clone();
                }
                value
            }
            ToolKind::Hosted { tool, config } => {
                let mut value = json!({"type":tool});
                if config.is_object() {
                    merge(&mut value, config);
                }
                value
            }
        })
        .collect();
    let choice = match &req.tool_choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool(name) => {
            json!({"type":if req.tools.iter().any(|t| &t.name == name && matches!(t.kind, ToolKind::Custom { .. })) { "custom" } else { "function" },"name":name})
        }
    };
    let mut body = json!({"model":req.model,"input":input,"instructions":req.system.join("\n\n"),"stream":true,"store":false,"tools":tools,"tool_choice":choice});
    for (key, value) in [
        ("max_output_tokens", json!(req.max_output)),
        ("temperature", json!(req.temperature)),
        ("top_p", json!(req.top_p)),
        ("parallel_tool_calls", json!(req.parallel_tools)),
        ("user", json!(req.user)),
    ] {
        if !value.is_null() {
            body[key] = value;
        }
    }
    if let Some(reasoning) = req.reasoning {
        body["reasoning"] = json!({"effort":effort(reasoning),"summary":"auto"});
    }
    if !req.cache.disabled
        && let Some(key) = &req.cache.key
    {
        body["prompt_cache_key"] = json!(key);
    }
    if let Some(schema) = &req.output_schema {
        let mut schema = schema.clone();
        strictify(&mut schema);
        body["text"] = json!({"format":{"type":"json_schema","name":"response","strict":true,"schema":schema}});
    }
    if let Some(tier) = req.provider.openai.as_ref().and_then(|options| options.service_tier.as_ref()) {
        body["service_tier"] = json!(tier);
    }
    if let Some(extra) = &req.extra {
        merge(&mut body, extra);
    }
    stateless_reasoning(&mut body);
    body
}
