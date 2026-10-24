//! Context management (docs/OTTO.md §7.3): otto's checkpoint summarizer (the
//! default), opt-in provider-native compaction, and context editing.
use crate::{
    Error, Result,
    session::{Context, Live},
};
use futures::StreamExt;
use otto_llm::{CallCx, Item, Part, Request, ResultPart, Role, StreamEvent};
use otto_store::Projection;
use std::collections::HashSet;
use tokio_util::sync::CancellationToken;

/// The newest tool result items always kept verbatim by context editing.
const KEEP_RESULTS: usize = 3;
const CLEARED: &str = "[tool result cleared to save context; call the tool again if you need it]";

pub(crate) fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    if limit < 32 {
        return text.chars().take(limit / 4).collect();
    }
    let half = (limit - 24) / 2;
    let mut head = half.min(text.len());
    while !text.is_char_boundary(head) {
        head -= 1
    }
    let mut tail = text.len().saturating_sub(half);
    while !text.is_char_boundary(tail) {
        tail += 1
    }
    format!("{}\n[... truncated ...]\n{}", &text[..head], &text[tail..])
}
fn summary_input(items: &[Item], budget: usize) -> String {
    let mut history = String::new();
    for item in items {
        let text = serde_json::to_string(item).unwrap_or_default();
        history.push_str(&bounded(&text, 4096));
        history.push('\n');
        if history.len() > budget * 2 {
            history = bounded(&history, budget);
        }
    }
    bounded(&history, budget)
}

/// Tool-result budget in bytes for a context window in tokens (~4 bytes each, a quarter of the window).
fn result_budget(window: u64) -> usize {
    usize::try_from(window.saturating_mul(4) / 4).unwrap_or(usize::MAX).clamp(32_000, 400_000)
}

/// The request view of the history: everything from the last checkpoint on,
/// call/result pairs repaired, and old tool results cleared above the budget.
pub(crate) fn trim(mut items: Vec<Item>, window: u64) -> Vec<Item> {
    if let Some(index) = items.iter().rposition(|i| i.parts.iter().any(|p| matches!(p, Part::Compaction { .. }))) {
        items = items.split_off(index);
        for part in &mut items[0].parts {
            // Native checkpoints (opaque) replay as they are; the provider
            // layer drops or converts them for any other provider.
            if let Part::Compaction { summary, opaque: None } = part {
                *part = Part::text(summary.clone().unwrap_or_default());
            }
        }
    }
    edit(pairs(items), result_budget(window))
}

/// Every result has its call earlier in the history, and every call has a result.
pub(crate) fn pairs(items: Vec<Item>) -> Vec<Item> {
    let mut calls = HashSet::new();
    let answered: HashSet<String> = items
        .iter()
        .flat_map(|i| &i.parts)
        .filter_map(|p| if let Part::ToolResult { call_id, .. } = p { Some(call_id.clone()) } else { None })
        .collect();
    let mut out = Vec::with_capacity(items.len());
    for mut item in items {
        item.parts.retain(|p| !matches!(p, Part::ToolResult { call_id, .. } if !calls.contains(call_id)));
        if item.parts.is_empty() {
            continue;
        }
        let missing: Vec<Part> = item
            .tool_calls()
            .filter(|(id, ..)| !answered.contains(*id))
            .map(|(id, ..)| Part::ToolResult {
                call_id: id.into(),
                content: vec![ResultPart::text("result missing: the call did not complete")],
                is_error: true,
            })
            .collect();
        calls.extend(item.tool_calls().map(|(id, ..)| id.to_owned()));
        let id = format!("{}-missing", item.id);
        out.push(item);
        if !missing.is_empty() {
            out.push(Item::new(id, Role::Tool, missing));
        }
    }
    out
}

