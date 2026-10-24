#![cfg(feature = "client")]
//! O3 Anthropic depth: compaction (on demand and threshold), context editing
//! betas, server tool blocks, our own client tools, the MiniMax dialect.
use otto_llm::{
    anthropic::{api::Beta, convert, stream},
    *,
};
use serde_json::json;

const NOW: &str = include_str!("fixtures/anthropic/compaction_now.sse");
const THRESHOLD: &str = include_str!("fixtures/anthropic/compaction_threshold.sse");
const SERVER: &str = include_str!("fixtures/anthropic/server_tools.sse");
const THINKING: &str = include_str!("fixtures/anthropic/thinking.sse");

async fn events(raw: &str, seed: u64, provider: &str) -> Vec<StreamEvent> {
    testing::collect_events(stream::decode_for(testing::chunked(raw.as_bytes(), seed), "claude-opus-5-5", provider)).await
}
fn parts(events: &[StreamEvent]) -> Vec<Part> {
    events.iter().filter_map(|e| if let StreamEvent::PartDone { part } = e { Some(part.clone()) } else { None }).collect()
}
fn compaction_text(events: &[StreamEvent]) -> String {
    events.iter().filter_map(|e| if let StreamEvent::CompactionDelta { text } = e { Some(text.as_str()) } else { None }).collect()
}
fn usage(events: &[StreamEvent]) -> Usage {
    events.iter().rev().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap()
}
fn request(items: Vec<Item>) -> Request {
    Request { model: "claude-opus-5-5".into(), items, ..Request::default() }
}

#[tokio::test]
async fn compaction_on_demand_request_stream_and_replay() {
    let mut req = request(vec![Item::user_text("u", "Name the entities"), Item::new("a", Role::Assistant, vec![Part::text("Recipe, Step")])]);
    req.compaction = Some(Compaction::Now { instructions: Some("Keep entity names.".into()) });
    req.stop = vec!["END".into()];
    req.output_schema = Some(json!({"type":"object"}));
    req.tool_choice = ToolChoice::Required;
    req.tools = vec![ToolDef {
        name: "read".into(),
        description: "Read".into(),
        input_schema: json!({"type":"object"}),
        kind: ToolKind::Function,
        defer_loading: false,
    }];
    let body = convert::body(&req).unwrap();
    assert_eq!(body["compaction"], json!({"type":"summarize","instructions":"Keep entity names."}));
    assert!(body.get("stop_sequences").is_none() && body["output_config"].get("format").is_none());
    assert_eq!(body["tool_choice"]["type"], "auto");
    assert_eq!(convert::betas_for(&body), vec![Beta::COMPACT]);
    for seed in 0..10 {
        let events = events(NOW, seed, "anthropic").await;
        assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::Other(r), .. }) if r == "compaction"));
        let u = usage(&events);
        assert_eq!((u.input, u.output), (144, 276), "billed usage sums the iterations");
        let parts = parts(&events);
        let Part::Compaction { summary: Some(summary), opaque: Some(opaque) } = &parts[0] else { panic!("compaction: {parts:?}") };
        assert!(summary.starts_with("Summary of the conversation"));
        assert_eq!(compaction_text(&events), *summary, "the summary streams for display");
        // The block replaces the history and goes first, byte-exact, with the beta on every later request.
        let next = request(vec![Item::new("c", Role::Assistant, parts.clone()), Item::user_text("u2", "Fields for Recipe?")]);
        let body = convert::body(&next).unwrap();
        assert_eq!(body["messages"][0]["content"][0], opaque.data);
        assert!(body["messages"][0]["content"][0].get("cache_control").is_none());
        assert_eq!(convert::betas_for(&body), vec![Beta::COMPACT]);
    }
}

