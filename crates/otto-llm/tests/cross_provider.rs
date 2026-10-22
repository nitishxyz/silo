#![cfg(feature = "client")]
//! The O3 exit criterion, offline: the same scripted tool-using task runs
//! through every provider's request converter and stream parser, and every
//! provider reports usage with a comparable cache hit rate.
use otto_llm::*;
use serde_json::{Value, json};

#[derive(Clone, Copy)]
enum Wire {
    Messages,
    Responses,
    Interactions,
    Chat,
}

/// Every provider with a transport, its wire, and a model id.
const PROVIDERS: [(&str, Wire, &str); 12] = [
    ("anthropic", Wire::Messages, "claude-sonnet-5-5"),
    ("minimax", Wire::Messages, "MiniMax-M3"),
    ("openai", Wire::Responses, "gpt-6.1-sol"),
    ("xai", Wire::Responses, "grok-4.6"),
    ("google", Wire::Interactions, "gemini-3.8-flash"),
    ("openrouter", Wire::Chat, "anthropic/claude-sonnet-5.5"),
    ("deepseek", Wire::Chat, "deepseek-v4-pro"),
    ("moonshotai", Wire::Chat, "kimi-k3"),
    ("zai", Wire::Chat, "glm-5.3"),
    ("ollama", Wire::Chat, "qwen3:8b"),
    ("lmstudio", Wire::Chat, "qwen/qwen3-coder-30b"),
    ("custom", Wire::Chat, "local-model"),
];

fn fixtures(wire: Wire) -> [&'static str; 2] {
    match wire {
        Wire::Messages => [include_str!("fixtures/scripted/anthropic_1.sse"), include_str!("fixtures/scripted/anthropic_2.sse")],
        Wire::Responses => [include_str!("fixtures/scripted/openai_1.sse"), include_str!("fixtures/scripted/openai_2.sse")],
        Wire::Interactions => [include_str!("fixtures/scripted/google_1.sse"), include_str!("fixtures/scripted/google_2.sse")],
        Wire::Chat => [include_str!("fixtures/scripted/chat_1.sse"), include_str!("fixtures/scripted/chat_2.sse")],
    }
}

async fn parse(provider: &str, wire: Wire, model: &str, raw: &str, seed: u64) -> Vec<StreamEvent> {
    let bytes = testing::chunked(raw.as_bytes(), seed);
    testing::collect_events(match wire {
        Wire::Messages => anthropic::stream::decode_for(bytes, model, provider),
        Wire::Responses => openai::stream::stream_for(bytes, model, provider),
        Wire::Interactions => google::stream::stream(bytes, model),
        Wire::Chat => compat::stream::stream(bytes, model, provider),
    })
    .await
}

fn convert(provider: &str, wire: Wire, req: &Request) -> Value {
    match wire {
        Wire::Messages => anthropic::convert::body_for(req, provider).unwrap(),
        Wire::Responses => openai::convert::request_for(req, provider),
        Wire::Interactions => google::convert::request(req).unwrap(),
        Wire::Chat => compat::convert::request(req, compat::dialect(provider).unwrap()).unwrap(),
    }
}

fn read_file() -> ToolDef {
    ToolDef {
        name: "read_file".into(),
        description: "Read a file".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        kind: ToolKind::Function,
        defer_loading: false,
    }
}

/// The "harness": run each tool call it gets.
fn run_tool(name: &str, input: &Value) -> String {
    assert_eq!(name, "read_file");
    assert_eq!(input, &json!({"path":"README.md"}));
    "# otto\nAn agent harness.".into()
}

