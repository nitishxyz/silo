#![cfg(feature = "client")]
//! The OpenAI-compatible family: one Chat Completions parser, per-dialect
//! request shaping, local model discovery.
use futures::StreamExt;
use otto_llm::{
    compat::{self, convert, stream},
    *,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEEPSEEK: &str = include_str!("fixtures/compat/deepseek_tools.sse");
const OPENROUTER: &str = include_str!("fixtures/compat/openrouter_reasoning.sse");
const MOONSHOT: &str = include_str!("fixtures/compat/moonshot_usage.sse");
const OLLAMA: &str = include_str!("fixtures/compat/ollama_no_done.sse");
const ERROR: &str = include_str!("fixtures/compat/midstream_error.sse");

async fn events(raw: &str, seed: u64, provider: &str) -> Vec<StreamEvent> {
    testing::collect_events(stream::stream(testing::chunked(raw.as_bytes(), seed), "m", provider)).await
}
fn parts(events: &[StreamEvent]) -> Vec<Part> {
    events.iter().filter_map(|e| if let StreamEvent::PartDone { part } = e { Some(part.clone()) } else { None }).collect()
}
fn usage(events: &[StreamEvent]) -> Usage {
    events.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap()
}
fn dialect(id: &str) -> &'static compat::Dialect {
    compat::dialect(id).unwrap()
}
fn tool(name: &str) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: "d".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        kind: ToolKind::Function,
        defer_loading: false,
    }
}

#[tokio::test]
async fn deepseek_parallel_streamed_calls_reasoning_replay_and_cache_usage() {
    for seed in 0..20 {
        let events = events(DEEPSEEK, seed, "deepseek").await;
        assert!(events.contains(&StreamEvent::ToolInputStart { call_id: "call_l".into(), name: "list_dir".into() }));
        let parts = parts(&events);
        let Part::Reasoning { text: Some(text), opaque: Some(opaque), .. } = &parts[0] else { panic!("reasoning: {parts:?}") };
        assert_eq!(text, "Need the file and the listing.");
        assert_eq!(opaque.provider.as_str(), "deepseek");
        let calls: Vec<_> = parts
            .iter()
            .filter_map(|p| if let Part::ToolCall { call_id, input, .. } = p { Some((call_id.as_str(), input.clone())) } else { None })
            .collect();
        assert_eq!(calls, [("call_r", json!({"path":"README.md"})), ("call_l", json!({"path":"."}))]);
        assert_eq!(usage(&events), Usage { input: 104, output: 30, cache_read: 896, cache_write: 0, reasoning: 12 });
        assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::ToolUse, .. })));
        let req = Request {
            model: "deepseek-v4-pro".into(),
            items: vec![
                Item::user_text("u", "look"),
                Item::new("a", Role::Assistant, parts.clone()),
                Item::new(
                    "t",
                    Role::Tool,
                    vec![Part::ToolResult { call_id: "call_r".into(), content: vec![ResultPart::text("# otto")], is_error: false }],
                ),
            ],
            tools: vec![tool("read_file")],
            reasoning: Some(Reasoning::Effort(Effort::High)),
            ..Request::default()
        };
        let body = convert::request(&req, dialect("deepseek")).unwrap();
        let assistant = &body["messages"][1];
        assert_eq!(assistant["reasoning_content"], "Need the file and the listing.");
        assert_eq!(
            assistant["tool_calls"][0],
            json!({"id":"call_r","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}})
        );
        assert_eq!(body["messages"][2], json!({"role":"tool","tool_call_id":"call_r","content":"# otto"}));
        assert_eq!(body["thinking"], json!({"type":"enabled"}));
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["stream_options"], json!({"include_usage":true}));
        // Another provider never gets DeepSeek's reasoning.
        assert!(convert::request(&req, dialect("moonshotai")).unwrap()["messages"][1].get("reasoning_content").is_none());
    }
}

