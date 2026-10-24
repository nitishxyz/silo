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
    assert_eq!(convert::effort(Reasoning::Effort(Effort::Max)), "max");
}
#[test]
fn efforts_clamp_to_what_the_model_takes() {
    for (provider, model, effort, expected) in [
        ("openai", "gpt-5.2", Effort::Max, "xhigh"),
        ("openai", "gpt-5.3-codex", Effort::Max, "xhigh"),
        ("openai", "gpt-5.1", Effort::Max, "high"),
        ("openai", "gpt-5.1", Effort::Minimal, "low"),
        ("openai", "gpt-5", Effort::None, "minimal"),
        ("openai", "gpt-5.2-pro", Effort::Low, "medium"),
        ("openai", "gpt-6-sol", Effort::Max, "max"),
        ("openai", "gpt-5.2-2025-12-11", Effort::Max, "xhigh"),
        ("openai", "gpt-unlisted", Effort::Max, "xhigh"),
        ("openai", "gpt-unlisted", Effort::Medium, "medium"),
        ("xai", "grok-4.6", Effort::Max, "xhigh"),
        ("xai", "grok-unlisted", Effort::None, "low"),
    ] {
        let got = convert::effort(convert::accepted(Reasoning::Effort(effort), provider, model));
        assert_eq!(got, expected, "{provider} {model} {effort:?}");
    }
    let budget = Reasoning::Budget(50_000);
    assert_eq!(convert::accepted(budget, "openai", "gpt-5.1"), budget);
    let req = Request { model: "gpt-5.1".into(), reasoning: Some(Reasoning::Effort(Effort::Max)), ..Request::default() };
    assert_eq!(convert::request(&req)["reasoning"]["effort"], "high");
    assert_eq!(convert::request_for(&Request { model: "grok-4.6".into(), ..req }, "xai")["reasoning"]["effort"], "xhigh");
}
#[test]
fn nearest_effort_prefers_the_higher_on_a_tie() {
    use Effort::*;
    assert_eq!(Max.nearest(&[Low, Medium, High]), High);
    assert_eq!(Max.nearest(&[None, Low, Medium, High, Xhigh]), Xhigh);
    assert_eq!(Medium.nearest(&[Low, High, Max]), High);
    assert_eq!(None.nearest(&[Low, Medium, High]), Low);
    assert_eq!(Xhigh.nearest(&[Low, Medium, High, Max]), Max);
    assert_eq!(Max.nearest(&[]), Max);
    assert_eq!(catalog::efforts("openai", "gpt-5.1"), [None, Low, Medium, High]);
    assert_eq!(catalog::efforts("openai", "gpt-5.1-2025-11-13"), [None, Low, Medium, High], "a dated snapshot takes its model's");
    assert!(catalog::efforts("openai", "gpt-unlisted").is_empty());
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

fn steering_events(steering: &mut openai::ws::Steering, events: &[Value]) -> (Vec<StreamEvent>, Vec<String>) {
    let mut out = (vec![], vec![]);
    for event in events {
        let (events, frames) = steering.event(event.clone()).unwrap();
        out.0.extend(events);
        out.1.extend(frames);
    }
    out
}
fn created(id: &str) -> Value {
    json!({"type":"response.created","response":{"id":id,"model":"gpt-6-astra","status":"in_progress"}})
}
fn message(text: &str) -> Value {
    json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}})
}
fn ended(kind: &str, id: &str, extra: Value) -> Value {
    let mut response = json!({"id":id,"status":"completed","usage":{"input_tokens":10,"output_tokens":3}});
    convert::merge(&mut response, &extra);
    json!({"type":kind,"response":response})
}

#[test]
fn ws_frames_encode_create_and_steer() {
    let req = Request { model: "gpt-6-astra".into(), items: vec![Item::user_text("u", "plan")], ..Request::default() };
    let create: Value = serde_json::from_str(&openai::ws::create_frame(convert::request(&req))).unwrap();
    assert_eq!(create["type"], "response.create");
    assert!(create.get("stream").is_none());
    assert_eq!(create["store"], false);
    assert_eq!(create["input"][0]["content"][0]["text"], "plan");
    let image = Part::Image(Media { mime: "image/png".into(), source: MediaSource::Base64 { data: "AAAA".into() }, name: None });
    let input = convert::steer_input(&[Part::text("smaller scope"), image]);
    let steer: Value = serde_json::from_str(&openai::ws::steer_frame("resp_1", &input)).unwrap();
    assert_eq!(
        steer,
        json!({"type":"response.steer","previous_response_id":"resp_1","input":[{"type":"message","role":"user","content":[
            {"type":"input_text","text":"smaller scope"},
            {"type":"input_image","image_url":"data:image/png;base64,AAAA","detail":"auto"}
        ]}]})
    );
}

