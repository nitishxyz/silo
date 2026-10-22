//! Chat Completions SSE translation, one parser for every dialect: content,
//! `reasoning_content` / `reasoning` / `reasoning_details`, streamed tool
//! calls by index, usage in its several shapes, and mid-stream errors.
use crate::{CallKind, ErrorClass, EventStream, FinishReason, LlmError, Opaque, Part, StreamEvent as E, Usage};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub struct State {
    provider: String,
    model: String,
    id: Option<String>,
    started: bool,
    text: String,
    reasoning: String,
    details: Vec<Value>,
    calls: BTreeMap<u64, Call>,
    reason: Option<String>,
    usage: Option<Usage>,
    pub done: bool,
}

/// Usage from any of the dialects' shapes.
#[must_use]
pub fn usage(u: &Value) -> Usage {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let prompt = n(&u["prompt_tokens"]);
    let cached = [&u["prompt_tokens_details"]["cached_tokens"], &u["prompt_cache_hit_tokens"], &u["cached_tokens"]]
        .into_iter()
        .find_map(Value::as_u64)
        .unwrap_or(0);
    let written = n(&u["prompt_tokens_details"]["cache_write_tokens"]);
    Usage {
        input: prompt.saturating_sub(cached + written),
        output: n(&u["completion_tokens"]),
        cache_read: cached,
        cache_write: written,
        reasoning: n(&u["completion_tokens_details"]["reasoning_tokens"]),
    }
}

/// Merge streamed `reasoning_details` entries by `index`, concatenating text.
fn merge_detail(details: &mut Vec<Value>, delta: &Value) {
    let index = delta["index"].as_u64();
    let existing = index.and_then(|i| details.iter_mut().find(|d| d["index"].as_u64() == Some(i)));
    let Some(existing) = existing else {
        details.push(delta.clone());
        return;
    };
    for (key, value) in delta.as_object().into_iter().flatten() {
        match (existing[key].as_str(), value.as_str()) {
            (Some(old), Some(new)) if matches!(key.as_str(), "text" | "summary" | "data" | "signature") => {
                existing[key] = json!(format!("{old}{new}"))
            }
            _ => existing[key] = value.clone(),
        }
    }
}

