//! `SessionView.part_times` from journal marks.

use otto_proto::{
    Item, Part, PartTime,
    timing::{Mark, part_times},
};

fn mark(kind: &str, at: i64, item: Option<&str>, call: Option<&str>) -> Mark {
    Mark { kind: kind.into(), at, item_id: item.map(Into::into), call_id: call.map(Into::into) }
}

#[test]
fn reasoning_runs_until_what_follows_and_calls_until_their_result() {
    let item = Item::new(
        "a",
        otto_llm::Role::Assistant,
        vec![
            Part::Reasoning { summary: None, text: Some("hmm".into()), opaque: None },
            Part::text("Looking."),
            Part::ToolCall {
                call_id: "c1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
                kind: otto_llm::CallKind::Function,
                opaque: None,
            },
            Part::ToolCall {
                call_id: "c2".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
                kind: otto_llm::CallKind::Function,
                opaque: None,
            },
        ],
    );
    let marks = [
        mark("item.started", 1_000, Some("a"), None),
        mark("reasoning.delta", 1_050, Some("a"), None),
        mark("reasoning.delta", 3_000, Some("a"), None),
        mark("text.delta", 4_000, Some("a"), None),
        // The call is timed from its start: its name, before any input streams.
        mark("tool.input_start", 4_400, None, Some("c1")),
        mark("tool.input_delta", 4_500, None, Some("c1")),
        mark("item.done", 5_000, Some("a"), None),
        mark("tool.call", 5_010, None, Some("c1")),
        mark("tool.call", 5_020, None, Some("c2")),
        mark("tool.result", 6_000, None, Some("c1")),
        // c2 waited for an approval that never came: the turn's end stops it.
        mark("turn.finished", 9_000, None, None),
    ];
    let times = part_times(&[item], &marks);
    let at = |part: u32, started_at: i64, finished_at: Option<i64>| PartTime { item_id: "a".into(), part, started_at, finished_at };
    assert_eq!(times, [at(0, 1_050, Some(4_000)), at(2, 4_400, Some(6_000)), at(3, 5_000, Some(9_000))]);
}

#[test]
fn unstreamed_reasoning_takes_the_item_and_running_parts_stay_open() {
    let item = Item::new(
        "b",
        otto_llm::Role::Assistant,
        vec![
            Part::Reasoning { summary: Some("s".into()), text: None, opaque: None },
            Part::ToolCall {
                call_id: "c".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
                kind: otto_llm::CallKind::Function,
                opaque: None,
            },
        ],
    );
    let marks = [mark("item.started", 10, Some("b"), None), mark("item.done", 20, Some("b"), None), mark("tool.call", 30, None, Some("c"))];
    let times = part_times(&[item, Item::user_text("u", "hi")], &marks);
    assert_eq!(times.len(), 2);
    assert_eq!((times[0].started_at, times[0].finished_at), (20, Some(20)));
    assert_eq!((times[1].started_at, times[1].finished_at), (20, None), "still running");
    // Nothing journaled (a forked session): no times.
    assert!(
        part_times(&[Item::new("x", otto_llm::Role::Assistant, vec![Part::Reasoning { summary: None, text: None, opaque: None }])], &[]).is_empty()
    );
}
