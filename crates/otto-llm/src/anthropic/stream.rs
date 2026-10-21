//! Incremental Messages event accumulation.
use crate::{CallKind, Citation, ErrorClass, EventStream, FinishReason, LlmError, Opaque, Part, StreamEvent, Usage};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

#[derive(Default)]
struct Block {
    value: Value,
    input: String,
}
#[derive(Default)]
struct State {
    blocks: BTreeMap<usize, Block>,
    response_id: Option<String>,
    model: String,
    usage: Usage,
    reason: Option<FinishReason>,
    stopped: bool,
}
/// Decode a Messages byte stream, including terminal errors and incomplete-stream detection.
pub fn decode<S>(bytes: S, model: impl Into<String>) -> EventStream
where
    S: Stream<Item = Result<Bytes, LlmError>> + Send + 'static,
{
    let source = Box::pin(crate::sse::sse_stream(bytes));
    let state = State { model: model.into(), ..State::default() };
    Box::pin(futures::stream::unfold((source, state, VecDeque::new(), false), |(mut source, mut state, mut queue, mut done)| async move {
        loop {
            if let Some(event) = queue.pop_front() {
                return Some((event, (source, state, queue, done)));
            }
            if done {
                return None;
            }
            match source.next().await {
                Some(Ok(event)) => {
                    let result = serde_json::from_str::<Value>(&event.data)
                        .map_err(|e| LlmError::new(ErrorClass::Other, format!("invalid Anthropic event: {e}")))
                        .and_then(|v| state.event(v));
                    match result {
                        Ok(events) => {
                            queue.extend(events.into_iter().map(Ok));
                            done = state.stopped;
                        }
                        Err(error) => {
                            queue.push_back(Err(error));
                            done = true;
                        }
                    }
                }
                Some(Err(error)) => {
                    queue.push_back(Err(error));
                    done = true;
                }
                None => {
                    done = true;
                    if !state.stopped {
                        queue.push_back(Err(LlmError::new(ErrorClass::Network, "Anthropic stream ended before message_stop")));
                    }
                }
            }
        }
    }))
}
fn string(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_owned()
}
impl State {
    fn opaque(&self, data: Value) -> Opaque {
        Opaque { provider: "anthropic".into(), model: self.model.clone(), data }
    }
    fn usage(&mut self, v: &Value) {
        for (field, target) in [
            ("input_tokens", &mut self.usage.input),
            ("output_tokens", &mut self.usage.output),
            ("cache_read_input_tokens", &mut self.usage.cache_read),
            ("cache_creation_input_tokens", &mut self.usage.cache_write),
        ] {
            if let Some(n) = v[field].as_u64() {
                *target = n;
            }
        }
    }
    fn event(&mut self, v: Value) -> Result<Vec<StreamEvent>, LlmError> {
        let mut events = Vec::new();
        let index = v["index"].as_u64().unwrap_or(0) as usize;
        match v["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let m = &v["message"];
                self.response_id = m["id"].as_str().map(str::to_owned);
                if let Some(model) = m["model"].as_str() {
                    self.model = model.into();
                }
                events.push(StreamEvent::Start { response_id: self.response_id.clone(), model: Some(self.model.clone()) });
                self.usage(&m["usage"]);
                events.push(StreamEvent::Usage(self.usage.clone()));
            }
            "content_block_start" => {
                let block = v["content_block"].clone();
                let call_id = string(&block, "id");
                let name = string(&block, "name");
                match block["type"].as_str() {
                    Some("tool_use") => events.push(StreamEvent::ToolInputStart { call_id, name }),
                    Some("server_tool_use") => events.push(StreamEvent::HostedProgress { call_id, tool: name, status: "in_progress".into() }),
                    _ => {}
                }
                if self.blocks.insert(index, Block { value: block, input: String::new() }).is_some() {
                    return Err(protocol("duplicate content block"));
                }
            }
            "content_block_delta" => {
                let block = self.blocks.get_mut(&index).ok_or_else(|| protocol("delta for missing block"))?;
                let delta = &v["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = string(delta, "text");
                        append(&mut block.value, "text", &text);
                        events.push(StreamEvent::TextDelta { text });
                    }
                    Some("thinking_delta") => {
                        let text = string(delta, "thinking");
                        append(&mut block.value, "thinking", &text);
                        events.push(StreamEvent::ReasoningDelta { text, summary: false });
                    }
                    Some("signature_delta") => append(&mut block.value, "signature", &string(delta, "signature")),
                    Some("input_json_delta") => {
                        let delta = string(delta, "partial_json");
                        block.input.push_str(&delta);
                        events.push(StreamEvent::ToolInputDelta { call_id: string(&block.value, "id"), delta });
                    }
                    Some("citations_delta") => {
                        if !block.value["citations"].is_array() {
                            block.value["citations"] = json!([]);
                        }
                        block.value["citations"].as_array_mut().expect("array").push(delta["citation"].clone());
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let mut block = self.blocks.remove(&index).ok_or_else(|| protocol("stop for missing block"))?;
                if !block.input.is_empty() {
                    block.value["input"] = serde_json::from_str(&block.input).map_err(|e| protocol(&format!("invalid tool JSON: {e}")))?;
                }
                let b = block.value;
                let part = match b["type"].as_str().unwrap_or_default() {
                    "text" => Part::Text {
                        text: string(&b, "text"),
                        citations: b["citations"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|c| Citation {
                                url: c["url"].as_str().map(str::to_owned),
                                title: c["title"].as_str().map(str::to_owned),
                                cited_text: c["cited_text"].as_str().map(str::to_owned),
                                raw: Some(self.opaque(c.clone())),
                            })
                            .collect(),
                    },
                    "thinking" | "redacted_thinking" => {
                        Part::Reasoning { summary: None, text: b["thinking"].as_str().map(str::to_owned), opaque: Some(self.opaque(b.clone())) }
                    }
                    "tool_use" | "server_tool_use" => {
                        let hosted = b["type"] == "server_tool_use";
                        Part::ToolCall {
                            call_id: string(&b, "id"),
                            name: string(&b, "name"),
                            input: b.get("input").cloned().unwrap_or_else(|| json!({})),
                            kind: if hosted { CallKind::Hosted } else { CallKind::Function },
                            opaque: if hosted || b.get("caller").is_some() { Some(self.opaque(b.clone())) } else { None },
                        }
                    }
                    "compaction" => Part::Compaction { summary: b["content"].as_str().map(str::to_owned), opaque: Some(self.opaque(b.clone())) },
                    kind => Part::HostedResult {
                        call_id: string(&b, "tool_use_id"),
                        tool: kind.trim_end_matches("_tool_result").into(),
                        payload: b.get("content").cloned().unwrap_or_else(|| b.clone()),
                        opaque: Some(self.opaque(b.clone())),
                    },
                };
                events.push(StreamEvent::PartDone { part });
            }
            "message_delta" => {
                self.usage(&v["usage"]);
                events.push(StreamEvent::Usage(self.usage.clone()));
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    self.reason = Some(match reason {
                        "end_turn" | "stop_sequence" => FinishReason::Stop,
                        "tool_use" => FinishReason::ToolUse,
                        "max_tokens" | "model_context_window_exceeded" => FinishReason::MaxTokens,
                        "pause_turn" => FinishReason::PauseTurn,
                        "refusal" => FinishReason::Refusal,
                        other => FinishReason::Other(other.into()),
                    });
                }
            }
            "message_stop" => {
                if !self.blocks.is_empty() {
                    return Err(protocol("message stopped with unfinished blocks"));
                }
                self.stopped = true;
                events.push(StreamEvent::Finish {
                    reason: self.reason.take().unwrap_or_else(|| FinishReason::Other("unknown".into())),
                    response_id: self.response_id.clone(),
                });
            }
            "error" => {
                let error = &v["error"];
                let class = match error["type"].as_str() {
                    Some("overloaded_error") => ErrorClass::Overloaded,
                    Some("rate_limit_error") => ErrorClass::RateLimited,
                    Some("authentication_error" | "permission_error") => ErrorClass::Auth,
                    Some("invalid_request_error") if string(error, "message").contains("prompt is too long") => ErrorClass::ContextOverflow,
                    Some("invalid_request_error" | "not_found_error" | "billing_error" | "request_too_large") => ErrorClass::InvalidRequest,
                    Some("timeout_error") => ErrorClass::Timeout,
                    Some("api_error") => ErrorClass::Server,
                    _ => ErrorClass::Other,
                };
                return Err(LlmError::new(class, string(error, "message")));
            }
            _ => {}
        }
        Ok(events)
    }
}
fn append(v: &mut Value, key: &str, text: &str) {
    let mut s = string(v, key);
    s.push_str(text);
    v[key] = json!(s);
}
fn protocol(message: &str) -> LlmError {
    LlmError::new(ErrorClass::Other, message)
}
