//! Subscription plan usage from response headers and stream events.
use otto_llm::{
    PlanUsage, PlanWindow, StreamEvent,
    plan_usage::{anthropic, codex, codex_event, from_headers, label, window_label},
};
use serde_json::json;

const NOW: i64 = 1_800_000_000;

fn window(name: &str, label: &str, used: f32, resets_at: Option<i64>) -> PlanWindow {
    PlanWindow { name: name.into(), label: label.into(), used, resets_at }
}

#[test]
fn labels_come_from_the_window_length() {
    assert_eq!(label(300), "5-hour");
    assert_eq!(label(10_080), "Weekly");
    assert_eq!(label(60), "1h");
    assert_eq!(label(1440), "1d");
    assert_eq!(label(43_200), "30d");
    assert_eq!(label(90), "2h", "rounded to the hour");
    assert_eq!(label(15), "15m");
    assert_eq!(window_label("5h"), "5-hour");
    assert_eq!(window_label("7d"), "Weekly");
    assert_eq!(window_label("7d_opus"), "Weekly Opus");
    assert_eq!(window_label("24h"), "1d");
    assert_eq!(window_label("overage"), "Overage");
}

#[test]
fn claude_unified_headers_all_windows() {
    let headers = [
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.18"),
        ("anthropic-ratelimit-unified-7d-reset", "1800400000"),
        ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.62"),
        ("anthropic-ratelimit-unified-5h-reset", "1800006000"),
        ("anthropic-ratelimit-unified-7d_opus-utilization", "0.4"),
        ("anthropic-ratelimit-unified-7d_opus-reset", "2027-01-01T00:00:00Z"),
        ("anthropic-ratelimit-unified-reset", "1800006000"),
        ("anthropic-ratelimit-tokens-remaining", "1000"),
    ];
    let usage = anthropic(headers);
    assert_eq!(
        usage.windows,
        vec![
            window("5h", "5-hour", 0.62, Some(1_800_006_000)),
            window("7d", "Weekly", 0.18, Some(1_800_400_000)),
            window("7d_opus", "Weekly Opus", 0.4, Some(1_798_761_600)),
        ],
        "shortest first; a per-model window too; rfc3339 resets"
    );
    assert_eq!(usage.tightest().map(|w| w.name.as_str()), Some("5h"));
}

#[test]
fn claude_headers_partial_malformed_and_percent() {
    let usage = anthropic([("anthropic-ratelimit-unified-5h-utilization", "0.3")]);
    assert_eq!(usage.windows, vec![window("5h", "5-hour", 0.3, None)], "no reset header: none");
    let usage = anthropic([
        ("anthropic-ratelimit-unified-5h-utilization", "nope"),
        ("anthropic-ratelimit-unified-7d-utilization", "-0.1"),
        ("anthropic-ratelimit-unified--utilization", "0.5"),
        ("anthropic-ratelimit-unified-1d-utilization", "NaN"),
    ]);
    assert!(usage.is_empty(), "malformed values are skipped: {usage:?}");
    let usage = anthropic([
        ("anthropic-ratelimit-unified-5h-utilization", "62"),
        ("anthropic-ratelimit-unified-5h-reset", "garbage"),
        ("anthropic-ratelimit-unified-7d-utilization", "1"),
        ("anthropic-ratelimit-unified-7d-reset", "1800400000000"),
        ("anthropic-ratelimit-unified-1d-utilization", "250"),
    ]);
    assert_eq!(
        usage.windows,
        vec![window("5h", "5-hour", 0.62, None), window("1d", "1d", 1.0, None), window("7d", "Weekly", 1.0, Some(1_800_400_000))],
        "a percent above 1 reads as a percent, clamped; 1 is full; a millisecond reset reads as seconds"
    );
    assert!(anthropic([("x-ratelimit-remaining", "5")]).is_empty(), "API-key headers: nothing");
}

