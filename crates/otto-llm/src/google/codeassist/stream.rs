//! Cloud Code Assist SSE (`data: {"response":{candidates,usageMetadata,…}}`) into [`StreamEvent`]s.
//! Text and thought runs are assembled into parts; the finish reason and usage may arrive in
//! separate chunks, so the finish is emitted when the stream ends.
use super::convert::PROVIDER;
use crate::{CallKind, ErrorClass, EventStream, FinishReason, LlmError, Opaque, Part, StreamEvent as E, Usage};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use std::collections::VecDeque;

enum Block {
    Text(String),
    Thought { text: String, signature: Option<String> },
}

pub struct State {
    model: String,
    id: Option<String>,
    started: bool,
    block: Option<Block>,
    tool: bool,
    reason: Option<String>,
    pub done: bool,
}

/// An `{"error":{code,status,message}}` body.
fn api_error(error: &Value) -> LlmError {
    let code = error["code"].as_u64();
    let status = error["status"].as_str().unwrap_or_default();
    let class = match (code, status) {
        (Some(429), _) | (_, "RESOURCE_EXHAUSTED") => ErrorClass::RateLimited,
        (Some(503), _) | (_, "UNAVAILABLE") => ErrorClass::Overloaded,
        (Some(401 | 403), _) | (_, "UNAUTHENTICATED" | "PERMISSION_DENIED") => ErrorClass::Auth,
        (Some(400 | 404), _) | (_, "INVALID_ARGUMENT" | "NOT_FOUND") => ErrorClass::InvalidRequest,
        (Some(504), _) | (_, "DEADLINE_EXCEEDED") => ErrorClass::Timeout,
        _ => ErrorClass::Server,
    };
    let error_out = LlmError::new(class, error["message"].as_str().unwrap_or("Antigravity stream error"));
    match code.and_then(|c| u16::try_from(c).ok()) {
        Some(code) => error_out.with_status(code),
        None => error_out,
    }
}

