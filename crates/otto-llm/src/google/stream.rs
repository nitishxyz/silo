//! Interactions SSE translation: `step.start` / `step.delta` / `step.stop`
//! assemble each step; completed steps alone populate the transcript.
use super::convert::PROVIDER;
use crate::{CallKind, ErrorClass, EventStream, FinishReason, LlmError, Opaque, Part, StreamEvent as E, Usage};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

#[derive(Default)]
struct Step {
    value: Value,
    arguments: String,
}

#[derive(Default)]
pub struct State {
    steps: BTreeMap<u64, Step>,
    id: Option<String>,
    model: String,
    tool: bool,
    pub done: bool,
}

fn string(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_owned()
}
fn append(v: &mut Value, key: &str, text: &str) {
    let mut s = string(v, key);
    s.push_str(text);
    v[key] = json!(s);
}

impl State {
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self { model: model.into(), ..Self::default() }
    }
    fn opaque(&self, data: Value) -> Opaque {
        Opaque { provider: PROVIDER.into(), model: self.model.clone(), data }
    }
    /// Translate one Interactions event.
    ///
    /// # Errors
    /// `error` events, failed interactions and malformed function arguments.
    pub fn event(&mut self, v: Value) -> Result<Vec<E>, LlmError> {
        let mut out = Vec::new();
        let index = v["index"].as_u64().unwrap_or(0);
        match v["event_type"].as_str().unwrap_or_default() {
            "interaction.created" => {
                let interaction = &v["interaction"];
                self.id = interaction["id"].as_str().map(Into::into);
                if let Some(model) = interaction["model"].as_str() {
                    self.model = model.into();
                }
                out.push(E::Start { response_id: self.id.clone(), model: Some(self.model.clone()) });
            }
            "step.start" => {
                let step = v["step"].clone();
                let kind = step["type"].as_str().unwrap_or_default();
                if kind == "function_call" {
                    out.push(E::ToolInputStart { call_id: string(&step, "id"), name: string(&step, "name") });
                } else if kind.ends_with("_call") {
                    out.push(E::HostedProgress {
                        call_id: string(&step, "id"),
                        tool: kind.trim_end_matches("_call").into(),
                        status: "in_progress".into(),
                    });
                }
                self.steps.insert(index, Step { value: step, arguments: String::new() });
            }
            "step.delta" => {
                let step = self.steps.entry(index).or_default();
                let delta = &v["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text" => {
                        let text = string(delta, "text");
                        if !step.value["content"].is_array() {
                            step.value["content"] = json!([]);
                        }
                        let content = step.value["content"].as_array_mut().expect("content");
                        match content.last_mut().filter(|c| c["type"] == "text") {
                            Some(last) => append(last, "text", &text),
                            None => content.push(json!({"type":"text","text":text})),
                        }
                        out.push(E::TextDelta { text });
                    }
                    "thought_summary" => {
                        let item = delta["content"].clone();
                        if let Some(text) = item["text"].as_str() {
                            out.push(E::ReasoningDelta { text: text.into(), summary: true });
                        }
                        if !step.value["summary"].is_array() {
                            step.value["summary"] = json!([]);
                        }
                        step.value["summary"].as_array_mut().expect("summary").push(item);
                    }
                    "thought_signature" => step.value["signature"] = delta["signature"].clone(),
                    "arguments_delta" => {
                        let text = string(delta, "arguments");
                        step.arguments.push_str(&text);
                        out.push(E::ToolInputDelta { call_id: string(&step.value, "id"), delta: text });
                    }
                    // Server tool steps: their fields arrive whole.
                    _ => {
                        for (key, value) in delta.as_object().into_iter().flatten().filter(|(key, _)| *key != "type") {
                            step.value[key] = value.clone();
                        }
                    }
                }
            }
            "step.stop" => {
                let Some(Step { mut value, arguments }) = self.steps.remove(&index) else {
                    return Err(LlmError::new(ErrorClass::Other, "step.stop for a missing step"));
                };
                if !arguments.is_empty() {
                    value["arguments"] =
                        serde_json::from_str(&arguments).map_err(|e| LlmError::new(ErrorClass::Other, format!("invalid function arguments: {e}")))?;
                }
                let kind = value["type"].as_str().unwrap_or_default().to_owned();
                let parts = match kind.as_str() {
                    "model_output" => value["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).map(Part::text).collect(),
                    "thought" => {
                        let summary: String = value["summary"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
                        vec![Part::Reasoning { summary: (!summary.is_empty()).then_some(summary), text: None, opaque: Some(self.opaque(value)) }]
                    }
                    "function_call" => {
                        self.tool = true;
                        vec![Part::ToolCall {
                            call_id: string(&value, "id"),
                            name: string(&value, "name"),
                            input: value.get("arguments").cloned().unwrap_or_else(|| json!({})),
                            kind: CallKind::Function,
                            opaque: Some(self.opaque(value)),
                        }]
                    }
                    kind if kind.ends_with("_call") => vec![Part::ToolCall {
                        call_id: string(&value, "id"),
                        name: kind.trim_end_matches("_call").into(),
                        input: value.get("arguments").cloned().unwrap_or(Value::Null),
                        kind: CallKind::Hosted,
                        opaque: Some(self.opaque(value)),
                    }],
                    kind if kind.ends_with("_result") => vec![Part::HostedResult {
                        call_id: string(&value, "call_id"),
                        tool: kind.trim_end_matches("_result").into(),
                        payload: value.get("result").cloned().unwrap_or(Value::Null),
                        opaque: Some(self.opaque(value)),
                    }],
                    _ => vec![],
                };
                out.extend(parts.into_iter().map(|part| E::PartDone { part }));
            }
            "interaction.completed" => {
                let interaction = &v["interaction"];
                if let Some(u) = interaction.get("usage").filter(|u| u.is_object()) {
                    let n = |key: &str| u[key].as_u64().unwrap_or(0);
                    let thought = n("total_thought_tokens");
                    out.push(E::Usage(Usage {
                        input: n("total_input_tokens").saturating_sub(n("total_cached_tokens")),
                        output: n("total_output_tokens") + thought,
                        cache_read: n("total_cached_tokens"),
                        reasoning: thought,
                        ..Usage::default()
                    }));
                }
                let reason = match interaction["status"].as_str().unwrap_or_default() {
                    "requires_action" => FinishReason::ToolUse,
                    "completed" if self.tool => FinishReason::ToolUse,
                    "completed" => FinishReason::Stop,
                    "incomplete" => FinishReason::MaxTokens,
                    "failed" => {
                        self.done = true;
                        let message = interaction["errors"][0]["message"].as_str().unwrap_or("interaction failed");
                        return Err(LlmError::new(ErrorClass::Server, message));
                    }
                    other => FinishReason::Other(other.into()),
                };
                out.push(E::Finish { reason, response_id: interaction["id"].as_str().map(Into::into).or_else(|| self.id.clone()) });
                self.done = true;
            }
            "error" => {
                self.done = true;
                let error = &v["error"];
                let code = string(error, "code").to_ascii_lowercase();
                let class = if code.contains("resource_exhausted") || code.contains("rate") {
                    ErrorClass::RateLimited
                } else if code.contains("unavailable") || code.contains("overloaded") {
                    ErrorClass::Overloaded
                } else if code.contains("deadline") || code.contains("timeout") {
                    ErrorClass::Timeout
                } else if code.contains("unauthenticated") || code.contains("permission") {
                    ErrorClass::Auth
                } else if code.contains("invalid") || code.contains("not_found") {
                    ErrorClass::InvalidRequest
                } else {
                    ErrorClass::Server
                };
                return Err(LlmError::new(class, error["message"].as_str().unwrap_or("Gemini stream error")));
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
    Box::pin(futures::stream::unfold(
        (Box::pin(crate::sse::sse_stream(bytes)), State::new(model), VecDeque::new()),
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
                        queue.push_back(Err(LlmError::new(ErrorClass::Network, "Interactions stream ended before completion")));
                    }
                }
            }
        },
    ))
}