#[test]
fn codex_headers() {
    let headers = [
        ("x-codex-primary-used-percent", "42.5"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-after-seconds", "6000"),
        ("x-codex-secondary-used-percent", "18"),
        ("x-codex-secondary-window-minutes", "10080"),
        ("x-codex-secondary-reset-at", "1800400000"),
        ("x-codex-installation-id", "abc"),
    ];
    assert_eq!(
        codex(headers, NOW).windows,
        vec![window("primary", "5-hour", 0.425, Some(NOW + 6000)), window("secondary", "Weekly", 0.18, Some(1_800_400_000))]
    );
    // Partial: no length (named after the window), no reset; malformed percent skipped; over 100 clamped.
    let usage =
        codex([("x-codex-primary-used-percent", "130"), ("x-codex-secondary-used-percent", "lots"), ("x-codex-primary-window-minutes", "x")], NOW);
    assert_eq!(usage.windows, vec![window("primary", "Primary", 1.0, None)]);
    // Percent, never a fraction: 0.5 is half a percent.
    assert_eq!(codex([("x-codex-primary-used-percent", "0.5")], NOW).windows[0].used, 0.005);
    assert!(codex([("x-ratelimit-remaining-requests", "5")], NOW).is_empty());
}

#[test]
fn both_header_families_or_none() {
    assert_eq!(from_headers([("content-type", "text/event-stream")], NOW), None, "API keys and local models: nothing");
    let both = from_headers([("anthropic-ratelimit-unified-5h-utilization", "0.1"), ("x-codex-primary-used-percent", "20")], NOW).unwrap();
    assert_eq!(both.windows.len(), 2);
}

#[test]
fn codex_rate_limits_event() {
    let event = json!({
        "type": "codex.rate_limits",
        "plan_type": "plus",
        "rate_limits": {
            "allowed": true,
            "primary": {"used_percent": 62.0, "window_minutes": 300, "reset_at": 1_800_006_000},
            "secondary": {"used_percent": 18, "window_minutes": 10080, "reset_after_seconds": 400_000},
        },
    });
    assert_eq!(
        codex_event(&event, NOW).unwrap().windows,
        vec![window("primary", "5-hour", 0.62, Some(1_800_006_000)), window("secondary", "Weekly", 0.18, Some(NOW + 400_000))]
    );
    assert_eq!(codex_event(&json!({"type": "codex.rate_limits", "rate_limits": {"primary": {"used_percent": "x"}}}), NOW), None);
    assert_eq!(codex_event(&json!({"type": "codex.rate_limits"}), NOW), None);
}

#[tokio::test]
async fn the_responses_stream_reports_rate_limit_events() {
    use futures::StreamExt;
    let sse = [
        json!({"type":"response.created","response":{"id":"r1","model":"gpt-x"}}),
        json!({"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":50,"window_minutes":300}}}),
        json!({"type":"response.completed","response":{"id":"r1","usage":{"input_tokens":1,"output_tokens":1}}}),
    ]
    .iter()
    .map(|v| format!("data: {v}\n\n"))
    .collect::<String>();
    let bytes = futures::stream::iter([Ok(bytes::Bytes::from(sse))]);
    let events: Vec<_> = otto_llm::openai::stream::stream(bytes, "gpt-x").map(Result::unwrap).collect().await;
    let plans: Vec<_> = events.iter().filter_map(|e| if let StreamEvent::PlanUsage(p) = e { Some(p.clone()) } else { None }).collect();
    assert_eq!(plans, vec![PlanUsage { windows: vec![window("primary", "5-hour", 0.5, None)] }]);
}

#[test]
fn the_tightest_window_and_merging() {
    let mut usage = PlanUsage { windows: vec![window("5h", "5-hour", 0.3, None), window("7d", "Weekly", 0.9, None)] };
    assert_eq!(usage.tightest().map(|w| w.name.as_str()), Some("7d"));
    assert_eq!(PlanUsage::default().tightest(), None);
    usage.merge(PlanUsage { windows: vec![window("5h", "5-hour", 0.95, Some(1)), window("7d_opus", "Weekly Opus", 0.1, None)] });
    assert_eq!(usage.windows.iter().map(|w| (w.name.as_str(), w.used)).collect::<Vec<_>>(), [("5h", 0.95), ("7d", 0.9), ("7d_opus", 0.1)]);
    assert_eq!(usage.tightest().map(|w| w.name.as_str()), Some("5h"));
}