impl State {
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self { model: model.into(), id: None, started: false, block: None, tool: false, reason: None, done: false }
    }
    fn opaque(&self, data: Value) -> Opaque {
        Opaque { provider: PROVIDER.into(), model: self.model.clone(), data }
    }
    /// The run being assembled, as its part.
    fn flush(&mut self) -> Vec<E> {
        match self.block.take() {
            Some(Block::Text(text)) if !text.is_empty() => vec![E::PartDone { part: Part::text(text) }],
            Some(Block::Thought { text, signature }) if !text.is_empty() || signature.is_some() => {
                let mut data = json!({"thought":true,"text":text});
                if let Some(signature) = &signature {
                    data["thoughtSignature"] = json!(signature);
                }
                let summary = (!text.is_empty()).then_some(text);
                vec![E::PartDone { part: Part::Reasoning { summary, text: None, opaque: Some(self.opaque(data)) } }]
            }
            _ => Vec::new(),
        }
    }
    /// Translate one SSE payload.
    ///
    /// # Errors
    /// An `error` payload.
    pub fn event(&mut self, v: &Value) -> Result<Vec<E>, LlmError> {
        if let Some(error) = v.get("error") {
            self.done = true;
            return Err(api_error(error));
        }
        let r = v.get("response").filter(|r| r.is_object()).unwrap_or(v);
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            self.id = r["responseId"].as_str().map(Into::into);
            out.push(E::Start { response_id: self.id.clone(), model: Some(r["modelVersion"].as_str().unwrap_or(&self.model).into()) });
        }
        let candidate = &r["candidates"][0];
        for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
            let signature = part["thoughtSignature"].as_str().filter(|s| !s.is_empty()).map(String::from);
            if let Some(call) = part.get("functionCall") {
                out.extend(self.flush());
                out.extend(self.call(call, signature));
            } else if let Some(text) = part["text"].as_str() {
                let thought = part["thought"] == true;
                let same = matches!((&self.block, thought), (Some(Block::Thought { .. }), true) | (Some(Block::Text(_)), false));
                if !same {
                    out.extend(self.flush());
                    self.block = Some(if thought { Block::Thought { text: String::new(), signature: None } } else { Block::Text(String::new()) });
                }
                match &mut self.block {
                    Some(Block::Thought { text: run, signature: sig }) => {
                        run.push_str(text);
                        if signature.is_some() {
                            *sig = signature;
                        }
                    }
                    Some(Block::Text(run)) => run.push_str(text),
                    None => {}
                }
                if !text.is_empty() {
                    out.push(if thought { E::ReasoningDelta { text: text.into(), summary: true } } else { E::TextDelta { text: text.into() } });
                }
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.reason = Some(reason.into());
        }
        if let Some(u) = r.get("usageMetadata").filter(|u| u.is_object()) {
            let n = |key: &str| u[key].as_u64().unwrap_or(0);
            let (cached, thoughts) = (n("cachedContentTokenCount"), n("thoughtsTokenCount"));
            out.push(E::Usage(Usage {
                input: n("promptTokenCount").saturating_sub(cached),
                output: n("candidatesTokenCount") + thoughts,
                cache_read: cached,
                reasoning: thoughts,
                ..Usage::default()
            }));
        }
        Ok(out)
    }
    fn call(&mut self, call: &Value, signature: Option<String>) -> Vec<E> {
        self.tool = true;
        let name = call["name"].as_str().unwrap_or_default().to_owned();
        let cleaned: String = call["id"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .take(64)
            .collect();
        let call_id = if cleaned.is_empty() { format!("{name}_{:08x}", rand::random::<u32>()) } else { cleaned };
        let input = call.get("args").filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
        vec![
            E::ToolInputStart { call_id: call_id.clone(), name: name.clone() },
            E::ToolInputDelta { call_id: call_id.clone(), delta: input.to_string() },
            E::PartDone {
                part: Part::ToolCall {
                    call_id,
                    name,
                    input,
                    kind: CallKind::Function,
                    opaque: signature.map(|s| self.opaque(json!({"thoughtSignature":s}))),
                },
            },
        ]
    }
    /// The stream ended: the last run, then the finish.
    ///
    /// # Errors
    /// No finish reason was seen, i.e. the response was cut off.
    pub fn end(&mut self) -> Result<Vec<E>, LlmError> {
        self.done = true;
        let Some(reason) = self.reason.take() else {
            return Err(LlmError::new(ErrorClass::Network, "Antigravity stream ended before completion"));
        };
        let mut out = self.flush();
        let reason = if self.tool {
            FinishReason::ToolUse
        } else {
            match reason.as_str() {
                "STOP" => FinishReason::Stop,
                "MAX_TOKENS" => FinishReason::MaxTokens,
                "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => FinishReason::ContentFilter,
                other => FinishReason::Other(other.into()),
            }
        };
        out.push(E::Finish { reason, response_id: self.id.clone() });
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
                let result = match source.next().await {
                    Some(Ok(event)) if event.data == "[DONE]" => continue,
                    Some(Ok(event)) => {
                        serde_json::from_str(&event.data).map_err(|e| LlmError::new(ErrorClass::Other, e.to_string())).and_then(|v| state.event(&v))
                    }
                    Some(Err(e)) => Err(e),
                    None => state.end(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{chunked, collect_events};

    const TOOLS: &str = concat!(
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Plan.","thought":true}]}}],"usageMetadata":{"promptTokenCount":10,"totalTokenCount":10},"modelVersion":"gemini-3.8-flash-high","responseId":"r1"},"traceId":"t"}"#,
        "\n\n",
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":" more","thought":true,"thoughtSignature":"c2ln"}]}}]}}"#,
        "\r\n\r\n",
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]}}]}}"#,
        "\n\n",
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]}}]}}"#,
        "\n\n",
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"read","args":{"path":"a"}},"thoughtSignature":"c2lnMg=="},{"functionCall":{"id":"call.2","name":"ls","args":{}}}]}}]}}"#,
        "\n\n",
        r#"data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"cachedContentTokenCount":40,"thoughtsTokenCount":5,"totalTokenCount":125}}}"#,
        "\n\n",
    );

    async fn events(raw: &str, seed: u64) -> Vec<E> {
        collect_events(stream(chunked(raw.as_bytes(), seed), "gemini-3.8-flash-high")).await
    }
    fn parts(events: &[E]) -> Vec<Part> {
        events.iter().filter_map(|e| if let E::PartDone { part } = e { Some(part.clone()) } else { None }).collect()
    }

    #[tokio::test]
    async fn thoughts_text_and_signed_calls() {
        for seed in 0..10 {
            let events = events(TOOLS, seed).await;
            assert_eq!(events[0], E::Start { response_id: Some("r1".into()), model: Some("gemini-3.8-flash-high".into()) });
            assert!(events.contains(&E::ReasoningDelta { text: "Plan.".into(), summary: true }));
            assert!(events.contains(&E::TextDelta { text: " world".into() }));
            let parts = parts(&events);
            assert_eq!(parts.len(), 4, "{parts:?}");
            let Part::Reasoning { summary, opaque: Some(thought), .. } = &parts[0] else { panic!("{parts:?}") };
            assert_eq!(summary.as_deref(), Some("Plan. more"));
            assert_eq!(thought.data, json!({"thought":true,"text":"Plan. more","thoughtSignature":"c2ln"}));
            assert_eq!((thought.provider.as_str(), thought.model.as_str()), (PROVIDER, "gemini-3.8-flash-high"));
            assert_eq!(parts[1], Part::text("Hello world"));
            let Part::ToolCall { call_id, name, input, opaque, .. } = &parts[2] else { panic!("{parts:?}") };
            assert!(call_id.starts_with("read_") && name == "read" && *input == json!({"path":"a"}));
            assert_eq!(opaque.as_ref().unwrap().data, json!({"thoughtSignature":"c2lnMg=="}));
            assert!(events.contains(&E::ToolInputStart { call_id: call_id.clone(), name: "read".into() }));
            assert!(events.contains(&E::ToolInputDelta { call_id: call_id.clone(), delta: r#"{"path":"a"}"#.into() }));
            // The API's own id is kept, cleaned; no signature means no opaque.
            assert!(matches!(&parts[3], Part::ToolCall { call_id, name, opaque: None, .. } if call_id == "call_2" && name == "ls"));
            let usage: Vec<_> = events.iter().filter_map(|e| if let E::Usage(u) = e { Some(u.clone()) } else { None }).collect();
            assert_eq!(usage.last().unwrap(), &Usage { input: 60, output: 25, cache_read: 40, cache_write: 0, reasoning: 5 });
            assert_eq!(events.last(), Some(&E::Finish { reason: FinishReason::ToolUse, response_id: Some("r1".into()) }));
        }
    }

    #[tokio::test]
    async fn finish_reasons() {
        let chunk = |parts: &str, reason: &str| {
            format!("data: {{\"candidates\":[{{\"content\":{{\"parts\":[{parts}]}},\"finishReason\":\"{reason}\"}}]}}\n\ndata: [DONE]\n\n")
        };
        let last = |raw: String| async move { events(&raw, 3).await.pop().unwrap() };
        assert_eq!(last(chunk(r#"{"text":"hi"}"#, "STOP")).await, E::Finish { reason: FinishReason::Stop, response_id: None });
        assert_eq!(last(chunk(r#"{"text":"hi"}"#, "MAX_TOKENS")).await, E::Finish { reason: FinishReason::MaxTokens, response_id: None });
        assert_eq!(last(chunk("", "SAFETY")).await, E::Finish { reason: FinishReason::ContentFilter, response_id: None });
        assert_eq!(
            last(chunk("", "MALFORMED_FUNCTION_CALL")).await,
            E::Finish { reason: FinishReason::Other("MALFORMED_FUNCTION_CALL".into()), response_id: None }
        );
    }

    #[tokio::test]
    async fn errors_and_cut_off_streams() {
        let cut = &TOOLS[..TOOLS.len() - 300];
        let items: Vec<_> = stream(chunked(cut.as_bytes(), 1), "m").collect().await;
        assert_eq!(items.last().unwrap().as_ref().unwrap_err().class, ErrorClass::Network);
        let quota = "data: {\"error\":{\"code\":429,\"message\":\"Individual quota reached. Resets in 2h\",\"status\":\"RESOURCE_EXHAUSTED\"}}\n\n";
        let items: Vec<_> = stream(chunked(quota.as_bytes(), 2), "m").collect().await;
        let error = items.last().unwrap().as_ref().unwrap_err();
        assert_eq!((error.class, error.status), (ErrorClass::RateLimited, Some(429)));
        assert!(error.message.contains("Resets in 2h"));
        let items: Vec<_> = stream(chunked(b"data: {not json}\n\n", 2), "m").collect().await;
        assert_eq!(items.last().unwrap().as_ref().unwrap_err().class, ErrorClass::Other);
        assert_eq!(api_error(&json!({"status":"UNAVAILABLE","message":"x"})).class, ErrorClass::Overloaded);
        assert_eq!(api_error(&json!({"code":403})).class, ErrorClass::Auth);
    }
}
