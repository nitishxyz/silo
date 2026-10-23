#![cfg(feature = "client")]
//! O3 OpenAI depth: `previous_response_id` continuation on WebSocket,
//! compaction, async tools, our own tools, Codex WebSocket, xAI.
use futures::StreamExt;
use otto_llm::{
    auth::{Credential, CredentialSource},
    http::{HttpClient, RetryPolicy},
    openai::{self, convert, stream},
    *,
};
use serde_json::{Value, json};
use std::time::Duration;

const COMPACTION: &str = include_str!("fixtures/openai/compaction.sse");
const TOOLS: &str = include_str!("fixtures/openai/tools.sse");
const SHELL: &str = include_str!("fixtures/openai/shell.sse");
const XAI: &str = include_str!("fixtures/xai/reasoning.sse");

fn parts(events: &[StreamEvent]) -> Vec<Part> {
    events.iter().filter_map(|e| if let StreamEvent::PartDone { part } = e { Some(part.clone()) } else { None }).collect()
}

#[tokio::test]
async fn server_compaction_items_replay_and_threshold_option() {
    for seed in 0..10 {
        let events = testing::collect_events(stream::stream(testing::chunked(COMPACTION.as_bytes(), seed), "gpt-6-astra")).await;
        let parts = parts(&events);
        let Part::Compaction { summary: None, opaque: Some(opaque) } = &parts[0] else { panic!("compaction part: {parts:?}") };
        assert_eq!(opaque.data["encrypted_content"], "gAAAA-compacted");
        let usage = events.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
        assert_eq!((usage.input, usage.cache_read), (400, 800));
        assert!((usage.cache_hit_rate().unwrap() - 800.0 / 1200.0).abs() < 1e-9);
        let req = Request {
            model: "gpt-6-astra".into(),
            items: vec![Item::user_text("u", "long task"), Item::new("a", Role::Assistant, parts)],
            compaction: Some(Compaction::Auto { threshold: 200_000, instructions: None }),
            ..Request::default()
        };
        let body = convert::request(&req);
        assert_eq!(body["input"][1], json!({"type":"compaction","id":"cmp_1","encrypted_content":"gAAAA-compacted"}));
        assert_eq!(body["context_management"], json!([{"type":"compaction","compact_threshold":200000}]));
    }
}