#[tokio::test]
async fn the_same_scripted_task_passes_on_every_provider() {
    let mut report = Vec::new();
    for (provider, wire, model) in PROVIDERS {
        for seed in 0..5 {
            let [first, second] = fixtures(wire);
            let mut req = Request {
                model: model.into(),
                system: vec!["You are otto.".into()],
                items: vec![Item::user_text("u", "What does the README say?")],
                tools: vec![read_file()],
                ..Request::default()
            };
            let opening = convert(provider, wire, &req);
            assert!(opening.to_string().contains("read_file"), "{provider}: tool declared");
            // Turn 1: the model calls the tool.
            let events = parse(provider, wire, model, first, seed).await;
            assert!(events.iter().any(|e| matches!(e, StreamEvent::ToolInputStart { name, .. } if name == "read_file")), "{provider}: input start");
            assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::ToolUse, .. })), "{provider}: tool use finish");
            let parts: Vec<Part> =
                events.iter().filter_map(|e| if let StreamEvent::PartDone { part } = e { Some(part.clone()) } else { None }).collect();
            let mut results = Vec::new();
            for (call_id, name, input) in Item::new("a1", Role::Assistant, parts.clone()).tool_calls() {
                results.push(Part::ToolResult { call_id: call_id.into(), content: vec![ResultPart::text(run_tool(name, input))], is_error: false });
            }
            assert_eq!(results.len(), 1, "{provider}: one call");
            let Part::ToolResult { call_id, .. } = &results[0] else { unreachable!() };
            let call_id = call_id.clone();
            let mut usage = Usage::default();
            usage.accumulate(&last_usage(&events, provider));
            req.items.push(Item::new("a1", Role::Assistant, parts));
            req.items.push(Item::new("t1", Role::Tool, results));
            // The follow-up request carries the call and its result in the provider's shape.
            let body = convert(provider, wire, &req).to_string();
            assert!(body.contains(&call_id), "{provider}: call id replayed");
            assert!(body.contains("An agent harness."), "{provider}: tool result sent");
            assert!(body.contains("README.md"), "{provider}: call arguments replayed");
            // Turn 2: the answer.
            let events = parse(provider, wire, model, second, seed).await;
            let answer: String = events
                .iter()
                .filter_map(|e| if let StreamEvent::PartDone { part: Part::Text { text, .. } } = e { Some(text.as_str()) } else { None })
                .collect();
            assert_eq!(answer, "README says otto.", "{provider}");
            assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::Stop, .. })), "{provider}: stop");
            let second = last_usage(&events, provider);
            assert!(second.cache_hit_rate().is_some_and(|rate| rate > 0.9), "{provider}: warm cache on turn 2: {second:?}");
            usage.accumulate(&second);
            if seed == 0 {
                report.push(format!(
                    "{provider}: cache hit rate {:.1}% (turn 2 {:.1}%)",
                    usage.cache_hit_rate().unwrap() * 100.0,
                    second.cache_hit_rate().unwrap() * 100.0
                ));
            }
        }
    }
    println!("{}", report.join("\n"));
    assert_eq!(report.len(), PROVIDERS.len());
}

fn last_usage(events: &[StreamEvent], provider: &str) -> Usage {
    events
        .iter()
        .rev()
        .find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None })
        .unwrap_or_else(|| panic!("{provider}: usage"))
}

#[test]
fn every_provider_resolves_without_network() {
    let dir = std::env::temp_dir().join(format!("otto-cross-{}", std::process::id()));
    let providers = providers::Providers::new(dir.join("auth"), dir.clone()).unwrap();
    for (provider, _, model) in PROVIDERS {
        assert!(providers::IDS.contains(&provider), "{provider}");
        let flow = futures::executor::block_on(providers.begin_login(provider, providers::LoginMode::ApiKey)).unwrap();
        futures::executor::block_on(providers.complete_login(flow, "test-key")).unwrap();
        if provider == "custom" {
            assert!(providers.resolve_model(provider, model).is_err(), "custom needs a base URL");
            providers.set_base_url("custom", Some("http://127.0.0.1:9/v1")).unwrap();
        }
        let resolved = providers.resolve_model(provider, model).unwrap_or_else(|e| panic!("{provider}: {e}"));
        assert_eq!(resolved.info().provider.as_str(), provider);
        let status = providers.status().into_iter().find(|s| s.provider == provider).unwrap();
        assert!(status.signed_in && status.mode.as_deref() == Some("api_key"), "{provider}");
    }
    assert_eq!(providers.status().iter().find(|s| s.provider == "custom").unwrap().base_url.as_deref(), Some("http://127.0.0.1:9/v1"));
    assert!(providers.begin_login_blocking_subscription("google"));
    let _ = std::fs::remove_dir_all(dir);
}

trait SubscriptionProbe {
    fn begin_login_blocking_subscription(&self, provider: &str) -> bool;
}
impl SubscriptionProbe for providers::Providers {
    /// Providers without a subscription transport refuse subscription sign-in.
    fn begin_login_blocking_subscription(&self, provider: &str) -> bool {
        futures::executor::block_on(self.begin_login(provider, providers::LoginMode::Subscription)).is_err()
            && self.login_modes(provider) == [auth::AuthMode::ApiKey]
    }
}