#[test]
fn ws_steering_commits_at_successor() {
    let mut s = openai::ws::Steering::new("gpt-6-astra");
    // Input before the response exists waits for its id.
    assert!(s.steer("early".into(), json!("first")).is_none());
    let (events, frames) = steering_events(&mut s, &[created("resp_1")]);
    assert!(matches!(&events[0], StreamEvent::Start { response_id: Some(id), .. } if id == "resp_1"));
    assert_eq!(frames.len(), 1);
    assert!(frames[0].contains("\"previous_response_id\":\"resp_1\""));
    let frame = s.steer("late".into(), json!("second")).unwrap();
    assert!(frame.contains("second"));
    let (events, frames) = steering_events(
        &mut s,
        &[
            json!({"type":"response.steer.accepted","steer":{"id":"steer_a","previous_response_id":"resp_1"}}),
            json!({"type":"response.steer.accepted","steer":{"id":"steer_b","previous_response_id":"resp_1"}}),
            json!({"type":"response.output_text.delta","delta":"Dra"}),
            message("Draft"),
            ended("response.incomplete", "resp_1", json!({"status":"incomplete","incomplete_details":{"reason":"steered"}})),
        ],
    );
    assert!(frames.is_empty());
    assert!(!s.done(), "a steered response is followed by its successor");
    assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::Other(r), .. }) if r == "steered"));
    let (events, _) = steering_events(&mut s, &[created("resp_2"), message("Short plan"), ended("response.completed", "resp_2", json!({}))]);
    assert_eq!(events[0], StreamEvent::Steered { ids: vec!["early".into(), "late".into()] });
    assert!(matches!(&events[1], StreamEvent::Start { response_id: Some(id), .. } if id == "resp_2"));
    assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::Stop, .. })));
    assert!(s.done());
    // Nothing is live any more: the caller keeps the input for the next step.
    assert!(s.steer("after".into(), json!("third")).is_none());
}

#[test]
fn ws_steering_pending_and_failed_are_not_committed() {
    let mut s = openai::ws::Steering::new("gpt-6-astra");
    steering_events(&mut s, &[created("resp_1")]);
    s.steer("tool".into(), json!("after the tool")).unwrap();
    s.steer("rejected".into(), json!("nope")).unwrap();
    let call = json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"read","arguments":"{}"}});
    let (events, _) = steering_events(
        &mut s,
        &[
            json!({"type":"response.steer.accepted","steer":{"id":"steer_a","previous_response_id":"resp_1"}}),
            json!({"type":"response.steer.failed","steer":{"input":"nope","previous_response_id":"resp_1"},"error":{"code":"too_many_pending_steers","message":"wait","type":"invalid_request_error"}}),
            call,
            ended("response.completed", "resp_1", json!({})),
        ],
    );
    assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::ToolUse, .. })));
    assert!(!s.done(), "accepted input is still unresolved");
    let (events, _) = steering_events(
        &mut s,
        &[
            json!({"type":"response.steer.pending","steer":{"id":"steer_a","previous_response_id":"resp_1"},"reason":"waiting_for_required_input","required_input":[{"type":"function_call_output","call_id":"c1","name":"read"}]}),
        ],
    );
    assert!(events.is_empty());
    assert!(s.done());
    let error = openai::ws::Steering::new("gpt-6-astra")
        .event(json!({"type":"error","status":429,"error":{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"slow down"}}))
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::RateLimited);
    assert_eq!(error.message, "slow down");
    // Spent quota over the socket: API billing, and the ChatGPT plan's window with its reset.
    let error = openai::ws::Steering::new("gpt-6-astra")
        .event(json!({"type":"error","error":{"type":"insufficient_quota","code":"insufficient_quota","message":"You exceeded your current quota"}}))
        .unwrap_err();
    assert_eq!((error.class, error.status), (ErrorClass::QuotaExhausted, Some(429)));
    let error = openai::ws::Steering::new("gpt-6-astra")
        .event(json!({"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":600}}))
        .unwrap_err();
    assert_eq!((error.class, error.retry_after), (ErrorClass::QuotaExhausted, Some(std::time::Duration::from_secs(600))));
    assert!(!error.retryable());
}

