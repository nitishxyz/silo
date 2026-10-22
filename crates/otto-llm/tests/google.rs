#![cfg(feature = "client")]
//! Gemini Interactions API: step streaming, thought signatures, function
//! calls, server tool steps, stateless full-history replay.
use futures::StreamExt;
use otto_llm::{
    google::{convert, stream},
    *,
};
use serde_json::json;

const TOOLS: &str = include_str!("fixtures/google/tools.sse");
const SEARCH: &str = include_str!("fixtures/google/search.sse");
const ERROR: &str = include_str!("fixtures/google/error.sse");

async fn events(raw: &str, seed: u64) -> Vec<StreamEvent> {
    testing::collect_events(stream::stream(testing::chunked(raw.as_bytes(), seed), "gemini-3.8-flash")).await
}
fn parts(events: &[StreamEvent]) -> Vec<Part> {
    events.iter().filter_map(|e| if let StreamEvent::PartDone { part } = e { Some(part.clone()) } else { None }).collect()
}

#[tokio::test]
async fn thoughts_and_parallel_function_calls_replay_byte_exact() {
    for seed in 0..20 {
        let events = events(TOOLS, seed).await;
        assert!(matches!(&events[0], StreamEvent::Start { response_id: Some(id), .. } if id == "v1_tools"));
        assert!(events.contains(&StreamEvent::ToolInputStart { call_id: "un6k8t18".into(), name: "read_file".into() }));
        assert!(events.contains(&StreamEvent::ReasoningDelta { text: "I should read the file.".into(), summary: true }));
        let parts = parts(&events);
        let Part::Reasoning { summary: Some(summary), opaque: Some(thought), .. } = &parts[0] else { panic!("thought: {parts:?}") };
        assert_eq!(summary, "I should read the file.");
        assert_eq!(thought.data["signature"], "CiQB-thought-sig");
        let calls: Vec<_> = parts
            .iter()
            .filter_map(|p| if let Part::ToolCall { call_id, input, .. } = p { Some((call_id.as_str(), input.clone())) } else { None })
            .collect();
        assert_eq!(calls, [("un6k8t18", json!({"path":"README.md"})), ("k2p9", json!({"path":"."}))]);
        let usage = events.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
        assert_eq!(usage, Usage { input: 44, output: 120, cache_read: 256, cache_write: 0, reasoning: 80 });
        assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::ToolUse, .. })));
        let req = Request {
            model: "gemini-3.8-flash".into(),
            system: vec!["Be terse.".into()],
            items: vec![
                Item::user_text("u", "Read the README"),
                Item::new("a", Role::Assistant, parts.clone()),
                Item::new(
                    "t",
                    Role::Tool,
                    vec![
                        Part::ToolResult { call_id: "un6k8t18".into(), content: vec![ResultPart::text("# otto")], is_error: false },
                        Part::ToolResult { call_id: "k2p9".into(), content: vec![ResultPart::text("no access")], is_error: true },
                    ],
                ),
            ],
            ..Request::default()
        };
        let body = convert::request(&req).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0], json!({"type":"user_input","content":[{"type":"text","text":"Read the README"}]}));
        assert_eq!(input[1], thought.data);
        assert_eq!(input[2]["arguments"], json!({"path":"README.md"}));
        assert_eq!(input[4], json!({"type":"function_result","call_id":"un6k8t18","name":"read_file","result":[{"type":"text","text":"# otto"}]}));
        assert_eq!(input[5]["is_error"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["system_instruction"], "Be terse.");
    }
}

#[tokio::test]
async fn server_tool_steps_and_text() {
    let events = events(SEARCH, 5).await;
    assert!(events.contains(&StreamEvent::HostedProgress { call_id: "gs_1".into(), tool: "google_search".into(), status: "in_progress".into() }));
    let parts = parts(&events);
    assert!(
        matches!(&parts[0], Part::ToolCall { kind: CallKind::Hosted, input, opaque: Some(o), .. } if input["queries"][0] == "largest mountain in Europe" && o.data["signature"] == "gs-sig")
    );
    assert!(matches!(&parts[1], Part::HostedResult { tool, payload, .. } if tool == "google_search" && payload[0]["title"] == "Mount Elbrus"));
    assert_eq!(parts[2], Part::text("Mount Elbrus, at 5,642 m."));
    assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::Stop, .. })));
    let body = convert::request(&Request { items: vec![Item::new("a", Role::Assistant, parts.clone())], ..Request::default() }).unwrap();
    assert_eq!(body["input"][0]["type"], "google_search_call");
    assert_eq!(body["input"][1]["type"], "google_search_result");
    assert_eq!(body["input"][2], json!({"type":"model_output","content":[{"type":"text","text":"Mount Elbrus, at 5,642 m."}]}));
}

#[tokio::test]
async fn error_events_are_terminal() {
    let results: Vec<_> = stream::stream(testing::chunked(ERROR.as_bytes(), 1), "gemini-3.8-flash").collect().await;
    let error = results.last().unwrap().as_ref().unwrap_err();
    assert_eq!(error.class, ErrorClass::Timeout);
    let truncated: Vec<_> = stream::stream(testing::chunked(&TOOLS.as_bytes()[..400], 1), "gemini-3.8-flash").collect().await;
    assert_eq!(truncated.last().unwrap().as_ref().unwrap_err().class, ErrorClass::Network);
}

#[test]
fn request_shaping() {
    let tool =
        |name: &str, kind| ToolDef { name: name.into(), description: "d".into(), input_schema: json!({"type":"object"}), kind, defer_loading: false };
    let mut req = Request {
        model: "gemini-4-argon".into(),
        items: vec![Item::user_text("u", "hi"), Item::new("s", Role::System, vec![Part::System { text: "now plan".into() }])],
        tools: vec![
            tool("read", ToolKind::Function),
            tool("google_search", ToolKind::Hosted { tool: "google_search".into(), config: serde_json::Value::Null }),
        ],
        tool_choice: ToolChoice::Tool("read".into()),
        reasoning: Some(Reasoning::Effort(Effort::Xhigh)),
        max_output: Some(2048),
        output_schema: Some(json!({"type":"object"})),
        ..Request::default()
    };
    let body = convert::request(&req).unwrap();
    assert_eq!(body["tools"][0], json!({"type":"function","name":"read","description":"d","parameters":{"type":"object"}}));
    assert_eq!(body["tools"][1], json!({"type":"google_search"}));
    assert_eq!(body["tool_choice"], json!({"allowed_tools":{"mode":"any","tools":["read"]}}));
    assert_eq!(body["generation_config"], json!({"max_output_tokens":2048,"thinking_level":"high","thinking_summaries":"auto"}));
    assert_eq!(body["response_format"]["mime_type"], "application/json");
    assert!(body["input"][1]["content"][0]["text"].as_str().unwrap().contains("now plan"));
    // Foreign reasoning never reaches Gemini.
    req.items.push(Item::new(
        "a",
        Role::Assistant,
        vec![Part::Reasoning {
            summary: None,
            text: Some("x".into()),
            opaque: Some(Opaque { provider: "anthropic".into(), model: "m".into(), data: json!({}) }),
        }],
    ));
    assert_eq!(convert::request(&req).unwrap()["input"].as_array().unwrap().len(), 2);
    req.tools.push(tool("patch", ToolKind::Custom { format: None }));
    assert_eq!(convert::request(&req).unwrap_err().class, ErrorClass::InvalidRequest);
}
