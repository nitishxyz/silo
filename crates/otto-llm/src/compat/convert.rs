//! Canonical requests to Chat Completions, shaped per dialect.
use super::{Dialect, ReasoningParam, Replay};
use crate::{CallKind, Effort, ErrorClass, LlmError, Media, MediaSource, Part, Reasoning, Request, ResultPart, Role, ToolChoice, ToolKind};
use serde_json::{Value, json};

fn media(media: &Media) -> Result<Value, LlmError> {
    let url = match &media.source {
        MediaSource::Base64 { data } => format!("data:{};base64,{data}", media.mime),
        MediaSource::Url { url } => url.clone(),
        MediaSource::File { .. } => return Err(LlmError::new(ErrorClass::InvalidRequest, "provider file ids cannot be sent to this provider")),
    };
    Ok(if media.mime.starts_with("image/") {
        json!({"type":"image_url","image_url":{"url":url}})
    } else {
        json!({"type":"file","file":{"filename":media.name.clone().unwrap_or_else(|| "file".into()),"file_data":url}})
    })
}

fn effort(reasoning: Reasoning) -> &'static str {
    match reasoning {
        Reasoning::Effort(Effort::None) | Reasoning::Budget(0) => "none",
        Reasoning::Effort(Effort::Minimal) | Reasoning::Budget(1..=1024) => "minimal",
        Reasoning::Effort(Effort::Low) | Reasoning::Budget(1025..=4096) => "low",
        Reasoning::Effort(Effort::Medium) | Reasoning::Budget(4097..=16384) => "medium",
        Reasoning::Effort(Effort::High) | Reasoning::Budget(16385..=32768) => "high",
        Reasoning::Effort(Effort::Xhigh) => "xhigh",
        Reasoning::Effort(Effort::Max) | Reasoning::Budget(_) => "max",
    }
}

/// One assistant message under construction.
#[derive(Default)]
struct Assistant {
    text: String,
    calls: Vec<Value>,
    reasoning: Option<Value>,
}
impl Assistant {
    fn finish(self, out: &mut Vec<Value>, replay: Replay) {
        if self.text.is_empty() && self.calls.is_empty() {
            return;
        }
        let mut message = json!({"role":"assistant","content":if self.text.is_empty() { Value::Null } else { json!(self.text) }});
        if !self.calls.is_empty() {
            message["tool_calls"] = json!(self.calls);
        }
        if let Some(reasoning) = self.reasoning {
            match replay {
                Replay::ReasoningContent => message["reasoning_content"] = reasoning["reasoning_content"].clone(),
                Replay::ReasoningDetails => message["reasoning_details"] = reasoning["reasoning_details"].clone(),
                Replay::None => {}
            }
        }
        out.push(message);
    }
}

