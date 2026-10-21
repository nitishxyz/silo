use crate::{Error, Result, session::Context};
use futures::StreamExt;
use otto_llm::{CallCx, Item, Part, Request, Role, StreamEvent};
use otto_store::Projection;
use tokio_util::sync::CancellationToken;
fn bounded(text: &str, limit: usize) -> String {
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
pub(crate) fn trim(mut items: Vec<Item>) -> Vec<Item> {
    if let Some(index) = items.iter().rposition(|i| i.parts.iter().any(|p| matches!(p, Part::Compaction { .. }))) {
        items = items.split_off(index);
        for part in &mut items[0].parts {
            if let Part::Compaction { summary, .. } = part {
                *part = Part::text(summary.clone().unwrap_or_default());
            }
        }
    }
    let mut budget = 32000usize;
    for item in items.iter_mut().rev() {
        for part in &mut item.parts {
            if let Part::ToolResult { content, .. } = part {
                for result in content {
                    if let otto_llm::ResultPart::Text { text } = result {
                        if text.len() > budget { *text = "[output cleared to save context]".into() } else { budget -= text.len() }
                    }
                }
            }
        }
    }
    items
}
pub(crate) async fn compact(cx: &Context, cancel: &CancellationToken, turn: i64) -> Result<()> {
    let hook = crate::step::apply_hook(cx, "PreCompact", "", serde_json::Value::Null, turn, 0).await?;
    if let Some(reason) = hook.deny {
        return Err(Error::Invalid(reason));
    }
    let items = cx.items().await?;
    if items.is_empty() {
        return Ok(());
    }
    let window = cx.model.info().caps.context_window;
    let budget = usize::try_from(if window == 0 { 32000 } else { window / 2 }).unwrap_or(32000).clamp(1024, 128000);
    let instructions = bounded(&items.iter().filter(|i| i.role == Role::User).map(Item::text).collect::<Vec<_>>().join("\n"), budget / 4);
    let history = summary_input(&items, budget);
    let id = cx.row.id.clone();
    let through = cx.runtime.db(move |s| Ok(s.session(&id)?.ok_or(Error::Closed)?.last_seq)).await?;
    let request = Request {
        model: cx.row.model.clone(),
        system: vec![
            "Summarize the conversation for continuation. Preserve decisions, task state, filenames, and unresolved issues. Do not execute tools."
                .into(),
        ],
        items: vec![Item::user_text(crate::id(), history)],
        max_output: Some((budget / 8).clamp(128, 4096) as u32),
        ..Default::default()
    };
    let mut stream = cx.model.stream(request, CallCx { cancel: cancel.clone(), session: None }).await?;
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
    summary.push_str("\nOriginal user instructions (verbatim):\n");
    summary.push_str(&instructions);
    cx.item(Item::new(crate::id(), Role::System, vec![Part::Compaction { summary: Some(summary.clone()), opaque: None }]), turn, 0).await?;
    cx.emit(otto_proto::Event::Compaction { through_seq: through, summary: Some(summary) }, Projection::None, turn, 0).await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn huge_history_is_bounded() {
        let items = (0..100).map(|_| otto_llm::Item::user_text("id", "é".repeat(100000))).collect::<Vec<_>>();
        let text = super::summary_input(&items, 8192);
        assert!(text.len() <= 8192);
        assert!(text.contains("truncated"));
    }
}