impl State {
    #[must_use]
    pub fn new(model: impl Into<String>, provider: &str) -> Self {
        Self { model: model.into(), provider: provider.into(), ..Self::default() }
    }
    /// Translate one chunk.
    ///
    /// # Errors
    /// Error chunks.
    pub fn chunk(&mut self, v: &Value) -> Result<Vec<E>, LlmError> {
        let mut out = Vec::new();
        if let Some(error) = v.get("error").filter(|e| !e.is_null()) {
            self.done = true;
            let status = error["code"].as_u64().and_then(|c| u16::try_from(c).ok()).filter(|c| *c >= 400).unwrap_or(500);
            let body = json!({ "error": error }).to_string();
            let e = crate::http::classify_status(
                reqwest::StatusCode::from_u16(status).unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
                &reqwest::header::HeaderMap::new(),
                &body,
            );
            return Err(LlmError { message: crate::http::provider_message(&body), ..e });
        }
        if !self.started {
            self.started = true;
            self.id = v["id"].as_str().map(Into::into);
            if let Some(model) = v["model"].as_str().filter(|m| !m.is_empty()) {
                self.model = model.into();
            }
            out.push(E::Start { response_id: self.id.clone(), model: Some(self.model.clone()) });
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage(u));
        }
        let Some(choice) = v["choices"].get(0) else { return Ok(out) };
        // Moonshot reports usage on the choice.
        if let Some(u) = choice.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage(u));
        }
        let delta = &choice["delta"];
        for key in ["reasoning_content", "reasoning"] {
            if let Some(text) = delta[key].as_str().filter(|t| !t.is_empty()) {
                self.reasoning.push_str(text);
                out.push(E::ReasoningDelta { text: text.into(), summary: false });
                break;
            }
        }
        for detail in delta["reasoning_details"].as_array().into_iter().flatten() {
            merge_detail(&mut self.details, detail);
        }
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(text);
            out.push(E::TextDelta { text: text.into() });
        }
        for (position, call) in delta["tool_calls"].as_array().into_iter().flatten().enumerate() {
            let index = call["index"].as_u64().unwrap_or(position as u64);
            let entry = self.calls.entry(index).or_default();
            let fresh = entry.id.is_empty() && entry.name.is_empty();
            if let Some(id) = call["id"].as_str().filter(|s| !s.is_empty()) {
                entry.id = id.into();
            }
            if let Some(name) = call["function"]["name"].as_str().filter(|s| !s.is_empty()) {
                entry.name.push_str(name);
            }
            if fresh && !(entry.id.is_empty() && entry.name.is_empty()) {
                if entry.id.is_empty() {
                    entry.id = format!("call_{index}");
                }
                out.push(E::ToolInputStart { call_id: entry.id.clone(), name: entry.name.clone() });
            }
            if let Some(arguments) = call["function"]["arguments"].as_str().filter(|s| !s.is_empty()) {
                entry.arguments.push_str(arguments);
                out.push(E::ToolInputDelta { call_id: entry.id.clone(), delta: arguments.into() });
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.reason = Some(reason.into());
        }
        Ok(out)
    }
    /// The completed parts, usage and finish, at `[DONE]` or end of stream.
    ///
    /// # Errors
    /// Malformed tool arguments, or a stream that ended without a finish reason.
    pub fn finish(&mut self) -> Result<Vec<E>, LlmError> {
        self.done = true;
        let Some(reason) = self.reason.take() else {
            return Err(LlmError::new(ErrorClass::Network, "chat stream ended before a finish reason"));
        };
        let mut out = Vec::new();
        if !self.reasoning.is_empty() || !self.details.is_empty() {
            let data =
                if self.details.is_empty() { json!({"reasoning_content": self.reasoning}) } else { json!({"reasoning_details": self.details}) };
            let text = (!self.reasoning.is_empty()).then(|| self.reasoning.clone());
            out.push(E::PartDone {
                part: Part::Reasoning {
                    summary: None,
                    text,
                    opaque: Some(Opaque { provider: self.provider.as_str().into(), model: self.model.clone(), data }),
                },
            });
        }
        if !self.text.is_empty() {
            out.push(E::PartDone { part: Part::text(std::mem::take(&mut self.text)) });
        }
        let tool = !self.calls.is_empty();
        for (_, call) in std::mem::take(&mut self.calls) {
            let input = if call.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.arguments).map_err(|e| LlmError::new(ErrorClass::Other, format!("invalid tool arguments: {e}")))?
            };
            out.push(E::PartDone { part: Part::ToolCall { call_id: call.id, name: call.name, input, kind: CallKind::Function, opaque: None } });
        }
        if let Some(usage) = self.usage.take() {
            out.push(E::Usage(usage));
        }
        let reason = match reason.as_str() {
            _ if tool => FinishReason::ToolUse,
            "stop" => FinishReason::Stop,
            "tool_calls" | "function_call" => FinishReason::ToolUse,
            "length" => FinishReason::MaxTokens,
            "content_filter" | "sensitive" => FinishReason::ContentFilter,
            other => FinishReason::Other(other.into()),
        };
        out.push(E::Finish { reason, response_id: self.id.clone() });
        Ok(out)
    }
}

pub fn stream<S>(bytes: S, model: impl Into<String>, provider: &str) -> EventStream
where
    S: Stream<Item = Result<Bytes, LlmError>> + Send + 'static,
{
    Box::pin(futures::stream::unfold(
        (Box::pin(crate::sse::sse_stream(bytes)), State::new(model, provider), VecDeque::new()),
        |(mut source, mut state, mut queue)| async move {
            loop {
                if let Some(event) = queue.pop_front() {
                    return Some((event, (source, state, queue)));
                }
                if state.done {
                    return None;
                }
                let result = match source.next().await {
                    Some(Ok(event)) if event.data.trim() == "[DONE]" => state.finish(),
                    Some(Ok(event)) => serde_json::from_str::<Value>(&event.data)
                        .map_err(|e| LlmError::new(ErrorClass::Other, e.to_string()))
                        .and_then(|v| state.chunk(&v)),
                    Some(Err(e)) => Err(e),
                    // Some local servers close without `[DONE]` after the finish chunk.
                    None => state.finish(),
                };
                match result {
                    Ok(events) => queue.extend(events.into_iter().map(Ok)),
                    Err(e) => {
                        state.done = true;
                        queue.push_back(Err(e));
                    }
                }
            }
        },
    ))
}