/// Context editing: keep the newest results whole; once all results exceed
/// `budget` bytes, clear the oldest in chunks. Pairs stay intact (only content
/// changes).
///
/// The amount cleared is a step function of the results' total size, which
/// only grows, and is taken from the oldest end. So between steps the cleared
/// set stays the same until the total passes the next chunk, and then only
/// grows: the start of the prompt stays byte-identical and the provider's
/// prompt cache keeps hitting. Counting back from the newest result instead
/// moved the cut on every step and rewrote the whole cached prompt each time.
pub(crate) fn edit(mut items: Vec<Item>, budget: usize) -> Vec<Item> {
    let size = |content: &[ResultPart]| -> usize { content.iter().map(|c| if let ResultPart::Text { text } = c { text.len() } else { 4096 }).sum() };
    let results: Vec<(usize, usize, usize)> = items
        .iter()
        .enumerate()
        .flat_map(|(i, item)| {
            item.parts
                .iter()
                .enumerate()
                .filter_map(move |(j, p)| if let Part::ToolResult { content, .. } = p { Some((i, j, content)) } else { None })
        })
        .map(|(i, j, content)| (i, j, size(content)))
        .collect();
    let target = clear_target(results.iter().map(|r| r.2).sum(), budget);
    // The newest results (by item) are never cleared.
    let mut kept_items: Vec<usize> = results.iter().map(|r| r.0).collect();
    kept_items.dedup();
    let protected: HashSet<usize> = kept_items.iter().rev().take(KEEP_RESULTS).copied().collect();
    let mut cleared = 0;
    for &(i, j, bytes) in &results {
        if cleared >= target || protected.contains(&i) {
            break;
        }
        cleared += bytes;
        if let Part::ToolResult { content, .. } = &mut items[i].parts[j]
            && !(content.len() == 1 && matches!(&content[0], ResultPart::Text { text } if text == CLEARED))
        {
            *content = vec![ResultPart::text(CLEARED)];
        }
    }
    items
}

/// Bytes of the oldest results to clear: none up to `budget`; past it, enough
/// to leave about 60% of the budget, in steps of 40% of the budget, so the
/// kept results range over 60–100% of the budget between steps.
fn clear_target(total: usize, budget: usize) -> usize {
    if total <= budget {
        return 0;
    }
    let chunk = (budget * 2 / 5).max(1);
    let floor = budget - chunk;
    (total - floor) / chunk * chunk
}

/// Provider-native compaction is opt-in (session `native_compaction`), needs a
/// model with `caps.native_compaction`, and a history this provider and model
/// produced alone, so its opaque checkpoint can be replayed.
fn native(live: &Live, items: &[Item]) -> bool {
    live.row.meta["native_compaction"] == true
        && live.model.info().caps.native_compaction
        && items.iter().filter_map(|i| i.origin.as_ref()).all(|o| o.provider.0 == live.row.provider && o.model == live.row.model)
}

/// Provider threshold compaction for an ordinary request of an opted-in
/// session, at 75% of the window (before otto's own 85% trigger).
pub(crate) fn threshold(live: &Live, items: &[Item]) -> Option<otto_llm::Compaction> {
    let window = live.model.info().caps.context_window;
    (window > 0 && native(live, items)).then(|| otto_llm::Compaction::Auto { threshold: window * 3 / 4, instructions: None })
}

/// The session's newest journal sequence: what a checkpoint taken now summarizes through.
pub(crate) async fn last_seq(cx: &Context) -> Result<i64> {
    let id = cx.row.id.clone();
    cx.runtime.db(move |s| Ok(s.session(&id)?.ok_or(Error::Closed)?.last_seq)).await
}

/// Summary text for display while compaction `id` streams: live only, never
/// journaled (the finished checkpoint is).
pub(crate) fn delta(cx: &Context, id: &str, text: String) {
    if !text.is_empty() {
        let _ = cx.deltas.send(otto_proto::Event::CompactionDelta { id: id.into(), text });
    }
}

/// Ask the provider to compact `items`; its checkpoint replaces them, stored as item `id`.
async fn compact_native(cx: &Context, live: &Live, cancel: &CancellationToken, turn: i64, id: &str, items: Vec<Item>) -> Result<String> {
    let request = Request {
        model: live.row.model.clone(),
        system: live.system.clone(),
        items,
        compaction: Some(otto_llm::Compaction::Now { instructions: None }),
        ..Default::default()
    };
    let mut stream = live.model.stream(request, CallCx { cancel: cancel.clone(), session: Some(cx.row.id.clone()), ..CallCx::default() }).await?;
    let mut checkpoint = None;
    while let Some(event) = tokio::select! {_=cancel.cancelled()=>return Err(Error::Closed),event=stream.next()=>event} {
        match event? {
            StreamEvent::CompactionDelta { text } => delta(cx, id, text),
            StreamEvent::PartDone { part: part @ Part::Compaction { .. } } => checkpoint = Some(part),
            _ => {}
        }
    }
    let Some(part) = checkpoint else { return Err(Error::Invalid("the provider returned no compaction".into())) };
    let summary = if let Part::Compaction { summary, .. } = &part { summary.clone().unwrap_or_default() } else { String::new() };
    let mut item = Item::new(id, Role::Assistant, vec![part]);
    item.origin = Some(otto_llm::Origin { provider: live.row.provider.as_str().into(), model: live.row.model.clone(), response_id: None });
    cx.item(item, turn, 0).await?;
    Ok(summary)
}

