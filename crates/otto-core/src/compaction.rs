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

/// Context editing: keep the newest results whole; clear older ones once
/// their total exceeds `budget` bytes. Pairs stay intact (only content changes).
pub(crate) fn edit(mut items: Vec<Item>, budget: usize) -> Vec<Item> {
    let mut remaining = budget;
    let mut seen = 0;
    for item in items.iter_mut().rev().filter(|i| i.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }))) {
        seen += 1;
        for part in &mut item.parts {
            let Part::ToolResult { content, .. } = part else { continue };
            let size: usize = content.iter().map(|c| if let ResultPart::Text { text } = c { text.len() } else { 4096 }).sum();
            if seen <= KEEP_RESULTS || size <= remaining {
                remaining = remaining.saturating_sub(size);
            } else if !(content.len() == 1 && matches!(&content[0], ResultPart::Text { text } if text == CLEARED)) {
                *content = vec![ResultPart::text(CLEARED)];
            }
        }
    }
    items
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

/// Ask the provider to compact `items`; its checkpoint replaces them.
async fn compact_native(cx: &Context, live: &Live, cancel: &CancellationToken, turn: i64, items: Vec<Item>) -> Result<String> {
    let request = Request {
        model: live.row.model.clone(),
        system: live.system.clone(),
        items,
        compaction: Some(otto_llm::Compaction::Now { instructions: None }),
        ..Default::default()
    };
    let mut stream = live.model.stream(request, CallCx { cancel: cancel.clone(), session: Some(cx.row.id.clone()) }).await?;
    let mut checkpoint = None;
    while let Some(event) = tokio::select! {_=cancel.cancelled()=>return Err(Error::Closed),event=stream.next()=>event} {
        if let StreamEvent::PartDone { part: part @ Part::Compaction { .. } } = event? {
            checkpoint = Some(part);
        }
    }
    let Some(part) = checkpoint else { return Err(Error::Invalid("the provider returned no compaction".into())) };
    let summary = if let Part::Compaction { summary, .. } = &part { summary.clone().unwrap_or_default() } else { String::new() };
    let mut item = Item::new(crate::id(), Role::Assistant, vec![part]);
    item.origin = Some(otto_llm::Origin { provider: live.row.provider.as_str().into(), model: live.row.model.clone(), response_id: None });
    cx.item(item, turn, 0).await?;
    Ok(summary)
}

/// Replace the history with a checkpoint summary. `trigger` is `manual` or `auto`.
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
    if native(&live, &items) {
        let id = cx.row.id.clone();
        let through = cx.runtime.db(move |s| Ok(s.session(&id)?.ok_or(Error::Closed)?.last_seq)).await?;
        let summary = compact_native(cx, &live, cancel, turn, items).await?;
        cx.emit(otto_proto::Event::Compaction { through_seq: through, summary: Some(summary).filter(|s| !s.is_empty()) }, Projection::None, turn, 0)
            .await?;
        return Ok(());
    }
    let budget = usize::try_from(if window == 0 { 32000 } else { window / 2 }).unwrap_or(32000).clamp(1024, 128000);
    let instructions = bounded(&items.iter().filter(|i| i.role == Role::User).map(Item::text).collect::<Vec<_>>().join("\n"), budget / 4);
    let history = summary_input(&items, budget);
    let id = cx.row.id.clone();
    let through = cx.runtime.db(move |s| Ok(s.session(&id)?.ok_or(Error::Closed)?.last_seq)).await?;
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
    let mut stream = live.model.stream(request, CallCx { cancel: cancel.clone(), session: None }).await?;
    let mut summary = String::new();
    while let Some(event) = tokio::select! {_=cancel.cancelled()=>return Err(Error::Closed),event=stream.next()=>event} {
        if let StreamEvent::PartDone { part: Part::Text { text, .. } } = event? {
            let remaining = budget.saturating_sub(summary.len());
            summary.push_str(&bounded(&text, remaining));
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
    summary.push_str("\nMost recent context (verbatim, truncated):\n");
    summary.push_str(&tail.join("\n"));
    summary.push_str("\nOriginal user instructions (verbatim):\n");
    summary.push_str(&instructions);
    cx.item(Item::new(crate::id(), Role::System, vec![Part::Compaction { summary: Some(summary.clone()), opaque: None }]), turn, 0).await?;
    cx.emit(otto_proto::Event::Compaction { through_seq: through, summary: Some(summary) }, Projection::None, turn, 0).await?;
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
        let edited = super::edit(super::pairs(items), 50_000);
        let texts: Vec<String> = edited.iter().filter(|i| i.role == Role::Tool).map(|i| serde_json::to_string(&i.parts).unwrap()).collect();
        assert_eq!(texts.len(), 6);
        assert!(texts[..3].iter().all(|t| t.contains("cleared")));
        assert!(texts[3..].iter().all(|t| !t.contains("cleared")));
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