#[test]
fn compact_endpoint_window_replaces_the_summarized_items() {
    let response = json!({
        "id": "resp_c", "model": "gpt-6-astra", "object": "response.compaction",
        "output": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Plan a trip"}]},
            {"type":"compaction","id":"cmp_9","encrypted_content":"enc"}
        ],
        "usage": {"input_tokens": 900, "output_tokens": 40}
    });
    let events = convert::compacted(&response, "openai");
    assert!(matches!(&events[0], StreamEvent::Start { response_id: Some(id), .. } if id == "resp_c"));
    assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::Other(r), .. }) if r == "compaction"));
    let window = parts(&events);
    let req = Request { items: vec![Item::new("c", Role::Assistant, window), Item::user_text("u2", "Add a day")], ..Request::default() };
    let body = convert::request(&req);
    assert_eq!(body["input"][0], response["output"][0]);
    assert_eq!(body["input"][1], response["output"][1]);
    assert_eq!(body["input"][2]["content"][0]["text"], "Add a day");
    // Another provider never receives the window.
    assert_eq!(convert::request_for(&req, "xai")["input"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn shell_call_commands_run_as_entries_and_answer_once() {
    let events = testing::collect_events(stream::stream(testing::chunked(SHELL.as_bytes(), 5), "gpt-5.1")).await;
    let parts = parts(&events);
    let calls: Vec<_> = parts
        .iter()
        .filter_map(|p| {
            if let Part::ToolCall { call_id, name, input, kind, opaque } = p {
                Some((call_id.as_str(), name.as_str(), input.clone(), *kind, opaque.is_some()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        calls,
        [
            ("call_sh", "shell", json!({"command":"ls src","timeout_ms":5000}), CallKind::Function, true),
            ("call_sh_1", "shell", json!({"command":"cat README.md","timeout_ms":5000}), CallKind::Function, false),
            ("call_local", "shell", json!({"command":"git log -1 '--format=%s it'\\''s'","cwd":"/tmp/x"}), CallKind::Function, true),
        ]
    );
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Finish { reason: FinishReason::ToolUse, .. })));
    let result = |id: &str, text: &str, is_error: bool| Part::ToolResult {
        call_id: id.into(),
        content: vec![ResultPart::Text { text: text.into() }],
        is_error,
    };
    let req = Request {
        model: "gpt-5.1".into(),
        items: vec![
            Item::user_text("u", "look around"),
            Item::new("a", Role::Assistant, parts),
            Item::new(
                "t",
                Role::Tool,
                vec![result("call_sh", "lib.rs", false), result("call_sh_1", "no such file", true), result("call_local", "fix", false)],
            ),
        ],
        ..Request::default()
    };
    // One raw item and one output per call id; each command's output in order.
    let input = convert::request(&req)["input"].as_array().unwrap().clone();
    assert_eq!(input.len(), 5);
    assert_eq!((input[1]["type"].as_str(), input[1]["call_id"].as_str()), (Some("shell_call"), Some("call_sh")));
    assert_eq!(input[2]["type"], "local_shell_call");
    let exit = |code: i32| json!({"type":"exit","exit_code":code});
    assert_eq!(
        input[3],
        json!({"type":"shell_call_output","call_id":"call_sh","output":[
            {"stdout":"lib.rs","stderr":"","outcome":exit(0)},{"stdout":"no such file","stderr":"","outcome":exit(1)}]})
    );
    assert_eq!(input[4], json!({"type":"function_call_output","call_id":"call_local","output":"fix"}));
    // Another provider gets plain calls of our `shell`, one per command.
    let other = convert::request_for(&req, "xai")["input"].as_array().unwrap().clone();
    let types: Vec<_> = other.iter().map(|i| (i["type"].as_str().unwrap(), i["call_id"].as_str().unwrap_or_default())).collect();
    assert_eq!(
        types[1..],
        [
            ("function_call", "call_sh"),
            ("function_call", "call_sh_1"),
            ("function_call", "call_local"),
            ("function_call_output", "call_sh"),
            ("function_call_output", "call_sh_1"),
            ("function_call_output", "call_local")
        ]
    );
}

#[tokio::test]
async fn our_tools_and_async_calls() {
    let events = testing::collect_events(stream::stream(testing::chunked(TOOLS.as_bytes(), 3), "gpt-6-astra")).await;
    assert!(events.contains(&StreamEvent::ToolInputStart { call_id: "call_patch".into(), name: "apply_patch".into() }));
    let parts = parts(&events);
    let calls: Vec<_> = parts
        .iter()
        .filter_map(|p| if let Part::ToolCall { name, input, kind, .. } = p { Some((name.as_str(), input, *kind)) } else { None })
        .collect();
    assert_eq!(calls[0].0, "apply_patch");
    assert_eq!(calls[0].2, CallKind::Custom);
    assert!(calls[0].1.as_str().unwrap().starts_with("*** Begin Patch\n*** Update File: lib/fib.py"));
    assert_eq!(calls[1], ("shell", &json!({"command":"cargo test","background":true}), CallKind::Function));
    assert_eq!(calls[2], ("lookup", &json!({"sku":"W"}), CallKind::Function));
    assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::ToolUse, .. })));
    let result = |id: &str, text: &str, is_error| Part::ToolResult { call_id: id.into(), content: vec![ResultPart::text(text)], is_error };
    let shell = json!({"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"timeout_ms":{"type":"integer"},"background":{"type":"boolean"}},"required":["command"]});
    let req = Request {
        model: "gpt-6-astra".into(),
        items: vec![
            Item::user_text("u", "rename fib"),
            Item::new("a", Role::Assistant, parts),
            Item::new(
                "t",
                Role::Tool,
                vec![result("call_patch", "patched", false), result("call_shell", "job j1", false), result("call_async", "{\"price\":12}", false)],
            ),
        ],
        tools: vec![
            ToolDef {
                name: "apply_patch".into(),
                description: "Apply a Codex patch".into(),
                input_schema: Value::Null,
                kind: ToolKind::Custom { format: Some(json!({"type":"text"})) },
                defer_loading: false,
            },
            ToolDef {
                name: "shell".into(),
                description: "Run a command".into(),
                input_schema: shell.clone(),
                kind: ToolKind::Function,
                defer_loading: false,
            },
            ToolDef {
                name: "lookup".into(),
                description: "Price".into(),
                input_schema: json!({"type":"object"}),
                kind: ToolKind::Function,
                defer_loading: false,
            },
        ],
        provider: ProviderOptions {
            openai: Some(openai::Options { async_tools: vec!["lookup".into()], ..Default::default() }),
            ..Default::default()
        },
        ..Request::default()
    };
    let body = convert::request(&req);
    // Our own tools: the freeform patch tool and our full shell schema.
    assert_eq!(body["tools"][0], json!({"type":"custom","name":"apply_patch","description":"Apply a Codex patch","format":{"type":"text"}}));
    assert_eq!(body["tools"][1]["type"], "function");
    assert_eq!(body["tools"][1]["name"], "shell");
    assert_eq!(body["tools"][1]["parameters"]["properties"]["background"], json!({"type":"boolean"}));
    assert_eq!(body["tools"][2]["async"], true);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[1]["type"], "custom_tool_call");
    assert_eq!(input[2]["type"], "function_call");
    // Raw async call items replay byte-exact.
    assert_eq!(input[3]["async"], true);
    assert_eq!(input[4]["type"], "custom_tool_call_output");
    assert_eq!(input[5]["type"], "function_call_output");
    assert_eq!(input[6]["type"], "function_call_output");
}