#[tokio::test]
async fn openrouter_reasoning_details_comments_and_repeated_finish() {
    let events = events(OPENROUTER, 4, "openrouter").await;
    assert_eq!(events.iter().filter(|e| matches!(e, StreamEvent::Finish { .. })).count(), 1);
    let parts = parts(&events);
    let Part::Reasoning { opaque: Some(opaque), .. } = &parts[0] else { panic!("reasoning") };
    assert_eq!(
        opaque.data["reasoning_details"],
        json!([{"type":"reasoning.text","text":"Check the docs.","signature":"sig-abc","format":"anthropic-claude-v1","index":0}])
    );
    assert_eq!(parts[1], Part::text("The answer is 42."));
    let u = usage(&events);
    assert_eq!((u.input, u.cache_read, u.cache_write), (500, 4000, 500));
    assert!((u.cache_hit_rate().unwrap() - 0.8).abs() < 1e-9);
    let req = Request {
        model: "anthropic/claude-sonnet-5.5".into(),
        items: vec![Item::user_text("u", "q"), Item::new("a", Role::Assistant, parts.clone())],
        reasoning: Some(Reasoning::Effort(Effort::Medium)),
        ..Request::default()
    };
    let body = convert::request(&req, dialect("openrouter")).unwrap();
    assert_eq!(body["messages"][1]["reasoning_details"], opaque.data["reasoning_details"]);
    assert_eq!(body["reasoning"], json!({"effort":"medium"}));
}

#[tokio::test]
async fn moonshot_choice_usage_and_ollama_without_done() {
    let moonshot = events(MOONSHOT, 2, "moonshotai").await;
    assert_eq!(usage(&moonshot), Usage { input: 50, output: 5, cache_read: 150, cache_write: 0, reasoning: 0 });
    assert!(matches!(moonshot.last(), Some(StreamEvent::Finish { reason: FinishReason::Stop, .. })));
    let ollama = events(OLLAMA, 9, "ollama").await;
    let parts = parts(&ollama);
    assert!(matches!(&parts[0], Part::Reasoning { text: Some(t), .. } if t == "thinking"));
    assert!(matches!(&parts[1], Part::ToolCall { name, .. } if name == "read_file"));
    assert_eq!(usage(&ollama).input, 80);
    assert_eq!(usage(&ollama).cache_hit_rate(), Some(0.0));
}

#[tokio::test]
async fn errors_and_truncation() {
    let results: Vec<_> = stream::stream(testing::chunked(ERROR.as_bytes(), 1), "m", "openrouter").collect().await;
    let error = results.last().unwrap().as_ref().unwrap_err();
    assert_eq!(error.class, ErrorClass::Server);
    assert_eq!(error.message, "Provider disconnected unexpectedly");
    let cut = &DEEPSEEK[..DEEPSEEK.find("\"finish_reason\":\"tool_calls\"").unwrap() - 40];
    let results: Vec<_> = stream::stream(testing::chunked(cut.as_bytes(), 1), "m", "deepseek").collect().await;
    assert_eq!(results.last().unwrap().as_ref().unwrap_err().class, ErrorClass::Network);
}