/// Replace the history with a checkpoint summary. `trigger` is `manual` or `auto`.
///
/// Clients see `compaction.started`, the summary as `compaction.delta`s (live
/// only), then `compaction` with the checkpoint, or `compaction.failed`.
pub(crate) async fn compact(cx: &Context, cancel: &CancellationToken, turn: i64, trigger: &str) -> Result<()> {
    let hook = crate::step::apply_hook(cx, "PreCompact", trigger, serde_json::json!({"trigger":trigger,"custom_instructions":""}), turn, 0).await?;
    if let Some(reason) = hook.deny {
        return Err(Error::Invalid(reason));
    }
    let live = cx.live();
    let window = live.model.info().caps.context_window;
    let items = trim(cx.items().await?, window);
    if items.is_empty() {
        return Ok(());
    }
    let id = crate::id();
    cx.emit(otto_proto::Event::CompactionStarted { id: id.clone(), trigger: trigger.into() }, Projection::None, turn, 0).await?;
    let result = checkpoint(cx, &live, cancel, turn, &id, items, window).await;
    if let Err(error) = &result {
        failed(cx, &id, error, cancel.is_cancelled(), turn, 0).await;
    }
    result
}

/// Journal `compaction.failed` for compaction `id` (best effort: a closing session may not take it).
pub(crate) async fn failed(cx: &Context, id: &str, error: &(dyn std::fmt::Display + Sync), cancelled: bool, turn: i64, step: i64) {
    let message = if cancelled { "Stopped".to_owned() } else { error.to_string() };
    let event = otto_proto::Event::CompactionFailed { id: id.into(), message, cancelled };
    let _ = cx.emit(event, Projection::None, turn, step).await;
}