/// Chat messages for `req`, with the provider's own reasoning replayed as its
/// dialect expects.
///
/// # Errors
/// Media the dialect cannot take.
pub fn messages(req: &Request, dialect: &Dialect) -> Result<Vec<Value>, LlmError> {
    let mut out = Vec::new();
    if !req.system.is_empty() {
        out.push(json!({"role":"system","content":req.system.join("\n\n")}));
    }
    for item in &req.items {
        if item.role == Role::Assistant {
            let mut message = Assistant::default();
            for part in &item.parts {
                match part {
                    Part::Text { text, .. } => message.text.push_str(text),
                    Part::Reasoning { opaque: Some(o), .. } if o.matches_provider(dialect.id) => message.reasoning = Some(o.data.clone()),
                    Part::ToolCall { call_id, name, input, kind: CallKind::Function | CallKind::Custom, .. } => {
                        let arguments = input.as_str().map_or_else(|| input.to_string(), str::to_owned);
                        message.calls.push(json!({"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}}));
                    }
                    Part::HostedResult { tool, payload, .. } => message.text.push_str(&format!("\n{tool} result: {payload}\n")),
                    Part::Compaction { summary: Some(summary), .. } => message.text.push_str(summary),
                    _ => {}
                }
            }
            message.finish(&mut out, dialect.replay);
            continue;
        }
        let mut content = Vec::new();
        for part in &item.parts {
            match part {
                Part::Text { text, .. } | Part::System { text } => content.push(json!({"type":"text","text":text})),
                Part::ToolsChanged { added, removed } => {
                    content.push(json!({"type":"text","text":format!("Tools added: {}; removed: {}", added.join(", "), removed.join(", "))}));
                }
                Part::Document(m) if !dialect.documents() => content.push(json!({"type":"text","text":m.unsupported(dialect.id)})),
                Part::Image(m) | Part::Document(m) => content.push(media(m)?),
                Part::Compaction { summary: Some(summary), .. } => content.push(json!({"type":"text","text":summary})),
                Part::ToolResult { call_id, content: result, is_error } => {
                    let mut text: String = result
                        .iter()
                        .filter_map(|p| if let ResultPart::Text { text } = p { Some(text.as_str()) } else { None })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if *is_error && text.is_empty() {
                        text = "error".into();
                    }
                    out.push(json!({"role":"tool","tool_call_id":call_id,"content":text}));
                    // Chat tool messages are text-only; images follow as user content.
                    let media: Vec<_> = result
                        .iter()
                        .filter_map(|p| match p {
                            ResultPart::Document(m) if !dialect.documents() => Some(Ok(json!({"type":"text","text":m.unsupported(dialect.id)}))),
                            ResultPart::Image(m) | ResultPart::Document(m) => Some(media(m)),
                            ResultPart::Text { .. } => None,
                        })
                        .collect::<Result<_, _>>()?;
                    if !media.is_empty() {
                        out.push(json!({"role":"user","content":media}));
                    }
                }
                _ => {}
            }
        }
        if content.is_empty() {
            continue;
        }
        let role = if item.role == Role::System { "system" } else { "user" };
        let content = if content.iter().all(|c| c["type"] == "text") {
            json!(content.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n"))
        } else {
            json!(content)
        };
        out.push(json!({"role":role,"content":content}));
    }
    Ok(out)
}

/// The `POST /chat/completions` body.
///
/// # Errors
/// Hosted or freeform tools (Chat Completions has functions only) and media.
pub fn request(req: &Request, dialect: &Dialect) -> Result<Value, LlmError> {
    let tools = req
        .tools
        .iter()
        .map(|tool| match &tool.kind {
            ToolKind::Function | ToolKind::Strict => {
                let mut schema = tool.input_schema.clone();
                let strict = tool.kind == ToolKind::Strict;
                if strict {
                    crate::openai::convert::strictify(&mut schema);
                }
                let mut function = json!({"name":tool.name,"description":tool.description,"parameters":schema});
                if strict {
                    function["strict"] = json!(true);
                }
                Ok(json!({"type":"function","function":function}))
            }
            _ => Err(LlmError::new(ErrorClass::InvalidRequest, format!("{} supports function tools only", dialect.name))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut body = json!({"model":req.model,"messages":messages(req, dialect)?,"stream":true});
    if dialect.usage_option {
        body["stream_options"] = json!({"include_usage":true});
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = match &req.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Tool(name) => json!({"type":"function","function":{"name":name}}),
        };
        if let Some(parallel) = req.parallel_tools {
            body["parallel_tool_calls"] = json!(parallel);
        }
        if dialect.tool_stream {
            body["tool_stream"] = json!(true);
        }
    }
    for (key, value) in
        [("max_tokens", json!(req.max_output)), ("temperature", json!(req.temperature)), ("top_p", json!(req.top_p)), ("user", json!(req.user))]
    {
        if !value.is_null() {
            body[key] = value;
        }
    }
    if !req.stop.is_empty() {
        body["stop"] = json!(req.stop);
    }
    if let Some(schema) = &req.output_schema {
        let mut schema = schema.clone();
        crate::openai::convert::strictify(&mut schema);
        body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"response","strict":true,"schema":schema}});
    }
    if let Some(reasoning) = req.reasoning {
        // An effort the model doesn't list goes to the nearest it does; off
        // stays off where the dialect turns thinking off with it.
        let reasoning = match reasoning {
            Reasoning::Effort(Effort::None) if matches!(dialect.reasoning, ReasoningParam::Thinking | ReasoningParam::ThinkingEffort) => reasoning,
            Reasoning::Effort(e) => Reasoning::Effort(e.nearest(&crate::catalog::efforts(dialect.id, &req.model))),
            Reasoning::Budget(_) => reasoning,
        };
        let effort = effort(reasoning);
        match dialect.reasoning {
            ReasoningParam::Effort => body["reasoning_effort"] = json!(effort),
            ReasoningParam::OpenRouter => body["reasoning"] = json!({"effort":effort}),
            ReasoningParam::Thinking => {
                let enabled = effort != "none";
                body["thinking"] = json!({"type":if enabled { "enabled" } else { "disabled" }});
                if enabled && dialect.replay == Replay::ReasoningContent {
                    // Z.ai preserved thinking: keep earlier reasoning in context.
                    body["thinking"]["clear_thinking"] = json!(false);
                }
            }
            ReasoningParam::ThinkingEffort => {
                body["thinking"] = json!({"type":if effort == "none" { "disabled" } else { "enabled" }});
                if effort != "none" {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            ReasoningParam::None => {}
        }
    }
    if let Some(extra) = &req.extra {
        crate::openai::convert::merge(&mut body, extra);
    }
    Ok(body)
}
