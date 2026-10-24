//! Responses SSE translation. Completed items alone populate the transcript.
use crate::{CallKind, Citation, ErrorClass, EventStream, FinishReason, LlmError, Opaque, Part, StreamEvent as E, Usage};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

fn string(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_owned()
}
fn texts(v: &Value) -> Option<String> {
    v.as_array().map(|a| a.iter().filter_map(|p| p["text"].as_str()).collect())
}
/// A `local_shell_call` argument as a shell word.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c)) {
        arg.into()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}
fn parts(item: &Value, model: &str, provider: &str) -> Result<Vec<Part>, LlmError> {
    let opaque = |model: &str, data: Value| Opaque { provider: provider.into(), model: model.into(), data };
    Ok(match item["type"].as_str().unwrap_or_default() {
        "message" => item["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| {
                let text = if c["type"] == "refusal" { &c["refusal"] } else { &c["text"] };
                text.as_str().map(|text| Part::Text {
                    text: text.into(),
                    citations: c["annotations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|a| a["type"] == "url_citation")
                        .map(|a| Citation {
                            url: a["url"].as_str().map(Into::into),
                            title: a["title"].as_str().map(Into::into),
                            cited_text: None,
                            raw: Some(opaque(model, a.clone())),
                        })
                        .collect(),
                })
            })
            .collect(),
        "reasoning" => {
            vec![Part::Reasoning { summary: texts(&item["summary"]), text: texts(&item["content"]), opaque: Some(opaque(model, item.clone())) }]
        }
        "function_call" | "custom_tool_call" => {
            let custom = item["type"] == "custom_tool_call";
            let input = if custom {
                item["input"].clone()
            } else {
                serde_json::from_str(item["arguments"].as_str().unwrap_or_default())
                    .map_err(|e| LlmError::new(ErrorClass::Other, format!("invalid function arguments: {e}")))?
            };
            // Async calls keep their raw item so `async: true` is replayed.
            let raw = item["async"].as_bool().unwrap_or_default().then(|| opaque(model, item.clone()));
            vec![Part::ToolCall {
                call_id: string(item, "call_id"),
                name: string(item, "name"),
                input,
                kind: if custom { CallKind::Custom } else { CallKind::Function },
                opaque: raw,
            }]
        }
        // Each command of a shell call runs as its own `shell` entry: the first
        // keeps the call id and the raw item, the rest get `{call_id}_{n}`, and
        // `convert` answers the call once with every command's output.
        "shell_call" | "local_shell_call" => {
            let action = &item["action"];
            let commands: Vec<String> = match action["commands"].as_array() {
                Some(commands) => commands.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                None => vec![action["command"].as_array().into_iter().flatten().filter_map(Value::as_str).map(quote).collect::<Vec<_>>().join(" ")],
            };
            let id = string(item, "call_id");
            commands
                .into_iter()
                .enumerate()
                .map(|(n, command)| {
                    let mut input = serde_json::json!({"command":command});
                    if let Some(timeout) = action["timeout_ms"].as_u64() {
                        input["timeout_ms"] = timeout.into();
                    }
                    if let Some(cwd) = action["working_directory"].as_str() {
                        input["cwd"] = cwd.into();
                    }
                    Part::ToolCall {
                        call_id: if n == 0 { id.clone() } else { format!("{id}_{n}") },
                        name: "shell".into(),
                        input,
                        kind: CallKind::Function,
                        opaque: (n == 0).then(|| opaque(model, item.clone())),
                    }
                })
                .collect()
        }
        // Server-side compaction (`context_management`): opaque, replayed as-is.
        "compaction" => vec![Part::Compaction { summary: None, opaque: Some(opaque(model, item.clone())) }],
        kind if kind.ends_with("_call_output") => vec![Part::HostedResult {
            call_id: string(item, "call_id"),
            tool: kind.trim_end_matches("_call_output").into(),
            payload: item.clone(),
            opaque: Some(opaque(model, item.clone())),
        }],
        kind if kind.ends_with("_call") => vec![
            Part::ToolCall {
                call_id: string(item, "id"),
                name: kind.trim_end_matches("_call").into(),
                input: item.get("action").cloned().unwrap_or(Value::Null),
                kind: CallKind::Hosted,
                opaque: Some(opaque(model, item.clone())),
            },
            Part::HostedResult { call_id: string(item, "id"), tool: kind.trim_end_matches("_call").into(), payload: item.clone(), opaque: None },
        ],
        _ => vec![],
    })
}
#[derive(Default)]
pub(super) struct State {
    calls: HashMap<String, String>,
    id: Option<String>,
    model: String,
    provider: String,
    tool: bool,
    refusal: bool,
    pub(super) done: bool,
}
impl State {
    pub(super) fn new(model: String) -> Self {
        Self::for_provider(model, "openai")
    }
    pub(super) fn for_provider(model: String, provider: &str) -> Self {
        Self { model, provider: provider.into(), ..Self::default() }
    }
    /// Translate one Responses event. A WebSocket steering successor starts
    /// with its own `response.created`, so per-response flags reset there.
    pub(super) fn event(&mut self, v: Value) -> Result<Vec<E>, LlmError> {
        let mut out = Vec::new();
        let kind = v["type"].as_str().unwrap_or_default();
        match kind {
            "response.created" => {
                self.id = v["response"]["id"].as_str().map(Into::into);
                self.tool = false;
                self.refusal = false;
                self.done = false;
                if let Some(m) = v["response"]["model"].as_str() {
                    self.model = m.into();
                }
                out.push(E::Start { response_id: self.id.clone(), model: Some(self.model.clone()) });
            }
            "response.output_text.delta" => out.push(E::TextDelta { text: string(&v, "delta") }),
            "response.refusal.delta" => {
                self.refusal = true;
                out.push(E::TextDelta { text: string(&v, "delta") });
            }
            // Complete content is emitted once, from output_item.done, just like output_text.
            "response.refusal.done" => self.refusal = true,
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                out.push(E::ReasoningDelta { text: string(&v, "delta"), summary: kind.contains("summary") })
            }
            "response.output_item.added" => {
                let item = &v["item"];
                if matches!(item["type"].as_str(), Some("function_call" | "custom_tool_call")) {
                    let call_id = string(item, "call_id");
                    self.calls.insert(string(item, "id"), call_id.clone());
                    out.push(E::ToolInputStart { call_id, name: string(item, "name") });
                }
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let id = string(&v, "item_id");
                let call_id = self.calls.get(&id).cloned().unwrap_or(id);
                out.push(E::ToolInputDelta { call_id, delta: string(&v, "delta") });
            }
            "response.output_item.done" => {
                self.refusal |= v["item"]["content"].as_array().is_some_and(|content| content.iter().any(|c| c["type"] == "refusal"));
                for part in parts(&v["item"], &self.model, &self.provider)? {
                    if matches!(part, Part::ToolCall { .. }) {
                        self.tool = true;
                    }
                    out.push(E::PartDone { part });
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = &v["response"];
                self.id = response["id"].as_str().map(Into::into).or(self.id.clone());
                if let Some(u) = response.get("usage").filter(|v| v.is_object()) {
                    let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                    out.push(E::Usage(Usage {
                        input: u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
                        output: u["output_tokens"].as_u64().unwrap_or(0),
                        cache_read: cached,
                        reasoning: u["output_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
                        ..Usage::default()
                    }));
                }
                let reason = if kind == "response.incomplete" {
                    match response["incomplete_details"]["reason"].as_str().unwrap_or_default() {
                        "max_output_tokens" => FinishReason::MaxTokens,
                        "content_filter" => FinishReason::ContentFilter,
                        reason => FinishReason::Other(reason.into()),
                    }
                } else if self.refusal {
                    FinishReason::Refusal
                } else if self.tool {
                    FinishReason::ToolUse
                } else {
                    FinishReason::Stop
                };
                out.push(E::Finish { reason, response_id: self.id.clone() });
                self.done = true;
            }
            "response.failed" | "error" => {
                self.done = true;
                let error = if kind == "response.failed" { &v["response"]["error"] } else { v.get("error").unwrap_or(&v) };
                let code = string(error, "code");
                // WebSocket `error` events carry the HTTP status they stand for.
                let status = v["status"].as_u64().and_then(|s| u16::try_from(s).ok()).filter(|s| *s >= 400).unwrap_or(match code.as_str() {
                    "rate_limit_exceeded" | "insufficient_quota" | "usage_limit_reached" => 429,
                    "invalid_api_key" => 401,
                    "context_length_exceeded" => 400,
                    "server_error" => 500,
                    _ => 400,
                });
                let body = serde_json::json!({ "error": error }).to_string();
                let error = crate::http::classify_status(
                    reqwest::StatusCode::from_u16(status).unwrap_or(reqwest::StatusCode::BAD_REQUEST),
                    &reqwest::header::HeaderMap::new(),
                    &body,
                );
                return Err(LlmError { message: crate::http::provider_message(&body), ..error });
            }
            kind if kind.starts_with("response.web_search_call.") => out.push(E::HostedProgress {
                call_id: string(&v, "item_id"),
                tool: "web_search".into(),
                status: kind.rsplit('.').next().unwrap_or_default().into(),
            }),
            // The Codex backend's `codex.rate_limits`: the plan's windows.
            kind if kind.ends_with("rate_limits") => {
                out.extend(crate::plan_usage::codex_event(&v, crate::plan_usage::unix_now()).map(E::PlanUsage));
            }
            _ => {}
        }
        Ok(out)
    }
}
pub fn stream<S>(bytes: S, model: impl Into<String>) -> EventStream
where
    S: Stream<Item = Result<Bytes, LlmError>> + Send + 'static,
{
    stream_for(bytes, model, "openai")
}
/// [`stream`] for a wire-compatible provider (`xai`): opaque items carry its id.
pub fn stream_for<S>(bytes: S, model: impl Into<String>, provider: &str) -> EventStream
where
    S: Stream<Item = Result<Bytes, LlmError>> + Send + 'static,
{
    let state = State::for_provider(model.into(), provider);
    Box::pin(futures::stream::unfold(
        (Box::pin(crate::sse::sse_stream(bytes)), state, VecDeque::new()),
        |(mut source, mut state, mut queue)| async move {
            loop {
                if let Some(event) = queue.pop_front() {
                    return Some((event, (source, state, queue)));
                }
                if state.done {
                    return None;
                }
                match source.next().await {
                    Some(Ok(event)) if event.data == "[DONE]" => {}
                    Some(Ok(event)) => {
                        let result = serde_json::from_str(&event.data)
                            .map_err(|e| LlmError::new(ErrorClass::Other, e.to_string()))
                            .and_then(|v| state.event(v));
                        match result {
                            Ok(events) => queue.extend(events.into_iter().map(Ok)),
                            Err(e) => {
                                state.done = true;
                                queue.push_back(Err(e));
                            }
                        }
                    }
                    Some(Err(e)) => {
                        state.done = true;
                        queue.push_back(Err(e));
                    }
                    None => {
                        state.done = true;
                        queue.push_back(Err(LlmError::new(ErrorClass::Network, "Responses stream ended before completion")));
                    }
                }
            }
        },
    ))
}