async fn checkpoint(cx: &Context, live: &Live, cancel: &CancellationToken, turn: i64, id: &str, items: Vec<Item>, window: u64) -> Result<()> {
    if native(live, &items) {
        let through = last_seq(cx).await?;
        let summary = compact_native(cx, live, cancel, turn, id, items).await?;
        let event = otto_proto::Event::Compaction { through_seq: through, summary: Some(summary).filter(|s| !s.is_empty()), id: Some(id.into()) };
        cx.emit(event, Projection::None, turn, 0).await?;
        return Ok(());
    }
    let budget = usize::try_from(if window == 0 { 32000 } else { window / 2 }).unwrap_or(32000).clamp(1024, 128000);
    let instructions = bounded(&items.iter().filter(|i| i.role == Role::User).map(Item::text).collect::<Vec<_>>().join("\n"), budget / 4);
    let history = summary_input(&items, budget);
    let through = last_seq(cx).await?;
    let request = Request {
        model: live.row.model.clone(),
        system: vec![
            "Summarize the conversation so work can continue from the summary alone. Use these sections: Goal, Decisions, Current state, Files touched, Open issues, Next steps. Keep exact filenames, commands, identifiers and error messages. Do not execute tools."
                .into(),
        ],
        items: vec![Item::user_text(crate::id(), history)],
        max_output: Some((budget / 8).clamp(128, 4096) as u32),
        ..Default::default()
    };
    let mut stream = live.model.stream(request, CallCx { cancel: cancel.clone(), session: None, ..CallCx::default() }).await?;
    let mut summary = String::new();
    while let Some(event) = tokio::select! {_=cancel.cancelled()=>return Err(Error::Closed),event=stream.next()=>event} {
        match event? {
            StreamEvent::TextDelta { text } => delta(cx, id, text),
            StreamEvent::PartDone { part: Part::Text { text, .. } } => {
                let remaining = budget.saturating_sub(summary.len());
                summary.push_str(&bounded(&text, remaining));
            }
            _ => {}
        }
    }
    if summary.trim().is_empty() {
        return Err(Error::Invalid("empty compaction summary".into()));
    }
    // The newest exchange verbatim (bounded), so a mid-turn checkpoint keeps its footing.
    let tail: Vec<String> = items
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|i| {
            format!(
                "[{}] {}",
                serde_json::to_value(i.role).ok().and_then(|r| r.as_str().map(String::from)).unwrap_or_default(),
                bounded(&serde_json::to_string(&i.parts).unwrap_or_default(), budget / 16)
            )
        })
        .collect();
    summary.push_str(otto_proto::COMPACTION_CONTEXT);
    summary.push_str(&tail.join("\n"));
    summary.push_str("\nOriginal user instructions (verbatim):\n");
    summary.push_str(&instructions);
    cx.item(Item::new(id, Role::System, vec![Part::Compaction { summary: Some(summary.clone()), opaque: None }]), turn, 0).await?;
    cx.emit(otto_proto::Event::Compaction { through_seq: through, summary: Some(summary), id: Some(id.into()) }, Projection::None, turn, 0).await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use otto_llm::{CallKind, Item, Part, ResultPart, Role};
    #[test]
    fn huge_history_is_bounded() {
        let items = (0..100).map(|_| otto_llm::Item::user_text("id", "é".repeat(100000))).collect::<Vec<_>>();
        let text = super::summary_input(&items, 8192);
        assert!(text.len() <= 8192);
        assert!(text.contains("truncated"));
    }
    fn call(id: &str) -> Item {
        Item::new(
            format!("a{id}"),
            Role::Assistant,
            vec![Part::ToolCall { call_id: id.into(), name: "read".into(), input: serde_json::json!({}), kind: CallKind::Function, opaque: None }],
        )
    }
    fn result(id: &str, size: usize) -> Item {
        Item::new(
            format!("r{id}"),
            Role::Tool,
            vec![Part::ToolResult { call_id: id.into(), content: vec![ResultPart::text("x".repeat(size))], is_error: false }],
        )
    }
    #[test]
    fn editing_clears_old_results_and_keeps_pairs() {
        let mut items = vec![Item::user_text("u", "go")];
        for i in 0..6 {
            items.push(call(&i.to_string()));
            items.push(result(&i.to_string(), 20_000));
        }
        // 120 kB of results over a 50 kB budget: the oldest 80 kB are due (past the 30 kB floor in 20 kB steps), but
        // the newest three results stay, so three are cleared.
        let edited = super::edit(super::pairs(items), 50_000);
        let texts: Vec<String> = edited.iter().filter(|i| i.role == Role::Tool).map(|i| serde_json::to_string(&i.parts).unwrap()).collect();
        assert_eq!(texts.len(), 6);
        assert!(texts[..3].iter().all(|t| t.contains("cleared")));
        assert!(texts[3..].iter().all(|t| !t.contains("cleared")));
    }
    #[test]
    fn the_newest_results_are_never_cleared() {
        let mut items = vec![Item::user_text("u", "go")];
        for i in 0..4 {
            items.push(call(&i.to_string()));
            items.push(result(&i.to_string(), 100_000));
        }
        let edited = super::edit(super::pairs(items), 50_000);
        let cleared: Vec<bool> =
            edited.iter().filter(|i| i.role == Role::Tool).map(|i| serde_json::to_string(&i.parts).unwrap().contains("cleared")).collect();
        assert_eq!(cleared, [true, false, false, false]);
    }
    /// The prompt cache needs a stable prefix: step after step, the cleared
    /// results are the same until a chunk boundary, and then only grow, from
    /// the oldest end.
    #[test]
    fn cleared_results_only_grow_from_the_oldest() {
        let mut items = vec![Item::user_text("u", "go")];
        let mut previous: Vec<bool> = Vec::new();
        let mut changes = 0;
        for i in 0..400 {
            items.push(call(&i.to_string()));
            items.push(result(&i.to_string(), 500 + (i * 7919) % 6_000));
            let edited = super::edit(super::pairs(items.clone()), 100_000);
            let cleared: Vec<bool> =
                edited.iter().filter(|i| i.role == Role::Tool).map(|i| serde_json::to_string(&i.parts).unwrap().contains("cleared")).collect();
            // A prefix: once a result is kept, every newer one is too.
            assert!(cleared.windows(2).all(|w| w[0] || !w[1]), "step {i}: cleared is not a prefix");
            // Only grows: everything cleared before stays cleared.
            assert!(previous.iter().zip(&cleared).all(|(was, now)| !was || *now), "step {i}: a cleared result came back");
            if previous.iter().filter(|c| **c).count() != cleared.iter().filter(|c| **c).count() {
                changes += 1;
            }
            previous = cleared;
        }
        // ~1.3 MB of results over a 100 kB budget: a change every ~40 kB, not every step.
        assert!(changes > 0 && changes < 40, "{changes} changes in 400 steps");
    }
    #[test]
    fn orphans_are_repaired() {
        let items = vec![result("gone", 10), Item::user_text("u", "go"), call("open")];
        let repaired = super::pairs(items);
        assert_eq!(repaired.len(), 3);
        assert_eq!(repaired[0].role, Role::User);
        assert!(matches!(&repaired[2].parts[0], Part::ToolResult { call_id, is_error: true, .. } if call_id == "open"));
    }
}