#[test]
fn ws_and_steering_caps_follow_model_and_mode() {
    let catalog = catalog::Catalog::global();
    assert!(catalog.info("openai", "gpt-6-astra", Some(openai::AuthMode::ApiKey)).unwrap().caps.steering);
    assert!(!catalog.info("openai", "gpt-6-astra", Some(openai::AuthMode::Subscription)).unwrap().caps.steering);
    assert!(!catalog.info("openai", "gpt-5.6", Some(openai::AuthMode::ApiKey)).unwrap().caps.steering);
    assert!(catalog.info_or_fallback("openai", "gpt-6-next", Some(openai::AuthMode::ApiKey)).unwrap().caps.steering);
}

async fn ws_send(ws: &mut tokio_websockets::WebSocketStream<tokio::net::TcpStream>, events: &[Value]) {
    use futures::SinkExt;
    for event in events {
        ws.send(tokio_websockets::Message::text(event.to_string())).await.unwrap();
    }
}
async fn ws_recv(ws: &mut tokio_websockets::WebSocketStream<tokio::net::TcpStream>) -> Value {
    loop {
        let message = ws.next().await.unwrap().unwrap();
        if let Some(text) = message.as_text() {
            return serde_json::from_str(text).unwrap();
        }
    }
}

#[tokio::test]
async fn ws_model_steers_mid_response_and_reuses_the_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let (request, mut ws) = tokio_websockets::ServerBuilder::new().accept(tcp).await.unwrap();
        assert_eq!(request.uri().path(), "/v1/responses");
        assert_eq!(request.headers()["authorization"], "Bearer key");
        let create = ws_recv(&mut ws).await;
        assert_eq!(create["type"], "response.create");
        assert_eq!(create["model"], "gpt-6-astra");
        ws_send(&mut ws, &[created("resp_1"), json!({"type":"response.output_text.delta","delta":"Draft"})]).await;
        let steer = ws_recv(&mut ws).await;
        assert_eq!(steer["type"], "response.steer");
        assert_eq!(steer["previous_response_id"], "resp_1");
        assert_eq!(steer["input"][0]["content"][0]["text"], "Keep it short");
        ws_send(
            &mut ws,
            &[
                json!({"type":"response.steer.accepted","steer":{"id":"steer_1","previous_response_id":"resp_1"}}),
                message("Draft"),
                ended("response.incomplete", "resp_1", json!({"status":"incomplete","incomplete_details":{"reason":"steered"}})),
                created("resp_2"),
                message("Short"),
                ended("response.completed", "resp_2", json!({})),
            ],
        )
        .await;
        // The second call arrives on the same connection.
        let again = ws_recv(&mut ws).await;
        assert_eq!(again["type"], "response.create");
        ws_send(&mut ws, &[created("resp_3"), message("Again"), ended("response.completed", "resp_3", json!({}))]).await;
        let _ = ws.next().await;
    });
    let model = openai::OpenAiModel::new(openai::OpenAiConfig {
        base_url: format!("http://{address}/v1"),
        auth: CredentialSource::api_key("key"),
        mode: openai::AuthMode::ApiKey,
        model: catalog::Catalog::global().info("openai", "gpt-6-astra", Some(openai::AuthMode::ApiKey)).unwrap(),
        http: HttpClient::new().unwrap(),
        retry: RetryPolicy::default(),
        idle_timeout: Duration::from_secs(5),
        organization: None,
        project: None,
    });
    let session = model.session().expect("steerable models stream over WebSocket");
    let req = Request { items: vec![Item::user_text("u", "Plan it")], ..Request::default() };
    let mut stream = model.stream(req.clone(), CallCx::default()).await.unwrap();
    let mut events = vec![];
    while let Some(event) = stream.next().await {
        let event = event.unwrap();
        if matches!(event, StreamEvent::TextDelta { .. }) {
            assert!(session.steer("s1", vec![Part::text("Keep it short")]).await.unwrap());
        }
        events.push(event);
    }
    let steered = events.iter().position(|e| matches!(e, StreamEvent::Steered { ids } if ids == &["s1".to_owned()])).unwrap();
    let texts = |events: &[StreamEvent]| {
        events.iter().filter_map(|e| if let StreamEvent::PartDone { part } = e { Some(part.clone()) } else { None }).collect::<Vec<_>>()
    };
    assert_eq!(texts(&events[..steered]), vec![Part::text("Draft")]);
    assert_eq!(texts(&events[steered..]), vec![Part::text("Short")]);
    assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::Stop, response_id: Some(id) }) if id == "resp_2"));
    // Steering outside a live stream is refused, so the caller keeps it.
    assert!(!session.steer("s2", vec![Part::text("later")]).await.unwrap());
    let events = testing::collect_events(model.stream(req, CallCx::default()).await.unwrap()).await;
    assert!(events.contains(&StreamEvent::PartDone { part: Part::text("Again") }));
    drop(model);
    server.await.unwrap();
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
