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
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
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
/// `reasoning` as `provider`'s `model` takes it: an effort the catalog doesn't
/// list for it becomes the nearest listed one (no `max` before GPT-5.6, no
/// `xhigh` before GPT-5.2, …). Unlisted models get the common set: no `max`,
/// and xAI can't turn reasoning off.
#[must_use]
pub fn accepted(reasoning: Reasoning, provider: &str, model: &str) -> Reasoning {
    let Reasoning::Effort(effort) = reasoning else { return reasoning };
    let mut known = crate::catalog::efforts(provider, model);
    if known.is_empty() {
        known = if provider == "xai" {
            vec![Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh]
        } else {
            vec![Effort::None, Effort::Minimal, Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh]
        };
    }
    Reasoning::Effort(effort.nearest(&known))
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
/// `response.steer` input. Steering accepts only user messages with text,
/// image and file content.
#[must_use]
pub fn steer_input(parts: &[Part]) -> Value {
    let content: Vec<_> = parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text, .. } | Part::System { text } => Some(json!({"type":"input_text","text":text})),
            Part::Image(m) => Some(media(m, true)),
            Part::Document(m) => Some(media(m, false)),
            _ => None,
        })
        .collect();
    json!([{"type":"message","role":"user","content":content}])
}
/// Native client tools: the call item type each tool's calls arrive as.
const NATIVE_CALLS: [&str; 2] = ["apply_patch_call", "shell_call"];
/// Stream events for a `/responses/compact` result: the returned window
/// becomes one compaction part that replaces the summarized items.
#[must_use]
pub fn compacted(response: &Value, provider: &str) -> Vec<crate::StreamEvent> {
    use crate::{FinishReason, StreamEvent as E, Usage};
    let id = response["id"].as_str().map(String::from);
    let model = response["model"].as_str().unwrap_or_default().to_owned();
    let window = json!({"type":"compacted_window","output":response["output"].as_array().cloned().unwrap_or_default()});
    let mut events = vec![
        E::Start { response_id: id.clone(), model: Some(model.clone()) },
        E::PartDone { part: Part::Compaction { summary: None, opaque: Some(crate::Opaque { provider: provider.into(), model, data: window }) } },
    ];
    let u = &response["usage"];
    if u.is_object() {
        let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
        events.push(E::Usage(Usage {
            input: u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
            output: u["output_tokens"].as_u64().unwrap_or(0),
            cache_read: cached,
            reasoning: u["output_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
            ..Usage::default()
        }));
    }
    events.push(E::Finish { reason: FinishReason::Other("compaction".into()), response_id: id });
    events
}
/// The output item for a native client call's result.
fn native_output(kind: &str, call_id: &str, content: &[ResultPart], is_error: bool) -> Value {
    let text: String = content.iter().filter_map(|p| if let ResultPart::Text { text } = p { Some(text.as_str()) } else { None }).collect();
    if kind == "shell_call" {
        // A tool may return the documented output array as JSON; otherwise wrap the text.
        let output = serde_json::from_str::<Value>(&text)
            .ok()
            .filter(Value::is_array)
            .unwrap_or_else(|| json!([{"stdout":text,"stderr":"","outcome":{"type":"exit","exit_code":i32::from(is_error)}}]));
        return json!({"type":"shell_call_output","call_id":call_id,"output":output});
    }
    json!({"type":"apply_patch_call_output","call_id":call_id,"status":if is_error { "failed" } else { "completed" },"output":text})
}
/// Responses input items for `items`, as [`request_for`] sends them.
#[must_use]
pub fn input(items: &[crate::Item], provider: &str) -> Vec<Value> {
    let mut input = Vec::new();
    let mut kinds: HashMap<&str, (CallKind, Option<&str>)> = HashMap::new();
    for item in items {
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
            let own = |o: &Option<crate::Opaque>| o.as_ref().filter(|o| o.matches_provider(provider)).map(|o| o.data.clone());
            match part {
                Part::Reasoning { opaque, .. } => input.extend(own(opaque)),
                // A `/responses/compact` window replaces the items it summarized.
                Part::Compaction { opaque, .. } => match own(opaque) {
                    Some(data) if data["type"] == "compacted_window" => input.extend(data["output"].as_array().into_iter().flatten().cloned()),
                    data => input.extend(data),
                },
                Part::ToolCall { call_id, name, input: args, kind, opaque } => {
                    let raw = own(opaque);
                    let native = raw.as_ref().and_then(|v| v["type"].as_str()).and_then(|t| NATIVE_CALLS.iter().copied().find(|n| *n == t));
                    kinds.insert(call_id.as_str(), (*kind, native));
                    match (kind, raw) {
                        (_, Some(raw)) => input.push(raw),
                        (CallKind::Function, None) => {
                            input.push(json!({"type":"function_call","call_id":call_id,"name":name,"arguments":args.to_string()}))
                        }
                        (CallKind::Custom, None) => {
                            input.push(json!({"type":"custom_tool_call","call_id":call_id,"name":name,"input":args.as_str().unwrap_or_default()}))
                        }
                        (CallKind::Hosted, None) => {}
                    }
                }
                Part::ToolResult { call_id, content, is_error } => {
                    let (kind, native) = kinds.get(call_id.as_str()).copied().unwrap_or_default();
                    if let Some(native) = native {
                        input.push(native_output(native, call_id, content, *is_error));
                        continue;
                    }
                    let content: Vec<_> = content
                        .iter()
                        .map(|p| match p {
                            ResultPart::Text { text } => json!({"type":"input_text","text":text}),
                            ResultPart::Image(m) => media(m, true),
                            ResultPart::Document(m) => media(m, false),
                        })
                        .collect();
                    let kind = if kind == CallKind::Custom { "custom_tool_call_output" } else { "function_call_output" };
                    input.push(json!({"type":kind, "call_id":call_id,"output":content}));
                }
                _ => {}
            }
        }
    }
    input
}
#[must_use]
pub fn request(req: &Request) -> Value {
    request_for(req, "openai")
}
/// A Responses body for `provider` (`openai`, or a wire-compatible one such
/// as `xai`): only that provider's opaque items are replayed.
#[must_use]
pub fn request_for(req: &Request, provider: &str) -> Value {
    let input = input(&req.items, provider);
    let asynchronous = req.provider.openai.as_ref().map(|o| o.async_tools.as_slice()).unwrap_or_default();
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
            ToolKind::Hosted { tool, config } | ToolKind::Native { tool, config } => {
                let mut value = json!({"type":tool});
                if config.is_object() {
                    merge(&mut value, config);
                }
                value
            }
        })
        .zip(&req.tools)
        .map(|(mut value, tool)| {
            if asynchronous.contains(&tool.name) && matches!(value["type"].as_str(), Some("function" | "custom")) {
                value["async"] = json!(true);
            }
            value
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
        body["reasoning"] = json!({"effort":effort(accepted(reasoning, provider, &req.model)),"summary":"auto"});
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
    if let Some(crate::Compaction::Auto { threshold, .. }) = &req.compaction {
        body["context_management"] = json!([{"type":"compaction","compact_threshold":threshold}]);
    }
    if let Some(extra) = &req.extra {
        merge(&mut body, extra);
    }
    stateless_reasoning(&mut body);
    body
}