#[tokio::test]
async fn xai_shares_the_wire_but_not_opaque_items() {
    let events = testing::collect_events(stream::stream_for(testing::chunked(XAI.as_bytes(), 1), "grok-4.6", "xai")).await;
    let parts = parts(&events);
    let Part::Reasoning { opaque: Some(opaque), .. } = &parts[0] else { panic!("reasoning") };
    assert_eq!(opaque.provider.as_str(), "xai");
    let usage = events.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
    assert_eq!((usage.input, usage.cache_read, usage.reasoning), (8, 32, 5));
    let req = Request {
        model: "grok-4.6".into(),
        items: vec![Item::user_text("u", "hi"), Item::new("a", Role::Assistant, parts)],
        reasoning: Some(Reasoning::Effort(Effort::None)),
        ..Request::default()
    };
    let body = convert::request_for(&req, "xai");
    assert_eq!(body["input"][1]["encrypted_content"], "xai-enc");
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(convert::request(&req)["input"].as_array().unwrap().len(), 2, "OpenAI gets the text but not xAI reasoning");
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
fn created(id: &str) -> Value {
    json!({"type":"response.created","response":{"id":id,"model":"gpt-6-astra","status":"in_progress"}})
}
fn done(item: Value) -> Value {
    json!({"type":"response.output_item.done","item":item})
}
fn completed(id: &str) -> Value {
    json!({"type":"response.completed","response":{"id":id,"status":"completed","usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":90},"output_tokens":5}}})
}