#[tokio::test]
async fn compaction_threshold_delta_and_usage() {
    let mut req = request(vec![Item::user_text("u", "Help me build a website")]);
    req.compaction = Some(Compaction::Auto { threshold: 150_000, instructions: None });
    let body = convert::body(&req).unwrap();
    assert_eq!(
        body["context_management"]["edits"],
        json!([{"type":"compact_20260112","trigger":{"type":"input_tokens","value":150000},"pause_after_compaction":true}])
    );
    assert_eq!(convert::betas_for(&body), vec![Beta::COMPACT_THRESHOLD]);
    let events = events(THRESHOLD, 7, "anthropic").await;
    let parts = parts(&events);
    assert!(matches!(&parts[0], Part::Compaction { summary: Some(s), .. } if s.starts_with("Summary: build a website")));
    assert_eq!(compaction_text(&events), "Summary: build a website; the landing page is done.");
    assert_eq!(parts[1], Part::text("Next, the contact form."));
    let u = usage(&events);
    assert_eq!((u.input, u.output), (203_000, 4_500));
    // An unsigned (threshold) block keeps its beta on replay.
    let body = convert::body(&request(vec![Item::new("a", Role::Assistant, parts), Item::user_text("u2", "go on")])).unwrap();
    assert_eq!(body["messages"][0]["content"][0]["type"], "compaction");
    assert_eq!(convert::betas_for(&body), vec![Beta::COMPACT_THRESHOLD]);
}

#[test]
fn context_editing_beta_and_our_shell() {
    let mut req = request(vec![Item::user_text("u", "edit")]);
    req.provider.anthropic = Some(anthropic::Options {
        context_management: Some(json!({"edits":[{"type":"clear_tool_uses_20250919","keep":{"type":"tool_uses","value":3}}]})),
        ..Default::default()
    });
    let shell = json!({"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"timeout_ms":{"type":"integer"},"background":{"type":"boolean"}},"required":["command"]});
    req.tools = vec![ToolDef {
        name: "shell".into(),
        description: "Run a command".into(),
        input_schema: shell.clone(),
        kind: ToolKind::Function,
        defer_loading: false,
    }];
    let body = convert::body(&req).unwrap();
    assert_eq!(convert::betas_for(&body), vec![Beta::CONTEXT_MANAGEMENT]);
    // Claude gets our shell (with background), not a predefined bash.
    let tool = &body["tools"][0];
    assert_eq!((&tool["name"], &tool["description"], &tool["input_schema"]), (&json!("shell"), &json!("Run a command"), &shell));
    assert!(tool.get("type").is_none());
}

#[tokio::test]
async fn server_tool_blocks_stream_and_replay_byte_exact() {
    for seed in 0..10 {
        let events = events(SERVER, seed, "anthropic").await;
        assert!(events.contains(&StreamEvent::HostedProgress {
            call_id: "srvtoolu_fetch".into(),
            tool: "web_fetch".into(),
            status: "in_progress".into()
        }));
        let parts = parts(&events);
        let kinds: Vec<_> = parts
            .iter()
            .map(|p| match p {
                Part::ToolCall { name, kind: CallKind::Hosted, .. } => format!("call:{name}"),
                Part::HostedResult { tool, .. } => format!("result:{tool}"),
                Part::Text { .. } => "text".into(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["call:web_fetch", "result:web_fetch", "call:bash_code_execution", "result:bash_code_execution", "text"]);
        let u = usage(&events);
        assert!((u.cache_hit_rate().unwrap() - 2000.0 / 2150.0).abs() < 1e-9);
        let body = convert::body(&request(vec![Item::user_text("u", "count words"), Item::new("a", Role::Assistant, parts.clone())])).unwrap();
        let content = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content[1]["content"]["content"]["source"]["data"], "Article content...");
        assert_eq!(content[2]["input"], json!({"command":"wc -w article.txt"}));
        assert_eq!(content[3]["type"], "bash_code_execution_tool_result");
    }
}

#[tokio::test]
async fn minimax_dialect_keeps_its_own_reasoning() {
    let events = events(THINKING, 2, "minimax").await;
    let parts = parts(&events);
    assert!(parts.iter().any(|p| matches!(p, Part::Reasoning { opaque: Some(o), .. } if o.provider.as_str() == "minimax")));
    let req = Request {
        model: "MiniMax-M3".into(),
        items: vec![
            Item::user_text("u", "hi"),
            Item::new("a", Role::Assistant, parts),
            Item::new("s", Role::System, vec![Part::System { text: "be brief".into() }]),
        ],
        ..Request::default()
    };
    let minimax = convert::body_for(&req, "minimax").unwrap();
    assert!(minimax["messages"][1]["content"].as_array().unwrap().iter().any(|b| b["type"] == "thinking"));
    assert_eq!(minimax["messages"].as_array().unwrap().last().unwrap()["role"], "user", "no mid-conversation system role outside Anthropic");
    assert!(convert::body_for(&req, "anthropic").unwrap()["messages"][1]["content"].as_array().unwrap().iter().all(|b| b["type"] != "thinking"));
}
