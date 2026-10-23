#![cfg(feature = "client")]
use futures::StreamExt;
use otto_llm::{
    anthropic::{self, AnthropicConfig, AnthropicModel, convert, stream},
    auth::{AuthMode, Credential, CredentialSource, CredentialStore, now_ms},
    http::{HttpClient, RetryPolicy},
    *,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const TEXT: &str = include_str!("fixtures/anthropic/text.sse");
const THINKING: &str = include_str!("fixtures/anthropic/thinking.sse");
const REDACTED: &str = include_str!("fixtures/anthropic/redacted.sse");
const PARALLEL: &str = include_str!("fixtures/anthropic/parallel.sse");
const SEARCH: &str = include_str!("fixtures/anthropic/web_search.sse");
const MAX: &str = include_str!("fixtures/anthropic/max_tokens.sse");
const ERROR: &str = include_str!("fixtures/anthropic/overloaded.sse");
async fn events(raw: &str, seed: u64) -> Vec<StreamEvent> {
    testing::collect_events(stream::decode(testing::chunked(raw.as_bytes(), seed), "claude-sonnet-4-6")).await
}
fn parts(events: &[StreamEvent]) -> Vec<Part> {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::PartDone { part } => Some(part.clone()),
            _ => None,
        })
        .collect()
}
fn request() -> Request {
    Request { model: "claude-sonnet-4-6".into(), items: vec![Item::user_text("u", "Hello")], ..Request::default() }
}
#[test]
fn adaptive_efforts_clamp_to_what_the_model_takes() {
    for (model, effort, expected) in [
        ("claude-sonnet-4-6", Effort::Xhigh, "max"),
        ("claude-opus-4-6", Effort::Max, "max"),
        ("claude-opus-4-7", Effort::Xhigh, "xhigh"),
        ("claude-opus-4-7", Effort::Max, "max"),
        ("claude-opus-5-5", Effort::Minimal, "low"),
    ] {
        let req = Request { model: model.into(), reasoning: Some(Reasoning::Effort(effort)), ..request() };
        let body = convert::body(&req).unwrap();
        assert_eq!(
            (body["thinking"]["type"].as_str(), body["output_config"]["effort"].as_str()),
            (Some("adaptive"), Some(expected)),
            "{model} {effort:?}"
        );
    }
}
#[tokio::test]
async fn fixtures_are_chunk_independent() {
    for raw in [TEXT, THINKING, REDACTED, PARALLEL, SEARCH, MAX] {
        let expected = events(raw, 0).await;
        assert!(matches!(expected.first(), Some(StreamEvent::Start { .. })));
        assert!(matches!(expected.last(), Some(StreamEvent::Finish { .. })));
        for seed in [1, 7, 42, 100, 999] {
            assert_eq!(events(raw, seed).await, expected);
        }
    }
    let text = events(TEXT, 0).await;
    assert_eq!(parts(&text), vec![Part::text("Hello!")]);
    assert!(text.contains(&StreamEvent::Usage(Usage { input: 10, output: 3, cache_read: 20, cache_write: 30, reasoning: 0 })));
    let tools = parts(&events(PARALLEL, 0).await);
    assert!(matches!(&tools[0], Part::ToolCall { input, call_id, .. } if *input == json!({"city":"Paris"}) && call_id == "call_a"));
    assert!(matches!(&tools[1], Part::ToolCall { input, .. } if *input == json!({})));
    assert!(matches!(events(MAX, 0).await.last(), Some(StreamEvent::Finish { reason: FinishReason::MaxTokens, .. })));
}
#[tokio::test]
async fn opaque_replay_is_unchanged() {
    for (raw, expected) in [
        (THINKING, json!({"type":"thinking","thinking":"Check the weather.","signature":"EqQBCgIY+abc/="})),
        (REDACTED, json!({"type":"redacted_thinking","data":"AbCd+/=Encrypted"})),
    ] {
        let parts = parts(&events(raw, 3).await);
        let mut req = request();
        req.items.push(Item::new("a", Role::Assistant, parts));
        let body = convert::body(&req).unwrap();
        assert_eq!(serde_json::to_vec(&body["messages"][1]["content"][0]).unwrap(), serde_json::to_vec(&expected).unwrap());
    }
    let result = events(SEARCH, 0).await;
    assert!(result.iter().any(|e| matches!(e, StreamEvent::HostedProgress { tool, .. } if tool == "web_search")));
    let blocks = parts(&result);
    assert!(matches!(&blocks[0], Part::ToolCall { kind: CallKind::Hosted, opaque: Some(_), .. }));
    assert!(matches!(&blocks[1], Part::HostedResult { opaque: Some(_), .. }));
    assert!(matches!(&blocks[2], Part::Text { citations, .. } if citations.len() == 1 && citations[0].raw.is_some()));
}
#[tokio::test]
async fn terminal_errors_and_finish_reasons() {
    for seed in [0, 1, 10, 42] {
        let events: Vec<_> = stream::decode(testing::chunked(ERROR.as_bytes(), seed), "model").collect().await;
        assert_eq!(events.last().unwrap().as_ref().unwrap_err().class, ErrorClass::Overloaded);
        assert_eq!(events.len(), 3);
    }
    for (wire, reason) in
        [("pause_turn", FinishReason::PauseTurn), ("refusal", FinishReason::Refusal), ("model_context_window_exceeded", FinishReason::MaxTokens)]
    {
        assert!(matches!(events(&MAX.replace("max_tokens", wire), 0).await.last(), Some(StreamEvent::Finish { reason: got, .. }) if *got == reason));
    }
    let truncated: Vec<_> = stream::decode(testing::chunked(b"", 0), "m").collect().await;
    assert_eq!(truncated[0].as_ref().unwrap_err().class, ErrorClass::Network);
}
#[test]
fn alternation_cache_tools_and_effort() {
    let mut req = request();
    req.system = vec!["stable".into(), "dynamic".into()];
    req.items.extend([
        Item::new("t", Role::Tool, vec![Part::ToolResult { call_id: "c".into(), content: vec![ResultPart::text("ok")], is_error: false }]),
        Item::new("s", Role::System, vec![Part::System { text: "reminder".into() }]),
    ]);
    req.tools = vec![ToolDef {
        name: "web_search".into(),
        description: "".into(),
        input_schema: Value::Null,
        kind: ToolKind::Hosted { tool: "web_search_20250305".into(), config: json!({"max_uses":2}) },
        defer_loading: true,
    }];
    req.parallel_tools = Some(false);
    req.cache.long_ttl = true;
    req.temperature = Some(0.5);
    req.reasoning = Some(Reasoning::Effort(Effort::High));
    req.extra = Some(json!({"output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}}));
    let body = convert::body(&req).unwrap();
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(body["messages"][0]["content"][0]["type"], "tool_result");
    assert!(body["messages"][0]["content"][2]["text"].as_str().unwrap().contains("system-reminder"));
    assert_eq!(body["tools"][0]["max_uses"], 2);
    assert_eq!(body["tools"][0]["defer_loading"], true);
    assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["thinking"]["display"], "summarized");
    assert_eq!(body["output_config"]["effort"], "high");
    assert!(body.get("temperature").is_none());
    assert_eq!(body.to_string().matches("cache_control").count(), 3);
    assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");
    req.model = "claude-opus-5-5".into();
    let body = convert::body(&req).unwrap();
    assert_eq!(body["messages"][1]["role"], "system");
    assert!(body.to_string().matches("cache_control").count() <= 4);
    // A system item right after a reply (a subagent report waking the parent) can't be a system message.
    let mut woken = req.clone();
    woken.items = vec![
        Item::user_text("u", "go"),
        Item::new("a", Role::Assistant, vec![Part::text("delegated")]),
        Item::new("s", Role::System, vec![Part::System { text: "Subagent completion".into() }]),
    ];
    let body = convert::body(&woken).unwrap();
    assert_eq!(body["messages"][2]["role"], "user");
    assert!(body["messages"][2]["content"][0]["text"].as_str().unwrap().contains("<system-reminder>\nSubagent completion"));
    req.model = "claude-haiku-4-5-20251001".into();
    assert_eq!(convert::body(&req).unwrap()["thinking"]["budget_tokens"], 8192);
    req.cache.disabled = true;
    assert!(!convert::body(&req).unwrap().to_string().contains("cache_control"));
}
#[test]
fn native_unknown_blocks_survive() {
    let block = json!({"type":"future_tool_result","opaque":"keep","nested":{"a":1}});
    let decoded: anthropic::api::ContentBlock = serde_json::from_value(block.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), block);
}
fn credential(access: &str) -> Credential {
    Credential::OAuth { access: access.into(), refresh: "refresh".into(), expires_at_ms: now_ms() + 3_600_000, account_id: None, extra: Value::Null }
}
#[tokio::test]
async fn http_401_refresh_retry_and_count_tokens() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for n in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let header_end = loop {
                let mut buf = [0; 4096];
                let read = socket.read(&mut buf).await.unwrap();
                assert!(read > 0);
                raw.extend_from_slice(&buf[..read]);
                if let Some(pos) = raw.windows(4).position(|v| v == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&raw[..header_end]).to_lowercase();
            let length: usize = headers.lines().find_map(|l| l.strip_prefix("content-length: ")).unwrap().trim().parse().unwrap();
            while raw.len() < header_end + length {
                let mut buf = [0; 4096];
                let read = socket.read(&mut buf).await.unwrap();
                assert!(read > 0);
                raw.extend_from_slice(&buf[..read]);
            }
            let body: Value = serde_json::from_slice(&raw[header_end..]).unwrap();
            assert!(headers.contains("?beta=true"));
            assert!(headers.contains(if n == 0 { "authorization: bearer old" } else { "authorization: bearer new" }));
            assert!(!headers.contains("x-api-key"));
            assert_eq!(body["system"][0]["text"], anthropic::subscription::SYSTEM_PREFIX);
            let (status, content_type, response) = match n {
                0 => (
                    "401 Unauthorized",
                    "application/json",
                    "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"expired\"}}",
                ),
                1 => ("200 OK", "text/event-stream", TEXT),
                _ => {
                    assert!(headers.contains("/v1/messages/count_tokens"));
                    assert!(body.get("max_tokens").is_none());
                    ("200 OK", "application/json", "{\"input_tokens\":42}")
                }
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let dir = std::env::temp_dir().join(format!("otto-anthropic-test-{}", rand::random::<u64>()));
    let store = CredentialStore::open(&dir).unwrap();
    store.set("anthropic", credential("old")).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let auth = CredentialSource::store(
        store,
        "anthropic",
        Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(credential("new")) })
        }),
    );
    let model = AnthropicModel::new(AnthropicConfig {
        base_url: endpoint,
        auth,
        mode: AuthMode::Subscription,
        model: anthropic::model_info("claude-sonnet-4-6").unwrap(),
        betas: vec![],
        http: HttpClient::new().unwrap(),
        retry: RetryPolicy::default(),
        idle_timeout: Duration::from_secs(5),
    });
    let events = testing::collect_events(model.stream(request(), CallCx::default()).await.unwrap()).await;
    assert_eq!(parts(&events), vec![Part::text("Hello!")]);
    assert_eq!(model.count_tokens(&request()).await.unwrap(), Some(42));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn media_strict_tools_inline_changes_and_foreign_reasoning() {
    let mut req = request();
    req.model = "claude-opus-5-5".into();
    req.tools = vec![ToolDef {
        name: "read".into(),
        description: "Read".into(),
        input_schema: json!({"type":"object"}),
        kind: ToolKind::Strict,
        defer_loading: true,
    }];
    req.items.push(Item::new("s", Role::System, vec![Part::ToolsChanged { added: vec!["read".into()], removed: vec![] }]));
    req.items.push(Item::new(
        "a",
        Role::Assistant,
        vec![Part::Reasoning {
            text: Some("foreign".into()),
            summary: None,
            opaque: Some(Opaque { provider: "openai".into(), model: "other".into(), data: json!({"type":"reasoning"}) }),
        }],
    ));
    req.items.push(Item::new(
        "media",
        Role::User,
        vec![
            Part::Image(Media { mime: "image/png".into(), source: MediaSource::Base64 { data: "YWJj".into() }, name: None }),
            Part::Document(Media {
                mime: "application/pdf".into(),
                source: MediaSource::File { provider: "anthropic".into(), id: "file_1".into() },
                name: None,
            }),
        ],
    ));
    let body = convert::body(&req).unwrap();
    assert_eq!(body["tools"][0]["strict"], true);
    assert_eq!(
        body["messages"][1]["content"][0],
        json!({"type":"tool_addition","tool":{"type":"tool_reference","name":"read"},"cache_control":{"type":"ephemeral"}})
    );
    assert_eq!(body["messages"][2]["content"][0]["source"]["media_type"], "image/png");
    assert_eq!(body["messages"][2]["content"][1]["source"]["file_id"], "file_1");
    assert!(!body.to_string().contains("foreign"));
    let native: anthropic::api::MessagesRequest = serde_json::from_value(body).unwrap();
    assert_eq!(native.tools[0].strict, Some(true));
}

#[tokio::test]
#[ignore = "requires OTTO_LIVE=1 and ANTHROPIC_API_KEY; incurs API charges"]
async fn live_and_optional_recording() {
    if std::env::var("OTTO_LIVE").as_deref() != Ok("1") {
        return;
    }
    let Ok(key) = std::env::var("ANTHROPIC_API_KEY") else {
        return;
    };
    let http = HttpClient::new().unwrap();
    let mut req = request();
    req.max_output = Some(64);
    let body = convert::body(&req).unwrap();
    let cx = CallCx::default();
    let response = otto_llm::http::send_with_retry(
        || {
            http.client()
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
        },
        &RetryPolicy::default(),
        &cx.cancel,
    )
    .await
    .unwrap();
    let stream = otto_llm::http::byte_stream_with_idle_timeout(response, Duration::from_secs(60), cx.cancel);
    futures::pin_mut!(stream);
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    if std::env::var("OTTO_RECORD").as_deref() == Ok("1") {
        std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/anthropic/live.sse"), &bytes).unwrap();
    }
    let events = testing::collect_events(stream::decode(testing::chunked(&bytes, 0), req.model)).await;
    assert!(matches!(events.last(), Some(StreamEvent::Finish { .. })));
}
