//! Canonical requests to Messages. Foreign or unsigned reasoning is dropped.
use crate::{Effort, ErrorClass, LlmError, Media, MediaSource, Part, Reasoning, Request, ResultPart, Role, ToolChoice, ToolKind};
use serde_json::{Value, json};

pub fn body(req: &Request) -> Result<Value, LlmError> {
    body_for(req, "anthropic")
}
/// A Messages body for `provider` (`anthropic`, or an Anthropic-dialect one
/// such as `minimax`): only its opaque blocks are replayed, and Anthropic
/// betas (mid-conversation system, compaction) apply to Anthropic only.
pub fn body_for(req: &Request, provider: &str) -> Result<Value, LlmError> {
    let anthropic = provider == "anthropic";
    let native_system = anthropic && super::supports_mid_system(&req.model);
    let mut messages: Vec<Value> = Vec::new();
    for item in &req.items {
        // A system message must follow a user one (a subagent report waking an idle parent follows its reply).
        let native_system = native_system && messages.last().is_some_and(|m| m["role"] != "assistant");
        let role = match item.role {
            Role::Assistant => "assistant",
            Role::System if native_system => "system",
            _ => "user",
        };
        let mut content = Vec::new();
        for part in &item.parts {
            if let Part::ToolsChanged { added, removed } = part
                && native_system
                && item.role == Role::System
            {
                for (kind, names) in [("tool_addition", added), ("tool_removal", removed)] {
                    content.extend(names.iter().map(|name| json!({"type":kind,"tool":{"type":"tool_reference","name":name}})));
                }
                continue;
            }
            if let Some(block) = part_block(part, provider)? {
                content.push(block);
            }
        }
        if item.role == Role::System && !native_system {
            let text = content.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
            content = vec![json!({"type":"text", "text":format!("<system-reminder>\n{text}\n</system-reminder>")})];
        }
        if content.is_empty() {
            continue;
        }
        if let Some(last) = messages.last_mut().filter(|m| m["role"] == role) {
            last["content"].as_array_mut().expect("content array").extend(content);
        } else {
            messages.push(json!({"role":role,"content":content}));
        }
    }
    for message in &mut messages {
        if message["role"] == "user" {
            message["content"].as_array_mut().expect("content array").sort_by_key(|b| b["type"] != "tool_result");
        }
    }
    let mut tools = Vec::new();
    for tool in &req.tools {
        let mut value = match &tool.kind {
            ToolKind::Hosted { tool: kind, config } | ToolKind::Native { tool: kind, config } => {
                let mut v = json!({"type":kind,"name":tool.name});
                if config.is_object() {
                    deep_merge(&mut v, config.clone());
                }
                v
            }
            ToolKind::Custom { .. } => return Err(LlmError::new(ErrorClass::InvalidRequest, "Anthropic does not support freeform custom tools")),
            _ => json!({"name":tool.name,"description":tool.description,"input_schema":tool.input_schema}),
        };
        if tool.kind == ToolKind::Strict {
            value["strict"] = json!(true);
        }
        if tool.defer_loading {
            value["defer_loading"] = json!(true);
        }
        tools.push(value);
    }
    let mut body = json!({"model":req.model,"max_tokens":req.max_output.unwrap_or(8192),"messages":messages,"stream":true});
    if !req.system.is_empty() {
        body["system"] = json!(req.system.iter().map(|t| json!({"type":"text","text":t})).collect::<Vec<_>>());
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = match &req.tool_choice {
            ToolChoice::Auto => json!({"type":"auto"}),
            ToolChoice::None => json!({"type":"none"}),
            ToolChoice::Required => json!({"type":"any"}),
            ToolChoice::Tool(name) => json!({"type":"tool","name":name}),
        };
        if let Some(parallel) = req.parallel_tools
            && req.tool_choice != ToolChoice::None
        {
            body["tool_choice"]["disable_parallel_tool_use"] = json!(!parallel);
        }
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(t) = req.top_p {
        body["top_p"] = json!(t);
    }
    if !req.stop.is_empty() {
        body["stop_sequences"] = json!(req.stop);
    }
    if let Some(user) = &req.user {
        body["metadata"] = json!({"user_id":user});
    }
    if let Some(schema) = &req.output_schema {
        body["output_config"] = json!({"format":{"type":"json_schema","schema":schema}});
    }
    match req.reasoning {
        Some(Reasoning::Budget(n)) => {
            body["thinking"] = json!({"type":"enabled","budget_tokens":n});
            if req.max_output.is_none() {
                body["max_tokens"] = json!(n.saturating_add(8192));
            }
        }
        Some(Reasoning::Effort(effort)) => {
            if effort == Effort::None && !super::always_thinking(&req.model) {
                body["thinking"] = json!({"type":"disabled"});
            } else if super::supports_adaptive(&req.model) {
                body["thinking"] = json!({"type":"adaptive"});
                // An effort the model doesn't list goes to the nearest it does (Opus 4.5 has no `max`, 4.6 no `xhigh`).
                let effort = match effort.nearest(&crate::catalog::efforts(provider, &req.model)) {
                    Effort::None | Effort::Minimal | Effort::Low => "low",
                    Effort::Medium => "medium",
                    Effort::High => "high",
                    Effort::Xhigh => "xhigh",
                    Effort::Max => "max",
                };
                body["output_config"]["effort"] = json!(effort);
            } else {
                let budget = match effort {
                    Effort::None | Effort::Minimal | Effort::Low => 1024,
                    Effort::Medium => 4096,
                    Effort::High => 8192,
                    Effort::Xhigh | Effort::Max => 16384,
                };
                body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
                if req.max_output.is_none() {
                    body["max_tokens"] = json!(budget + 8192);
                }
            }
        }
        None => {}
    }
    if body["thinking"]["type"] == "enabled" || body["thinking"]["type"] == "adaptive" || super::always_thinking(&req.model) {
        body.as_object_mut().expect("object").remove("temperature");
    }
    if let Some(options) = &req.provider.anthropic {
        if let Some(value) = &options.context_management {
            body["context_management"] = value.clone();
        }
        if let Some(value) = &options.container {
            body["container"] = value.clone();
        }
        if let Some(value) = &options.thinking {
            body["thinking"] = value.clone();
            body.as_object_mut().expect("object").remove("temperature");
        }
    }
    if !req.cache.disabled {
        cache(&mut body, req, provider);
    }
    match &req.compaction {
        // On demand: the response is one signed compaction block. The API
        // rejects these fields on a summarization request.
        Some(crate::Compaction::Now { instructions }) if anthropic => {
            let object = body.as_object_mut().expect("object");
            for key in ["stop_sequences", "context_management"] {
                object.remove(key);
            }
            if let Some(config) = object.get_mut("output_config").and_then(Value::as_object_mut) {
                config.remove("format");
            }
            if matches!(body["tool_choice"]["type"].as_str(), Some("any" | "tool")) {
                body["tool_choice"] = json!({"type":"auto"});
            }
            body["compaction"] = json!({"type":"summarize"});
            if let Some(text) = instructions {
                body["compaction"]["instructions"] = json!(text);
            }
        }
        Some(crate::Compaction::Auto { threshold, instructions }) if anthropic => {
            // Paused, so the caller can put the current prompt after the
            // checkpoint: the API drops everything before it.
            let mut edit = json!({"type":"compact_20260112","trigger":{"type":"input_tokens","value":threshold},"pause_after_compaction":true});
            if let Some(text) = instructions {
                edit["instructions"] = json!(text);
            }
            if !body["context_management"]["edits"].is_array() {
                body["context_management"]["edits"] = json!([]);
            }
            body["context_management"]["edits"].as_array_mut().expect("edits").push(edit);
        }
        _ => {}
    }
    if let Some(extra) = &req.extra {
        deep_merge(&mut body, extra.clone());
    }
    Ok(body)
}

/// Betas a request body needs: compaction (requested or a block in the
/// history; on-demand blocks are signed) and context editing.
#[must_use]
pub fn betas_for(body: &Value) -> Vec<&'static str> {
    let mut betas = Vec::new();
    let edits = body["context_management"]["edits"].as_array().cloned().unwrap_or_default();
    let blocks: Vec<&Value> = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"] == "compaction")
        .collect();
    if body.get("compaction").is_some() || blocks.iter().any(|b| b.get("signature").is_some()) {
        betas.push(super::api::Beta::COMPACT);
    }
    if edits.iter().any(|e| e["type"] == "compact_20260112") || blocks.iter().any(|b| b.get("signature").is_none()) {
        betas.push(super::api::Beta::COMPACT_THRESHOLD);
    }
    if edits.iter().any(|e| e["type"].as_str().is_some_and(|t| t.starts_with("clear_"))) {
        betas.push(super::api::Beta::CONTEXT_MANAGEMENT);
    }
    betas
}