#[tokio::test]
async fn ws_continues_from_the_cached_response_and_resends_when_it_is_gone() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let (_, mut ws) = tokio_websockets::ServerBuilder::new().accept(tcp).await.unwrap();
        // 1: a fresh connection sends everything.
        let first = ws_recv(&mut ws).await;
        assert!(first.get("previous_response_id").is_none());
        assert_eq!(first["input"].as_array().unwrap().len(), 1);
        let call = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a\"}"});
        ws_send(&mut ws, &[created("resp_1"), done(call), completed("resp_1")]).await;
        // 2: only the tool result, continuing resp_1. The server lost it: resend in full.
        let second = ws_recv(&mut ws).await;
        assert_eq!(second["previous_response_id"], "resp_1");
        assert_eq!(second["input"], json!([{"type":"function_call_output","call_id":"call_1","output":[{"type":"input_text","text":"A"}]}]));
        ws_send(&mut ws, &[json!({"type":"error","status":400,"error":{"type":"invalid_request_error","code":"previous_response_not_found","message":"Previous response with id 'resp_1' not found.","param":"previous_response_id"}})]).await;
        let full = ws_recv(&mut ws).await;
        assert!(full.get("previous_response_id").is_none());
        assert_eq!(full["input"].as_array().unwrap().len(), 3);
        let text = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done","annotations":[]}]});
        ws_send(&mut ws, &[created("resp_2"), done(text), completed("resp_2")]).await;
        // 3: the next user message continues resp_2.
        let third = ws_recv(&mut ws).await;
        assert_eq!(third["previous_response_id"], "resp_2");
        assert_eq!(third["input"], json!([{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]));
        let text = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Ok","annotations":[]}]});
        ws_send(&mut ws, &[created("resp_3"), done(text), completed("resp_3")]).await;
        // 4: changed instructions cannot continue.
        let fourth = ws_recv(&mut ws).await;
        assert!(fourth.get("previous_response_id").is_none());
        ws_send(&mut ws, &[created("resp_4"), completed("resp_4")]).await;
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
    let mut req = Request { items: vec![Item::user_text("u", "read a")], ..Request::default() };
    let run = |req: Request| async { testing::collect_events(model.stream(req, CallCx::default()).await.unwrap()).await };
    let first = run(req.clone()).await;
    req.items.push(Item::new("a1", Role::Assistant, parts(&first)));
    req.items.push(Item::new(
        "t1",
        Role::Tool,
        vec![Part::ToolResult { call_id: "call_1".into(), content: vec![ResultPart::text("A")], is_error: false }],
    ));
    let second = run(req.clone()).await;
    assert!(second.contains(&StreamEvent::PartDone { part: Part::text("Done") }));
    req.items.push(Item::new("a2", Role::Assistant, parts(&second)));
    req.items.push(Item::user_text("u2", "next"));
    let third = run(req.clone()).await;
    assert!(third.contains(&StreamEvent::PartDone { part: Part::text("Ok") }));
    req.items.push(Item::new("a3", Role::Assistant, parts(&third)));
    req.system = vec!["new instructions".into()];
    req.items.push(Item::user_text("u3", "again"));
    run(req).await;
    drop(model);
    server.await.unwrap();
}

#[tokio::test]
async fn codex_subscription_streams_over_websocket_with_the_beta_header() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let (request, mut ws) = tokio_websockets::ServerBuilder::new().accept(tcp).await.unwrap();
        assert_eq!(request.uri().path(), "/backend-api/codex/responses");
        assert_eq!(request.headers()["openai-beta"], "responses_websockets=2026-02-06");
        assert_eq!(request.headers()["authorization"], "Bearer access");
        assert_eq!(request.headers()["chatgpt-account-id"], "acct");
        assert_eq!(request.headers()["session_id"], "sess");
        let create = ws_recv(&mut ws).await;
        assert_eq!(create["type"], "response.create");
        assert_eq!(create["store"], false);
        assert!(create.get("stream").is_none());
        let text = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hi","annotations":[]}]});
        ws_send(&mut ws, &[created("resp_s"), done(text), completed("resp_s")]).await;
        let _ = ws.next().await;
    });
    let credential = Credential::OAuth {
        access: "access".into(),
        refresh: "refresh".into(),
        expires_at_ms: otto_llm::auth::now_ms() + 3_600_000,
        account_id: Some("acct".into()),
        extra: Value::Null,
    };
    let model = openai::OpenAiModel::new(openai::OpenAiConfig {
        base_url: format!("http://{address}/backend-api/codex"),
        auth: CredentialSource::Static(credential),
        mode: openai::AuthMode::Subscription,
        model: catalog::Catalog::global().info("openai", "gpt-6.1-sol", Some(openai::AuthMode::Subscription)).unwrap(),
        http: HttpClient::new().unwrap(),
        retry: RetryPolicy::default(),
        idle_timeout: Duration::from_secs(5),
        organization: None,
        project: None,
    });
    assert!(model.session().is_none(), "no steering on the Codex endpoint");
    let req = Request { items: vec![Item::user_text("u", "hi")], ..Request::default() };
    let events = testing::collect_events(model.stream(req, CallCx { session: Some("sess".into()), ..CallCx::default() }).await.unwrap()).await;
    assert!(events.contains(&StreamEvent::PartDone { part: Part::text("Hi") }));
    drop(model);
    server.await.unwrap();
}
