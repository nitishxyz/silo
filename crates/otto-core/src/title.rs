//! Automatic session titles: one short call after the first finished turn.
use crate::{Error, Result, compaction::bounded, session::Context};
use futures::StreamExt;
use otto_llm::{CallCx, Item, Model, Part, Request, Role, StreamEvent};
use std::{sync::Arc, time::Duration};

const PROMPT: &str = "Write a title of at most 6 words for this conversation. Reply with the title only: no quotes, no trailing punctuation.";
const LIMIT: usize = 60;

/// Name the session unless it got a title meanwhile. Failures leave it untitled.
pub(crate) async fn name(cx: &Context) -> Result<()> {
    let items = cx.items().await?;
    let first = |role| items.iter().find(|i| i.role == role && !i.text().trim().is_empty()).map(Item::text);
    let user = first(Role::User).ok_or_else(|| Error::Invalid("no user message to title".into()))?;
    let mut text = format!("User:\n{}", bounded(&user, 2000));
    if let Some(reply) = first(Role::Assistant) {
        text.push_str(&format!("\n\nAssistant:\n{}", bounded(&reply, 500)));
    }
    let live = cx.live();
    let (model, id) = small(&cx.runtime, &live.row.provider).unwrap_or_else(|| (live.model.clone(), live.row.model.clone()));
    let request = Request { model: id, system: vec![PROMPT.into()], items: vec![Item::user_text(crate::id(), text)], ..Default::default() };
    let call = async {
        let mut stream = model.stream(request, CallCx::default()).await?;
        let mut out = String::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::PartDone { part: Part::Text { text, .. } } = event? {
                out.push_str(&text);
            }
        }
        Ok::<_, Error>(out)
    };
    let out = tokio::time::timeout(Duration::from_secs(60), call).await.map_err(|_| Error::Invalid("title request timed out".into()))??;
    let title = clean(&out).ok_or_else(|| Error::Invalid("the model returned no title".into()))?;
    cx.runtime.auto_rename(cx.row.id.clone(), title).await?;
    Ok(())
}

/// The provider's cheapest priced model the resolver lists.
pub(crate) fn small(runtime: &crate::Runtime, provider: &str) -> Option<(Arc<dyn Model>, String)> {
    let models = &runtime.0.config.models;
    let (_, id) = models
        .list()
        .into_iter()
        .filter(|m| m.provider.0 == provider)
        .filter_map(|m| m.pricing.as_ref().map(|p| (p.input + p.output, m.id.clone())))
        .min_by(|a, b| a.0.total_cmp(&b.0))?;
    Some((models.resolve(provider, &id).ok()?, id))
}

fn clean(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.strip_prefix("Title:").or_else(|| line.strip_prefix("title:")).unwrap_or(line);
    let quotes = |c: char| matches!(c, '"' | '\'' | '`' | '*' | '#' | '\u{201c}' | '\u{201d}');
    let line = line.trim().trim_matches(quotes).trim().trim_end_matches(['.', '!', '?', ',', ';', ':']).trim_matches(quotes).trim();
    let mut title: String = line.chars().take(LIMIT).collect();
    if line.chars().count() > LIMIT
        && let Some(space) = title.rfind(char::is_whitespace)
    {
        title.truncate(space);
    }
    let title = title.trim().to_owned();
    (!title.is_empty()).then_some(title)
}
