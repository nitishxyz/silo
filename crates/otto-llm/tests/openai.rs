#![cfg(feature = "client")]
use futures::StreamExt;
use otto_llm::{
    auth::{Credential, CredentialSource, CredentialStore, now_ms},
    http::{HttpClient, RetryPolicy},
    openai::{self, convert, stream, subscription},
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
const TEXT: &str = include_str!("fixtures/openai/text.sse");
const REASON: &str = include_str!("fixtures/openai/reasoning.sse");
async fn events(raw: &str, seed: u64) -> Vec<StreamEvent> {
    testing::collect_events(stream::stream(testing::chunked(raw.as_bytes(), seed), "gpt-5.2")).await
}

#[tokio::test]
async fn default_effort_stateless_tool_loop() {
    let mut req = Request { model: "gpt-5.2".into(), items: vec![Item::user_text("u", "Read the file")], ..Request::default() };
    let body = convert::request(&req);
    assert!(body.get("reasoning").is_none());
    assert_eq!(body["store"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    for seed in 0..20 {
        let parts: Vec<_> = events(REASON, seed)
            .await
            .into_iter()
            .filter_map(|e| match e {
                StreamEvent::PartDone { part } => Some(part),
                _ => None,
            })
            .collect();
        let Part::Reasoning { opaque: Some(opaque), .. } = &parts[0] else { panic!("missing reasoning") };
        let original = opaque.data.clone();
        req.items.truncate(1);
        req.items.push(Item::new("a", Role::Assistant, parts));
        req.items.push(Item::new(
            "t",
            Role::Tool,
            vec![Part::ToolResult { call_id: "call_1".into(), content: vec![ResultPart::text("file contents")], is_error: false }],
        ));
        let mut replay = convert::request(&req);
        assert_eq!(replay["input"][1], original);
        assert_eq!(replay["input"][3]["type"], "function_call_output");
        subscription::shape(&mut replay, None);
        assert_eq!(replay["input"][1]["encrypted_content"], original["encrypted_content"]);
        assert_eq!(replay["include"], json!(["reasoning.encrypted_content"]));
    }
}

#[test]
fn stateless_drops_unusable_reasoning_and_preserves_inclusions() {
    for encrypted in [None, Some(Value::Null), Some(json!(""))] {
        let mut reasoning = json!({"type":"reasoning","id":"rs_old","summary":[{"type":"summary_text","text":"summary"}]});
        if let Some(encrypted) = encrypted {
            reasoning["encrypted_content"] = encrypted;
        }
        let mut req = Request {
            items: vec![Item::new(
                "a",
                Role::Assistant,
                vec![
                    Part::Reasoning {
                        summary: Some("summary".into()),
                        text: None,
                        opaque: Some(Opaque { provider: "openai".into(), model: "gpt-5.2".into(), data: reasoning.clone() }),
                    },
                    Part::text("answer"),
                ],
            )],
            extra: Some(json!({"include":["web_search_call.action.sources"]})),
            ..Request::default()
        };
        let body = convert::request(&req);
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["include"], json!(["web_search_call.action.sources", "reasoning.encrypted_content"]));
        req.extra = Some(json!({"store":true,"include":[]}));
        let mut body = convert::request(&req);
        assert_eq!(body["input"][0], reasoning);
        subscription::shape(&mut body, None);
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }
}

#[tokio::test]
async fn refusal_content_deltas_finish_and_replay() {
    let raw = include_str!("fixtures/openai/refusal.sse");
    let expected = events(raw, 0).await;
    for seed in 1..20 {
        assert_eq!(events(raw, seed).await, expected);
    }
    let text: String = expected
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "I cannot help with that.");
    let parts: Vec<_> = expected
        .iter()
        .filter_map(|e| match e {
            StreamEvent::PartDone { part } => Some(part.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(parts, vec![Part::text(&text)]);
    assert!(matches!(expected.last(), Some(StreamEvent::Finish { reason: FinishReason::Refusal, .. })));
    let body = convert::request(&Request { items: vec![Item::new("a", Role::Assistant, parts)], ..Request::default() });
    assert_eq!(body["input"][0]["role"], "assistant");
    assert_eq!(body["input"][0]["content"][0], json!({"type":"output_text","text":text}));
    // A completed refusal message is sufficient even without display events.
    let without_deltas = raw.lines().filter(|line| !line.contains("response.refusal.")).collect::<Vec<_>>().join("\n") + "\n";
    let result = events(&without_deltas, 1).await;
    assert!(matches!(result.last(), Some(StreamEvent::Finish { reason: FinishReason::Refusal, .. })));
    assert_eq!(result.iter().filter(|e| matches!(e, StreamEvent::PartDone { .. })).count(), 1);
}

#[tokio::test]
async fn fixtures_and_replay() {
    for raw in [
        TEXT,
        REASON,
        include_str!("fixtures/openai/parallel.sse"),
        include_str!("fixtures/openai/custom.sse"),
        include_str!("fixtures/openai/web.sse"),
        include_str!("fixtures/openai/incomplete.sse"),
    ] {
        let expected = events(raw, 0).await;
        for seed in 1..20 {
            assert_eq!(events(raw, seed).await, expected);
        }
        assert!(matches!(expected.first(), Some(StreamEvent::Start { .. })));
        assert!(matches!(expected.last(), Some(StreamEvent::Finish { .. })));
    }
    let e = events(REASON, 0).await;
    assert!(e.contains(&StreamEvent::Usage(Usage { input: 60, output: 20, cache_read: 40, reasoning: 15, ..Usage::default() })));
    let parts: Vec<_> = e.into_iter().filter_map(|e| if let StreamEvent::PartDone { part } = e { Some(part) } else { None }).collect();
    let Part::Reasoning { opaque: Some(o), .. } = &parts[0] else { panic!("reasoning missing") };
    let original = serde_json::to_vec(&o.data).unwrap();
    let req = Request { items: vec![Item::new("a", Role::Assistant, parts)], ..Request::default() };
    assert_eq!(serde_json::to_vec(&convert::request(&req)["input"][0]).unwrap(), original);
    let web = events(include_str!("fixtures/openai/web.sse"), 0).await;
    assert!(web.iter().any(
        |e| matches!(e,StreamEvent::PartDone { part:Part::Text { citations,.. } } if citations[0].url.as_deref()==Some("https://rust-lang.org"))
    ));
    let short = events(include_str!("fixtures/openai/incomplete.sse"), 0).await;
    assert!(matches!(short.last(), Some(StreamEvent::Finish { reason: FinishReason::MaxTokens, .. })));
    let parallel = events(include_str!("fixtures/openai/parallel.sse"), 0).await;
    assert!(parallel.contains(&StreamEvent::ToolInputDelta { call_id: "call_b".into(), delta: "{}".into() }));
}
#[tokio::test]
async fn errors_and_truncation() {
    for seed in 0..20 {
        let e: Vec<_> = stream::stream(testing::chunked(include_bytes!("fixtures/openai/failed.sse"), seed), "gpt-5.2").collect().await;
        assert_eq!(e.last().unwrap().as_ref().unwrap_err().class, ErrorClass::Server);
    }
    let e: Vec<_> = stream::stream(testing::chunked(b"data: {}\n\n", 0), "gpt-5.2").collect().await;
    assert_eq!(e.last().unwrap().as_ref().unwrap_err().class, ErrorClass::Network);
}
#[test]
fn strict_schema_and_effort() {
    let mut schema = json!({"type":"object","properties":{"a":{"type":"string"},"b":{"type":"array","items":{"type":"object","properties":{"c":{"type":"integer"}}}}},"required":["a"]});
    convert::strictify(&mut schema);
    assert_eq!(schema["required"], json!(["a", "b"]));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["a"]["type"], "string");
    assert_eq!(schema["properties"]["b"]["anyOf"][0]["items"]["additionalProperties"], false);
    assert_eq!(schema["properties"]["b"]["anyOf"][0]["items"]["properties"]["c"]["anyOf"][1]["type"], "null");
    for (n, expected) in [(0, "none"), (512, "minimal"), (2000, "low"), (8000, "medium"), (20000, "high"), (50000, "xhigh")] {
        assert_eq!(convert::effort(Reasoning::Budget(n)), expected);
    }
    assert_eq!(convert::effort(Reasoning::Effort(Effort::Max)), "xhigh");
}
#[test]
fn conversion_and_subscription() {
    let req = Request {
        model: "gpt-5.2".into(),
        system: vec!["one".into(), "two".into()],
        max_output: Some(100),
        temperature: Some(0.3),
        reasoning: Some(Reasoning::Effort(Effort::Low)),
        items: vec![
            Item::new(
                "a",
                Role::Assistant,
                vec![Part::ToolCall {
                    call_id: "c".into(),
                    name: "apply_patch".into(),
                    input: json!("raw patch"),
                    kind: CallKind::Custom,
                    opaque: None,
                }],
            ),
            Item::new("b", Role::Tool, vec![Part::ToolResult { call_id: "c".into(), content: vec![ResultPart::text("done")], is_error: false }]),
            Item::new("c", Role::System, vec![Part::System { text: "mid".into() }]),
        ],
        extra: Some(json!({"reasoning":{"summary":"detailed"}})),
        ..Request::default()
    };
    let mut body = convert::request(&req);
    assert_eq!(body["input"][0]["input"], "raw patch");
    assert_eq!(body["input"][1]["type"], "custom_tool_call_output");
    assert_eq!(body["input"][2]["role"], "developer");
    assert_eq!(body["instructions"], "one\n\ntwo");
    assert_eq!(body["reasoning"], json!({"effort":"low","summary":"detailed"}));
    subscription::shape(&mut body, Some("session"));
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("temperature").is_none());
    assert_eq!(body["prompt_cache_key"], "session");
    assert_eq!(body["store"], false);
    let headers = subscription::headers(&token("access"), Some("session")).unwrap();
    assert_eq!(headers["authorization"], "Bearer access");
    assert_eq!(headers["originator"], "otto");
    assert_eq!(headers["session_id"], "session");
    assert_eq!(headers["chatgpt-account-id"], "acct");
    assert!(headers.contains_key("x-codex-installation-id"));
    let unknown = json!({"type":"future_item","anything":[1,2]});
    let item: openai::api::Item = serde_json::from_value(unknown.clone()).unwrap();
    assert_eq!(serde_json::to_value(item).unwrap(), unknown);
}
fn token(access: &str) -> Credential {
    Credential::OAuth {
        access: access.into(),
        refresh: "refresh".into(),
        expires_at_ms: now_ms() + 3_600_000,
        account_id: Some("acct".into()),
        extra: Value::Null,
    }
}
#[tokio::test]
async fn http_unauthorized_refresh_retry() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..end]);
                    let size: usize =
                        head.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").map(|s| s.parse().unwrap())).unwrap();
                    if bytes.len() >= end + 4 + size {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&bytes);
            assert!(text.contains(if attempt == 0 { "Bearer old" } else { "Bearer new" }));
            let (status, body) = if attempt == 0 { ("401 Unauthorized", "{\"error\":{\"message\":\"expired\"}}") } else { ("200 OK", TEXT) };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let dir = std::env::temp_dir().join(format!("otto-openai-{}", rand::random::<u64>()));
    let store = CredentialStore::open(&dir).unwrap();
    store.set("openai", token("old")).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let auth = CredentialSource::store(
        store,
        "openai",
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(token("new")) })
        }),
    );
    let model = openai::OpenAiModel::new(openai::OpenAiConfig {
        base_url: format!("http://{address}/v1"),
        auth,
        mode: openai::AuthMode::ApiKey,
        model: openai::model_info("gpt-5.2").unwrap(),
        http: HttpClient::new().unwrap(),
        retry: RetryPolicy::default(),
        idle_timeout: Duration::from_secs(5),
        organization: None,
        project: None,
    });
    let e = testing::collect_events(model.stream(Request::default(), CallCx::default()).await.unwrap()).await;
    assert!(matches!(e.last(), Some(StreamEvent::Finish { reason: FinishReason::Stop, .. })));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    task.await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn oauth_claims_and_rejection() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let payload = URL_SAFE_NO_PAD.encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account"}}"#);
    assert_eq!(subscription::account_id(&format!("header.{payload}.signature")).as_deref(), Some("account"));
    assert!(subscription::account_id("not-a-jwt").is_none());
    assert!(subscription::is_refresh_rejection(&LlmError::new(ErrorClass::InvalidRequest, "invalid_grant")));
}

#[tokio::test]
#[ignore = "requires OTTO_LIVE=1 and OPENAI_API_KEY; incurs API charges"]
async fn live() {
    if std::env::var("OTTO_LIVE").as_deref() != Ok("1") {
        return;
    }
    let model = openai::OpenAiModel::new(openai::OpenAiConfig {
        base_url: "https://api.openai.com/v1".into(),
        auth: CredentialSource::api_key(std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY")),
        mode: openai::AuthMode::ApiKey,
        model: openai::model_info("gpt-5.2").unwrap(),
        http: HttpClient::new().unwrap(),
        retry: RetryPolicy::default(),
        idle_timeout: Duration::from_secs(60),
        organization: None,
        project: None,
    });
    let req = Request { items: vec![Item::user_text("u", "Say hello.")], max_output: Some(128), ..Request::default() };
    let result = testing::collect_events(model.stream(req, CallCx::default()).await.unwrap()).await;
    assert!(result.iter().any(|e| matches!(e, StreamEvent::PartDone { part: Part::Text { .. } })));
}