fn part_block(part: &Part, provider: &str) -> Result<Option<Value>, LlmError> {
    let opaque = match part {
        Part::Reasoning { opaque, .. } | Part::ToolCall { opaque, .. } | Part::HostedResult { opaque, .. } | Part::Compaction { opaque, .. } => {
            opaque.as_ref()
        }
        _ => None,
    };
    if let Some(opaque) = opaque.filter(|o| o.matches_provider(provider)) {
        return Ok(Some(opaque.data.clone()));
    }
    Ok(match part {
        Part::Text { text, citations } => {
            let mut b = json!({"type":"text","text":text});
            let citations: Vec<_> =
                citations.iter().filter_map(|c| c.raw.as_ref().filter(|o| o.matches_provider(provider)).map(|o| o.data.clone())).collect();
            if !citations.is_empty() {
                b["citations"] = json!(citations);
            }
            Some(b)
        }
        Part::System { text } => Some(json!({"type":"text","text":text})),
        Part::Image(media) => Some(media_block("image", media)?),
        Part::Document(media) => Some(media_block("document", media)?),
        Part::Audio(_) => return Err(LlmError::new(ErrorClass::InvalidRequest, "Anthropic does not support audio input")),
        Part::ToolCall { call_id, name, input, kind, .. } if *kind != crate::CallKind::Hosted => {
            Some(json!({"type":"tool_use","id":call_id,"name":name,"input":input}))
        }
        Part::ToolResult { call_id, content, is_error } => {
            let content = content
                .iter()
                .map(|part| match part {
                    ResultPart::Text { text } => Ok(json!({"type":"text","text":text})),
                    ResultPart::Image(m) => media_block("image", m),
                    ResultPart::Document(m) => media_block("document", m),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Some(json!({"type":"tool_result","tool_use_id":call_id,"content":content,"is_error":is_error}))
        }
        Part::HostedResult { payload, .. } => Some(json!({"type":"text","text":payload.to_string()})),
        Part::Compaction { summary: Some(text), .. } => Some(json!({"type":"text","text":text})),
        Part::ToolsChanged { added, removed } => {
            Some(json!({"type":"text","text":format!("Tools added: {}; removed: {}", added.join(", "), removed.join(", "))}))
        }
        _ => None,
    })
}
fn media_block(kind: &str, media: &Media) -> Result<Value, LlmError> {
    let source = match &media.source {
        MediaSource::Base64 { data } => json!({"type":"base64","media_type":media.mime,"data":data}),
        MediaSource::Url { url } => json!({"type":"url","url":url}),
        MediaSource::File { provider, id } if provider.as_str() == "anthropic" => json!({"type":"file","file_id":id}),
        _ => return Err(LlmError::new(ErrorClass::InvalidRequest, "cannot send another provider's file to Anthropic")),
    };
    Ok(json!({"type":kind,"source":source}))
}
pub(crate) fn deep_merge(target: &mut Value, value: Value) {
    if let (Some(a), Some(b)) = (target.as_object_mut(), value.as_object()) {
        for (k, v) in b {
            deep_merge(a.entry(k).or_insert(Value::Null), v.clone());
        }
    } else {
        *target = value;
    }
}
fn cache(body: &mut Value, req: &Request, provider: &str) {
    let control = if req.cache.long_ttl { json!({"type":"ephemeral","ttl":"1h"}) } else { json!({"type":"ephemeral"}) };
    let protected: Vec<_> = req
        .items
        .iter()
        .flat_map(|i| &i.parts)
        .filter_map(|p| match p {
            Part::Reasoning { opaque, .. } | Part::ToolCall { opaque, .. } | Part::HostedResult { opaque, .. } | Part::Compaction { opaque, .. } => {
                opaque.as_ref().filter(|o| o.matches_provider(provider)).map(|o| &o.data)
            }
            _ => None,
        })
        .collect();
    let mut remaining = 4usize.saturating_sub(cache_count(body));
    let mut mark = |block: &mut Value| {
        if remaining > 0 && block.get("cache_control").is_none() && !protected.contains(&&*block) {
            block["cache_control"] = control.clone();
            remaining -= 1;
        }
    };
    for key in ["system", "tools"] {
        if let Some(last) = body.get_mut(key).and_then(Value::as_array_mut).and_then(|v| v.last_mut()) {
            mark(last);
        }
    }
    if let Some(messages) = body["messages"].as_array_mut() {
        for message in messages.iter_mut().rev().take(2) {
            if let Some(last) = message["content"].as_array_mut().and_then(|v| v.last_mut()) {
                // Signed blocks must remain untouched, and cannot carry breakpoints.
                if !matches!(last["type"].as_str(), Some("thinking" | "redacted_thinking")) {
                    mark(last);
                }
            }
        }
    }
}
fn cache_count(value: &Value) -> usize {
    match value {
        Value::Object(map) => usize::from(map.contains_key("cache_control")) + map.values().map(cache_count).sum::<usize>(),
        Value::Array(values) => values.iter().map(cache_count).sum(),
        _ => 0,
    }
}