#[test]
fn dialect_request_shapes() {
    let req = |reasoning| Request {
        model: "m".into(),
        system: vec!["sys".into()],
        items: vec![
            Item::new(
                "u",
                Role::User,
                vec![
                    Part::text("see"),
                    Part::Image(Media { mime: "image/png".into(), source: MediaSource::Base64 { data: "AAA".into() }, name: None }),
                ],
            ),
            Item::new("s", Role::System, vec![Part::System { text: "mode: plan".into() }]),
        ],
        tools: vec![tool("read")],
        tool_choice: ToolChoice::Tool("read".into()),
        parallel_tools: Some(true),
        max_output: Some(100),
        reasoning,
        ..Request::default()
    };
    let zai = convert::request(&req(Some(Reasoning::Effort(Effort::High))), dialect("zai")).unwrap();
    assert_eq!(zai["thinking"], json!({"type":"enabled","clear_thinking":false}));
    assert_eq!(zai["tool_stream"], true);
    assert!(zai.get("stream_options").is_none());
    assert_eq!(zai["messages"][0], json!({"role":"system","content":"sys"}));
    assert_eq!(zai["messages"][1]["content"][1]["image_url"]["url"], "data:image/png;base64,AAA");
    assert_eq!(zai["messages"][2], json!({"role":"system","content":"mode: plan"}));
    assert_eq!(zai["tool_choice"], json!({"type":"function","function":{"name":"read"}}));
    assert_eq!(zai["parallel_tool_calls"], true);
    assert_eq!(zai["max_tokens"], 100);
    let off = convert::request(&req(Some(Reasoning::Effort(Effort::None))), dialect("zai")).unwrap();
    assert_eq!(off["thinking"], json!({"type":"disabled"}));
    let ollama = convert::request(&req(Some(Reasoning::Effort(Effort::Low))), dialect("ollama")).unwrap();
    assert_eq!(ollama["reasoning_effort"], "low");
    assert!(convert::request(&req(None), dialect("lmstudio")).unwrap().get("reasoning_effort").is_none());
    let mut hosted = req(None);
    hosted.tools.push(ToolDef { kind: ToolKind::Hosted { tool: "web_search".into(), config: Value::Null }, ..tool("web") });
    assert_eq!(convert::request(&hosted, dialect("openrouter")).unwrap_err().class, ErrorClass::InvalidRequest);
}

#[test]
fn dialect_table_and_base_urls() {
    for id in ["openrouter", "deepseek", "moonshotai", "zai", "minimax", "ollama", "lmstudio", "custom"] {
        assert!(compat::dialect(id).is_some(), "{id}");
    }
    assert_eq!(dialect("minimax").wire, compat::Wire::Anthropic);
    assert!(!dialect("ollama").key_required && !dialect("custom").key_required && dialect("deepseek").key_required);
    assert_eq!(dialect("ollama").base(Some("127.0.0.1:11500")).unwrap(), "http://127.0.0.1:11500/v1");
    assert_eq!(dialect("lmstudio").base(Some("http://box:1234/v1/")).unwrap(), "http://box:1234/v1");
    assert_eq!(dialect("deepseek").base(None).unwrap(), "https://api.deepseek.com");
}

/// Serve one canned HTTP response per connection.
async fn serve(body: &'static str) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let mut buffer = [0; 4096];
            let _ = tcp.read(&mut buffer).await;
            let response =
                format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            let _ = tcp.write_all(response.as_bytes()).await;
        }
    });
    address
}

#[tokio::test]
async fn local_discovery_lists_models_and_tolerates_absent_servers() {
    let http = reqwest::Client::new();
    let ollama = serve(r#"{"models":[{"name":"qwen3:8b","model":"qwen3:8b"},{"name":"llama4:latest","model":"llama4:latest"}]}"#).await;
    let ids = compat::discover(dialect("ollama"), &format!("http://{ollama}/v1"), &http, None).await;
    assert_eq!(ids, ["llama4:latest", "qwen3:8b"]);
    let lmstudio = serve(r#"{"object":"list","data":[{"id":"qwen/qwen3-coder-30b","object":"model"}]}"#).await;
    assert_eq!(compat::discover(dialect("lmstudio"), &format!("http://{lmstudio}/v1"), &http, None).await, ["qwen/qwen3-coder-30b"]);
    // Nothing listens on port 9: an empty list, quickly, without an error.
    let started = std::time::Instant::now();
    assert!(compat::discover(dialect("lmstudio"), "http://127.0.0.1:9/v1", &http, None).await.is_empty());
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    assert!(compat::discover(dialect("deepseek"), "https://api.deepseek.com", &http, None).await.is_empty(), "no discovery for hosted APIs");
}
