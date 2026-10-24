//! When the timed parts of a session's items ran (`SessionView.part_times`),
//! derived from otto's journal so a reloaded session reads as it did live.
//! Wall time, as a client watching the session sees it: a reasoning part runs
//! from its first delta until what follows it streams (or the item, or the
//! turn, ends); a tool call from when it first shows (its input streaming,
//! the item calling it, or the call) until its result, else the end of its
//! turn, so approval and answer waits count.

use otto_llm::{Item, Part, Role};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// When part `part` of item `item_id` ran (ms since the Unix epoch).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PartTime {
    pub item_id: String,
    /// Index into the item's parts.
    pub part: u32,
    pub started_at: i64,
    /// `None` while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
}

/// One journal event of the kinds in [`MARKS`], without its payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mark {
    pub kind: String,
    pub at: i64,
    /// `item_id` of item and delta events, `item.id` of `item.done`.
    pub item_id: Option<String>,
    pub call_id: Option<String>,
}

/// The journal events timing reads.
pub const MARKS: &[&str] = &[
    "reasoning.delta",
    "text.delta",
    "tool.input_start",
    "tool.input_delta",
    "tool.call",
    "tool.result",
    "item.started",
    "item.done",
    "turn.finished",
];

/// Times of the reasoning parts and tool calls of `items`, from `marks` in journal order.
#[must_use]
pub fn part_times(items: &[Item], marks: &[Mark]) -> Vec<PartTime> {
    use std::collections::HashMap;
    // One pass over the journal: per item its `item.done` and reasoning runs
    // (the k-th run of deltas is its k-th reasoning part; it ends with whatever
    // is journaled next), per call its first sighting and result.
    let (mut done, mut runs, mut first, mut result) =
        (HashMap::new(), HashMap::<&str, Vec<(i64, Option<i64>)>>::new(), HashMap::new(), HashMap::<&str, Vec<usize>>::new());
    let mut finished = Vec::new();
    for (i, m) in marks.iter().enumerate() {
        match (m.kind.as_str(), m.item_id.as_deref(), m.call_id.as_deref()) {
            ("item.done", Some(item), _) => {
                done.entry(item).or_insert(i);
            }
            ("reasoning.delta", Some(item), _) if i == 0 || marks[i - 1].kind != "reasoning.delta" || marks[i - 1].item_id != m.item_id => {
                let end = marks[i + 1..].iter().find(|n| n.kind != "reasoning.delta" || n.item_id != m.item_id).map(|n| n.at);
                runs.entry(item).or_default().push((m.at, end));
            }
            ("tool.input_start" | "tool.input_delta" | "tool.call", _, Some(call)) => {
                first.entry(call).or_insert(i);
            }
            ("tool.result", _, Some(call)) => result.entry(call).or_default().push(i),
            ("turn.finished", ..) => finished.push(i),
            _ => {}
        }
    }
    let after = |list: &[usize], start: usize| list.get(list.partition_point(|&i| i < start)).copied();
    let mut out = Vec::new();
    for item in items.iter().filter(|i| i.role == Role::Assistant) {
        let done = done.get(item.id.as_str()).copied();
        let mut runs = runs.get(item.id.as_str()).into_iter().flatten().copied();
        for (pi, part) in item.parts.iter().enumerate() {
            let (started_at, finished_at) = match part {
                // Not streamed: it showed, done, with the item.
                Part::Reasoning { .. } => match (runs.next(), done) {
                    (Some(run), _) => run,
                    (None, Some(d)) => (marks[d].at, Some(marks[d].at)),
                    (None, None) => continue,
                },
                Part::ToolCall { call_id, .. } => {
                    let Some(start) = [first.get(call_id.as_str()).copied(), done].into_iter().flatten().min() else { continue };
                    let ends = [result.get(call_id.as_str()).and_then(|r| after(r, start)), after(&finished, start)];
                    (marks[start].at, ends.into_iter().flatten().min().map(|e| marks[e].at))
                }
                _ => continue,
            };
            out.push(PartTime { item_id: item.id.clone(), part: u32::try_from(pi).unwrap_or(u32::MAX), started_at, finished_at });
        }
    }
    out
}
