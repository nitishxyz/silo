use crate::{
    Error, Result,
    approvals::Bridge,
    session::{Context, Live},
};
use futures::StreamExt;
use otto_llm::*;
use otto_proto::{ApprovalDecision, Event, InputPart, QueuedInput};
use otto_store::{Projection, ToolStatus, UsageRecord};
use otto_tools::policy::{Decision, Rule, RuleDecision};
use otto_tools::{ToolCx, ToolOutput};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) async fn run(cx: Arc<Context>, input: QueuedInput, turn: i64, cancel: CancellationToken) -> Result<()> {
    cx.emit(Event::TurnStarted { turn }, Projection::TurnStatus(otto_store::TurnStatus::Running), turn, 0).await?;
    let mut result = run_inner(cx.clone(), input, turn, &cancel).await;
    if !cancel.is_cancelled() {
        let error = result.as_ref().err().map(ToString::to_string);
        if let Err(error) = crate::subagents::completed(&cx, turn, if error.is_some() { "error" } else { "done" }, error).await
            && result.is_ok()
        {
            result = Err(error);
        }
    }
    let status = if cancel.is_cancelled() {
        otto_proto::TurnStatus::Cancelled
    } else if result.is_err() {
        otto_proto::TurnStatus::Error
    } else {
        otto_proto::TurnStatus::Done
    };
    if let Err(error) = &result {
        let (class, message, retryable) = match error {
            Error::Model(e) => {
                let provider = cx.live().row.provider.clone();
                let resets_in = if e.class == ErrorClass::QuotaExhausted { plan_reset(&cx, &provider) } else { None };
                let class = serde_json::to_value(e.class)?.as_str().unwrap_or("other").to_owned();
                (class, e.describe(&otto_llm::providers::display_name(&provider), resets_in), e.retryable())
            }
            _ => ("other".into(), error.to_string(), false),
        };
        cx.emit(Event::Error { class, message, retryable }, Projection::None, turn, 0).await?;
    }
    let stored = match status {
        otto_proto::TurnStatus::Cancelled => otto_store::TurnStatus::Cancelled,
        otto_proto::TurnStatus::Error => otto_store::TurnStatus::Error,
        _ => otto_store::TurnStatus::Done,
    };
    let session_id = cx.row.id.clone();
    let usage = cx
        .runtime
        .db(move |s| {
            let mut total = Usage::default();
            for event in s.events_since(&session_id, 0, usize::MAX)? {
                if event.turn == turn && event.kind == "usage" {
                    let usage: Usage = serde_json::from_value(event.payload["usage"].clone())?;
                    total.input += usage.input;
                    total.output += usage.output;
                    total.cache_read += usage.cache_read;
                    total.cache_write += usage.cache_write;
                    total.reasoning += usage.reasoning;
                }
            }
            Ok(total)
        })
        .await?;
    cx.emit(Event::TurnFinished { turn, status, usage }, Projection::TurnStatus(stored), turn, 0).await?;
    result
}
/// A retry wait as the `model.retrying` event.
fn retrying(notice: &RetryNotice) -> Event {
    let at_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    Event::ModelRetrying {
        class: serde_json::to_value(notice.class).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "other".into()),
        message: notice.message.clone(),
        delay_ms: u64::try_from(notice.delay.as_millis()).unwrap_or(u64::MAX),
        attempt: notice.attempt,
        max_attempts: notice.max_attempts,
        at_ms,
    }
}
/// When `provider`'s spent plan windows reset (the last of them), from its last plan usage.
fn plan_reset(cx: &Context, provider: &str) -> Option<std::time::Duration> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let now = i64::try_from(now).ok()?;
    let plans = cx.runtime.plan_usage(Some(provider));
    let at = plans.iter().flat_map(|p| &p.usage.windows).filter(|w| w.used >= 0.99).filter_map(|w| w.resets_at).filter(|at| *at > now).max()?;
    Some(std::time::Duration::from_secs(u64::try_from(at - now).ok()?))
}
/// The input as a user item's parts and the `@` mentions in their text.
async fn parts(cx: &Context, input: Vec<InputPart>) -> Result<(Vec<Part>, Vec<Mention>)> {
    let mut parts = Vec::new();
    let mut mentions = Vec::new();
    let mut run = Vec::new();
    for part in input {
        if matches!(part, InputPart::Text { .. } | InputPart::Mention { .. }) {
            run.push(part);
            continue;
        }
        text_run(cx, std::mem::take(&mut run), &mut parts, &mut mentions)?;
        parts.push(match part {
            InputPart::Text { .. } | InputPart::Mention { .. } => continue,
            InputPart::Image { mime, data_base64 } => Part::Image(Media { mime, source: MediaSource::Base64 { data: data_base64 }, name: None }),
            InputPart::File { path } => {
                Part::text(format!("Attached file {path}:\n{}", tokio::fs::read_to_string(std::path::Path::new(&cx.row.cwd).join(&path)).await?))
            }
            InputPart::Attachment { id } => {
                parts.extend(attachment(cx, &id).await?);
                continue;
            }
        });
    }
    text_run(cx, run, &mut parts, &mut mentions)?;
    Ok((parts, mentions))
}
/// Text parts in a row: each its own part, or with a mention among them one part with each
/// mention as `@target`, its range in [`Item::text`] noted and checked (an unknown agent, a missing path).
fn text_run(cx: &Context, run: Vec<InputPart>, parts: &mut Vec<Part>, mentions: &mut Vec<Mention>) -> Result<()> {
    let user = &cx.runtime.0.config.user_config_dir;
    if !run.iter().any(|p| matches!(p, InputPart::Mention { .. })) {
        for part in run {
            if let InputPart::Text { text } = part {
                parts.push(Part::text(crate::commands::expand(&cx.root, user, &text)?));
            }
        }
        return Ok(());
    }
    let base: usize = parts.iter().map(|p| if let Part::Text { text, .. } = p { text.len() } else { 0 }).sum();
    let agents: Vec<String> = crate::agents::discover(&cx.root, user).into_iter().map(|a| a.name).collect();
    let mut text = String::new();
    for part in run {
        match part {
            InputPart::Text { text: t } => text.push_str(&t),
            InputPart::Mention { kind, target } => {
                let start = base + text.len();
                text.push('@');
                text.push_str(&target);
                let found = match kind {
                    MentionKind::Agent => agents.contains(&target),
                    MentionKind::File => project_path(&cx.row.cwd, &target),
                };
                mentions.push(Mention { kind, range: [start, base + text.len()], missing: !found, target });
            }
            _ => {}
        }
    }
    parts.push(Part::text(crate::commands::expand(&cx.root, user, &text)?));
    Ok(())
}
/// `target` is a file, or a folder when it ends in `/`, inside `cwd`.
fn project_path(cwd: &str, target: &str) -> bool {
    let relative = std::path::Path::new(target.trim_end_matches('/'));
    let inside = !target.is_empty() && relative.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
    inside && std::fs::metadata(std::path::Path::new(cwd).join(relative)).is_ok_and(|m| !target.ends_with('/') || m.is_dir())
}
/// The model's note on a user item's mentions (those found): which agents to delegate to, which paths to read.
pub(crate) fn mention_note(mentions: &[Mention]) -> Option<String> {
    let named = |kind: MentionKind| {
        let mut names: Vec<&str> = Vec::new();
        for m in mentions.iter().filter(|m| m.kind == kind && !m.missing) {
            if !names.contains(&m.target.as_str()) {
                names.push(&m.target);
            }
        }
        names
    };
    let (agents, files) = (named(MentionKind::Agent), named(MentionKind::File));
    let mut lines = Vec::new();
    if let [one] = agents.as_slice() {
        lines.push(format!("Mentioned agents: {one}. Delegate the matching part with subagent {{agent:\"{one}\"}}."));
    } else if !agents.is_empty() {
        lines.push(format!("Mentioned agents: {}. Delegate the matching parts with subagent {{agent:\"<name>\"}}.", agents.join(", ")));
    }
    if !files.is_empty() {
        lines.push(format!("Mentioned files: {} (paths relative to the project; read them as needed).", files.join(", ")));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}
/// User items with mentions get their note after the text, for the model only.
pub(crate) fn noted(mut items: Vec<Item>) -> Vec<Item> {
    for item in &mut items {
        if let Some(note) = mention_note(&item.mentions) {
            item.parts.push(Part::text(note));
        }
    }
    items
}
/// An attachment's note, then the image, downscaled, when it is one (kept for the transcript; a model
/// without vision isn't sent it: [`visible`]).
async fn attachment(cx: &Context, id: &str) -> Result<Vec<Part>> {
    let dir = otto_tools::attachments::dir(&cx.runtime.attachments_root(), &cx.row.id);
    let (attachment, path) = otto_tools::attachments::get(&dir, id)?;
    let mut parts = vec![Part::text(attachment.note())];
    let bytes = tokio::fs::read(&path).await?;
    if otto_tools::images::decodable(&bytes) {
        let prepared =
            tokio::task::spawn_blocking(move || otto_tools::images::prepare(&bytes, otto_tools::images::MAX_EDGE, otto_tools::images::QUALITY))
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?;
        if let Ok(image) = prepared {
            let data = otto_tools::attachments::encode(&image.bytes);
            parts.push(Part::Image(Media { mime: image.mime.into(), source: MediaSource::Base64 { data }, name: Some(attachment.name) }));
        }
    }
    Ok(parts)
}
/// What a model sees of `items`: one without vision gets a line in place of each image (an
/// attachment's note still says what it is, so `copy_attachment` works).
pub(crate) fn visible(mut items: Vec<Item>, vision: bool) -> Vec<Item> {
    if vision {
        return items;
    }
    let note = |m: &Media| format!("[{} not sent: this model can't view images]", m.name.as_deref().unwrap_or("image"));
    for part in items.iter_mut().flat_map(|i| i.parts.iter_mut()) {
        match part {
            Part::Image(m) => *part = Part::text(note(m)),
            Part::ToolResult { content, .. } => {
                for c in content.iter_mut() {
                    if let ResultPart::Image(m) = c {
                        *c = ResultPart::text(note(m));
                    }
                }
            }
            _ => {}
        }
    }
    items
}
/// Commit a user input unless a UserPromptSubmit hook blocks it (`false`).
async fn input(cx: &Context, input: QueuedInput, turn: i64, step: i64) -> Result<bool> {
    // An outbox wake has no input of its own; the deliveries are the input.
    if input.input.is_empty() {
        return Ok(true);
    }
    let prompt = input
        .input
        .iter()
        .filter_map(|p| match p {
            InputPart::Text { text } => Some(text.clone()),
            InputPart::Mention { target, .. } => Some(format!("@{target}")),
            _ => None,
        })
        .collect::<String>();
    let hook = apply_hook(cx, "UserPromptSubmit", "", serde_json::json!({"prompt":prompt}), turn, step).await?;
    if let Some(reason) = hook.deny.or(hook.stop) {
        cx.emit(
            Event::Error { class: "hook".into(), message: format!("Prompt blocked by a hook: {reason}"), retryable: false },
            Projection::None,
            turn,
            step,
        )
        .await?;
        return Ok(false);
    }
    let (parts, mentions) = parts(cx, input.input).await?;
    let mut item = Item::new(input.id, Role::User, parts);
    item.mentions = mentions;
    cx.item(item, turn, step).await?;
    Ok(true)
}
/// Hand pending steering input to the live model stream, in arrival order.
/// What the model cannot take stays queued for the next step boundary.
async fn hand_off(cx: &Context, session: &dyn ModelSession, built: &mut std::collections::HashMap<String, Item>) -> Result<()> {
    let mut queued = cx.state(|s| std::mem::take(&mut s.steer)).await?.into_iter();
    let mut handed = Vec::new();
    for next in queued.by_ref() {
        let outcome = match parts(cx, next.input.clone()).await {
            Ok((parts, mentions)) => {
                let mut item = Item::new(next.id.clone(), Role::User, parts);
                item.mentions = mentions;
                let shown = noted(vec![item.clone()]).remove(0).parts;
                match session.steer(&next.id, shown).await {
                    Ok(true) => {
                        built.insert(next.id.clone(), item);
                        handed.push(next);
                        continue;
                    }
                    Ok(false) => Ok(()),
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => Err(error),
        };
        let mut kept = vec![next];
        kept.extend(queued);
        cx.state(move |s| {
            s.native.extend(handed);
            kept.append(&mut s.steer);
            s.steer = kept;
        })
        .await?;
        return outcome;
    }
    cx.state(move |s| s.native.extend(handed)).await
}
/// Steering the stream did not commit goes back to the boundary queue, first.
async fn requeue_native(cx: &Context) -> Result<()> {
    cx.state(|s| {
        let mut native = std::mem::take(&mut s.native);
        native.append(&mut s.steer);
        s.steer = native;
    })
    .await
}
/// Persist a finished assistant item and its usage.
async fn settle(
    cx: &Context,
    live: &Live,
    item: &Item,
    lookup: &std::collections::HashMap<String, Arc<dyn otto_tools::Tool>>,
    usage: &Usage,
    turn: i64,
    step: i64,
) -> Result<()> {
    let mut persisted = item.clone();
    for part in &mut persisted.parts {
        if let Part::ToolCall { name, input, .. } = part {
            if let Some(tool) = lookup.get(name) {
                *input = otto_tools::policy::redact(input, &tool.spec().sensitive);
            }
            *input = crate::prepared::public(input.take());
        }
    }
    cx.item(persisted, turn, step).await?;
    let cost = live.model.info().pricing.as_ref().map(|p| {
        (usage.input as f64 * p.input
            + usage.output as f64 * p.output
            + usage.cache_read as f64 * p.cache_read
            + usage.cache_write as f64 * p.cache_write)
            / 1_000_000.0
    });
    cx.emit(
        Event::Usage {
            turn,
            step,
            usage: usage.clone(),
            cost_usd: cost,
            // A provider that reported no usage leaves the last known prompt size.
            context: otto_proto::ContextUse {
                used: Some(usage.total_input())
                    .filter(|n| *n > 0)
                    .unwrap_or_else(|| cx.live().row.meta["context"]["used"].as_u64().unwrap_or_default()),
                window: live.model.info().caps.context_window,
            },
        },
        Projection::Usage(UsageRecord {
            provider: live.row.provider.clone(),
            model: live.row.model.clone(),
            usage: usage.clone(),
            cost_usd: cost.unwrap_or_default(),
        }),
        turn,
        step,
    )
    .await?;
    Ok(())
}
async fn run_inner(cx: Arc<Context>, first: QueuedInput, turn: i64, cancel: &CancellationToken) -> Result<()> {
    if cx
        .state(|s| {
            let first = !s.started;
            s.started = true;
            first
        })
        .await?
    {
        let source = if cx.row.last_seq == 0 { "startup" } else { "resume" };
        apply_hook(&cx, "SessionStart", source, serde_json::json!({"source":source}), turn, 0).await?;
    }
    let prompt_id = first.id.clone();
    if !input(&cx, first, turn, 0).await? {
        return Ok(());
    }
    let mut stop_continuations = 0;
    let limit = cx.live().row.meta["max_steps"].as_u64().filter(|n| *n > 0).unwrap_or(200) as i64;
    let mut continuations = 0;
    let mut overflow = false;
    'steps: for step in 1..=limit {
        // `session.update` takes effect here: one snapshot per model request.
        let live = cx.live();
        let family = live.family();
        let loaded = crate::tools::loaded(&cx).await?;
        // Sources (MCP) are listed every step, so list changes apply at once
        // and providers always get the complete current tool list.
        let mut tools = cx.runtime.builtins(&family);
        tools.extend(cx.runtime.extra_tools());
        tools.extend(cx.runtime.source_tools(&cx.root).await);
        let session_tools = crate::tools::tools(cx.clone(), &tools, &loaded);
        tools.extend(session_tools);
        let mut defs = Vec::new();
        let mut lookup = std::collections::HashMap::new();
        let mut asynchronous = Vec::new();
        for tool in tools {
            let spec = tool.spec();
            if !crate::tools::granted(&live, spec) {
                continue;
            }
            let presentation = spec.presentation(&family);
            let name = presentation.map_or_else(|| spec.name.clone(), |p| p.name.clone());
            if !crate::tools::behind_loader(&live, spec) || loaded.contains(&spec.name) {
                let kind =
                    presentation.and_then(|p| p.freeform.clone()).map_or(ToolKind::Function, |format| ToolKind::Custom { format: Some(format) });
                if spec.mode == otto_tools::CallMode::Async && matches!(kind, ToolKind::Function | ToolKind::Custom { .. }) {
                    asynchronous.push(name.clone());
                }
                defs.push(ToolDef {
                    name: name.clone(),
                    description: presentation.and_then(|p| p.description.clone()).unwrap_or_else(|| spec.description.clone()),
                    input_schema: presentation.and_then(|p| p.input_schema.clone()).unwrap_or_else(|| spec.input_schema.clone()),
                    kind,
                    defer_loading: false,
                });
            }
            lookup.insert(name, tool);
        }
        let lookup = Arc::new(lookup);
        let offered: Arc<str> = {
            let mut names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
            names.sort_unstable();
            names.join(", ").into()
        };
        if cancel.is_cancelled() {
            return Ok(());
        }
        crate::subagents::deliver(&cx, turn, step).await?;
        let steer = cx.state(|s| std::mem::take(&mut s.steer)).await?;
        if !steer.is_empty() {
            cx.queue_changed(turn, step).await?;
        }
        for queued in steer {
            input(&cx, queued, turn, step).await?;
        }
        let items = visible(noted(crate::compaction::trim(cx.items().await?, live.model.info().caps.context_window)), live.model.info().caps.vision);
        let compaction = crate::compaction::threshold(&live, &items);
        let mut provider = ProviderOptions::default();
        let caps = &live.model.info().caps;
        if caps.async_tools && !asynchronous.is_empty() {
            provider.openai = Some(otto_llm::openai::Options { async_tools: asynchronous, ..Default::default() });
        }
        let request = Request {
            model: live.row.model.clone(),
            system: live.system.clone(),
            items,
            tools: defs.clone(),
            parallel_tools: Some(caps.parallel_tools),
            reasoning: serde_json::from_value::<Effort>(live.row.meta["effort"].clone()).ok().filter(|_| caps.reasoning).map(Reasoning::Effort),
            provider,
            compaction: compaction.clone(),
            ..Default::default()
        };
        // Waits to retry the request (inside the provider client) are shown as they start.
        let (retry_tx, mut retry_rx) = tokio::sync::mpsc::unbounded_channel::<RetryNotice>();
        let call = CallCx {
            cancel: cancel.clone(),
            session: Some(cx.row.id.clone()),
            retry: Some(RetryHook::new(move |notice| drop(retry_tx.send(notice.clone())))),
        };
        let pending = live.model.stream(request, call);
        tokio::pin!(pending);
        let stream = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                Some(notice) = retry_rx.recv() => cx.emit(retrying(&notice), Projection::None, turn, step).await.map(drop)?,
                s = &mut pending => break s,
            }
        };
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) if e.class == ErrorClass::ContextOverflow && !overflow => {
                overflow = true;
                crate::compaction::compact(&cx, cancel, turn, "auto").await?;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let mut item = Item::new(crate::id(), Role::Assistant, vec![]);
        item.origin = Some(Origin { provider: live.row.provider.as_str().into(), model: live.row.model.clone(), response_id: None });
        cx.emit(Event::ItemStarted { item_id: item.id.clone(), role: Role::Assistant }, Projection::None, turn, step).await?;
        let mut usage = Usage::default();
        let mut reason = FinishReason::Stop;
        let mut deltas = crate::deltas::Deltas::new(&cx.row.id);
        let mut tick = tokio::time::interval(otto_store::Coalescer::MAX_DELAY);
        // Native steering (§7.2): only models whose live session can take it.
        let session = live.model.session().filter(|_| caps.steering);
        let mut steered = std::collections::HashMap::new();
        loop {
            let event = tokio::select! {
                _=cancel.cancelled()=>{deltas.flush(&cx).await?;return Ok(())},
                _=tick.tick()=>{deltas.flush(&cx).await?;continue},
                _=cx.steer_wake.notified(),if session.is_some()=>{
                    if let Some(session)=session {hand_off(&cx,session,&mut steered).await?;}
                    continue
                },
                event=stream.next()=>event
            };
            let Some(event) = event else { break };
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    deltas.flush(&cx).await?;
                    if error.class == ErrorClass::ContextOverflow && !overflow {
                        overflow = true;
                        requeue_native(&cx).await?;
                        crate::compaction::compact(&cx, cancel, turn, "auto").await?;
                        continue 'steps;
                    }
                    return Err(error.into());
                }
            };
            match event {
                StreamEvent::Start { response_id, .. } => item.origin.as_mut().unwrap().response_id = response_id,
                StreamEvent::PartDone { part } => item.parts.push(part),
                StreamEvent::TextDelta { text } => {
                    deltas.push(&cx, Event::TextDelta { item_id: item.id.clone(), text }, turn, step).await?;
                }
                StreamEvent::ReasoningDelta { text, summary } => {
                    deltas.push(&cx, Event::ReasoningDelta { item_id: item.id.clone(), text, summary }, turn, step).await?;
                }
                StreamEvent::ToolInputStart { call_id, name } => {
                    cx.emit(Event::ToolInputStart { call_id, name }, Projection::None, turn, step).await?;
                }
                StreamEvent::ToolInputDelta { call_id, delta } => {
                    cx.emit(Event::ToolInputDelta { call_id, delta }, Projection::None, turn, step).await?;
                }
                StreamEvent::Usage(u) => usage = u,
                StreamEvent::PlanUsage(plan) => cx.runtime.record_plan(live.row.provider.as_str(), plan),
                // The provider applied steering input mid-response and goes on
                // in a new response: close this item, then the user input.
                StreamEvent::Steered { ids } => {
                    deltas.flush(&cx).await?;
                    settle(&cx, &live, &item, &lookup, &usage, turn, step).await?;
                    for id in &ids {
                        if let Some(user) = steered.remove(id) {
                            cx.item(user, turn, step).await?;
                        }
                    }
                    cx.state(move |s| s.native.retain(|q| !ids.contains(&q.id))).await?;
                    cx.queue_changed(turn, step).await?;
                    item = Item::new(crate::id(), Role::Assistant, vec![]);
                    item.origin = Some(Origin { provider: live.row.provider.as_str().into(), model: live.row.model.clone(), response_id: None });
                    cx.emit(Event::ItemStarted { item_id: item.id.clone(), role: Role::Assistant }, Projection::None, turn, step).await?;
                    usage = Usage::default();
                    reason = FinishReason::Stop;
                }
                StreamEvent::Finish { reason: r, response_id } => {
                    reason = r;
                    if response_id.is_some() {
                        item.origin.as_mut().unwrap().response_id = response_id;
                    }
                }
                _ => {}
            }
        }
        deltas.flush(&cx).await?;
        drop(stream);
        // Limits a provider reports out of band (Kimi's polled usage), off the turn's path.
        let (model, runtime, provider) = (live.model.clone(), cx.runtime.clone(), live.row.provider.as_str().to_owned());
        tokio::spawn(async move {
            if let Some(plan) = model.plan_usage().await {
                runtime.record_plan(&provider, plan);
            }
        });
        requeue_native(&cx).await?;
        settle(&cx, &live, &item, &lookup, &usage, turn, step).await?;
        let calls = item.tool_calls().map(|(id, name, value)| (id.to_owned(), name.to_owned(), value.clone())).collect::<Vec<_>>();
        if calls.is_empty() {
            if reason == FinishReason::Other("compaction".into()) && item.parts.iter().any(|p| matches!(p, Part::Compaction { .. })) {
                // Threshold compaction paused after its checkpoint, which drops
                // everything before it (this turn's prompt too): the prompt
                // follows the checkpoint verbatim, then the turn carries on.
                let prompt = cx.items().await?.into_iter().find(|i| i.id == prompt_id && i.role == Role::User);
                if let Some(prompt) = prompt {
                    cx.item(Item::new(crate::id(), Role::User, prompt.parts), turn, step).await?;
                }
                continue;
            }
            if reason == FinishReason::PauseTurn {
                continue;
            }
            if reason == FinishReason::MaxTokens && continuations < 3 {
                continuations += 1;
                cx.item(Item::user_text(crate::id(), "Continue from where you stopped."), turn, step).await?;
                continue;
            }
            if cx.state(|s| !s.steer.is_empty()).await? {
                continue;
            }
            // Stop / SubagentStop hooks may keep the turn going (bounded).
            let event = if crate::subagents::subagent(&cx.row) { "SubagentStop" } else { "Stop" };
            let hook = apply_hook(&cx, event, "", serde_json::json!({"stop_hook_active":stop_continuations > 0}), turn, step).await?;
            if let (Some(reason), None, true) = (hook.deny, hook.stop, stop_continuations < 3) {
                stop_continuations += 1;
                cx.item(Item::user_text(crate::id(), format!("{event} hook: {reason}")), turn, step).await?;
                continue;
            }
            return Ok(());
        }
        let exclusive = calls.iter().any(|(_, name, _)| lookup.get(name).is_some_and(|t| t.spec().exclusive));
        let mut jobs = Vec::new();
        let result_id = crate::id();
        for (call_id, name, value) in calls {
            jobs.push(execute(cx.clone(), lookup.clone(), offered.clone(), call_id, name, value, turn, step, cancel.clone(), result_id.clone()));
        }
        let outputs = if exclusive {
            let mut outputs = Vec::new();
            for job in jobs {
                outputs.push(job.await?)
            }
            outputs
        } else {
            futures::future::try_join_all(jobs).await?
        };
        cx.item(Item::new(result_id, Role::Tool, outputs), turn, step).await?;
        for text in cx.state(|s| std::mem::take(&mut s.after_tools)).await? {
            cx.item(Item::new(crate::id(), Role::System, vec![Part::System { text }]), turn, step).await?;
        }
        if let Some(reason) = cx.state(|s| s.halt.take()).await? {
            cx.emit(
                Event::Error { class: "hook".into(), message: format!("Stopped by a hook: {reason}"), retryable: false },
                Projection::None,
                turn,
                step,
            )
            .await?;
            return Ok(());
        }
        // With provider threshold compaction the provider compacts first.
        let window = caps.context_window;
        if compaction.is_none() && window > 0 && usage.total_input() > window * 85 / 100 {
            crate::compaction::compact(&cx, cancel, turn, "auto").await?;
        }
    }
    Err(Error::Invalid("maximum steps per turn reached".into()))
}
pub(crate) async fn apply_hook(
    cx: &Context,
    event: &str,
    tool: &str,
    input: serde_json::Value,
    turn: i64,
    step: i64,
) -> Result<crate::hooks::Outcome> {
    let outcome = crate::hooks::run(cx, event, tool, input).await?;
    for text in &outcome.context {
        cx.item(Item::new(crate::id(), Role::System, vec![Part::System { text: text.clone() }]), turn, step).await?;
    }
    for message in &outcome.notices {
        cx.emit(Event::Error { class: "hook".into(), message: message.clone(), retryable: false }, Projection::None, turn, step).await?;
    }
    Ok(outcome)
}
/// Hooks run while tools execute: their context waits until the tool result
/// item is committed, so call/result pairs stay adjacent.
async fn tool_hook(cx: &Context, event: &str, tool: &str, fields: serde_json::Value, turn: i64, step: i64) -> Result<crate::hooks::Outcome> {
    let mut outcome = crate::hooks::run(cx, event, tool, fields).await?;
    for message in &outcome.notices {
        cx.emit(Event::Error { class: "hook".into(), message: message.clone(), retryable: false }, Projection::None, turn, step).await?;
    }
    let context = std::mem::take(&mut outcome.context);
    cx.state(move |s| s.after_tools.extend(context)).await?;
    Ok(outcome)
}
#[allow(clippy::too_many_arguments)]
async fn execute(
    cx: Arc<Context>,
    lookup: Arc<std::collections::HashMap<String, Arc<dyn otto_tools::Tool>>>,
    offered: Arc<str>,
    call_id: String,
    name: String,
    value: serde_json::Value,
    turn: i64,
    step: i64,
    cancel: CancellationToken,
    result_id: String,
) -> Result<Part> {
    let original = crate::prepared::public(value);
    let bridge = Arc::new(Bridge { cx: cx.clone(), cancel: cancel.clone(), turn, step });
    let mut tcx = ToolCx::new(&call_id, &cx.row.cwd);
    tcx.cancel = cancel.clone();
    tcx.project_root = cx.root.clone();
    tcx.approver = Some(bridge.clone());
    tcx.spill_dir = Some(cx.runtime.0.config.state_dir.join("outputs").join(&cx.row.id));
    tcx.attachments = Some(cx.runtime.attachments_root());
    tcx.vision = cx.live().model.info().caps.vision;
    tcx.sessions = crate::mailbox::lineage(&cx.runtime, &cx.row.id).await?;
    tcx.mailbox = Some(Arc::new(crate::mailbox::Mailbox { runtime: cx.runtime.clone(), session: cx.row.id.clone() }));
    let tool = lookup.get(&name).cloned();
    let mut effective = original.clone();
    let mut prepared = None;
    let decision = if let Some(tool) = &tool {
        match crate::prepared::prepare(&cx, tool.clone(), original.clone(), tcx.clone()).await {
            Err(error) => Decision::Deny { reason: error.to_string() },
            Ok(initial) => match tool_hook(
                &cx,
                "PreToolUse",
                &name,
                serde_json::json!({"tool_name":name,"tool_input":crate::prepared::public(initial.input.clone())}),
                turn,
                step,
            )
            .await
            {
                Err(error) => Decision::Deny { reason: error.to_string() },
                Ok(hooks) => {
                    let next = if let Some(updated) = hooks.input {
                        effective = crate::prepared::public(updated);
                        crate::prepared::prepare(&cx, tool.clone(), effective.clone(), tcx.clone()).await
                    } else {
                        Ok(initial)
                    };
                    match next {
                        Err(error) => Decision::Deny { reason: error.to_string() },
                        Ok(p) => {
                            if let Some(reason) = hooks.stop.clone() {
                                cx.state(move |s| s.halt = Some(reason)).await?;
                            }
                            let decision = match (hooks.deny.or(hooks.stop), &p.decision) {
                                (Some(reason), _) => Decision::Deny { reason },
                                (None, Decision::Allow) if hooks.ask => Decision::Ask { summary: format!("A hook asks to confirm {name}") },
                                (None, decision) => decision.clone(),
                            };
                            prepared = Some(p);
                            decision
                        }
                    }
                }
            },
        }
    } else if cx.live().row.meta["grant"].is_array() {
        Decision::Deny { reason: format!("{name} is not among the tools this sub-agent was given. Available tools: {offered}") }
    } else {
        Decision::Deny { reason: format!("unknown tool: {name}. Available tools: {offered}") }
    };
    let redacted = crate::prepared::card(tool.as_ref(), &original, &effective, prepared.as_ref());
    cx.emit(
        Event::ToolCall { call_id: call_id.clone(), name: name.clone(), input: redacted.clone() },
        Projection::ToolIntent { call_id: call_id.clone(), name: name.clone(), input: redacted.clone() },
        turn,
        step,
    )
    .await?;
    let denied = match decision {
        Decision::Allow => None,
        Decision::Deny { reason } => Some(reason),
        Decision::Ask { summary } => match bridge.gate(&call_id, &name, &summary, redacted.clone()).await? {
            ApprovalDecision::Deny => Some("user denied the tool".into()),
            ApprovalDecision::Allow => None,
            ApprovalDecision::AlwaysAllow => {
                let name = name.clone();
                let path = cx.root.join(".otto/rules.json");
                cx.state(move |s| {
                    s.policy.rules.insert(0, Rule { tool: name, pattern: None, decision: RuleDecision::Allow });
                    s.policy.save(&path)
                })
                .await?
                .map_err(|e| Error::Invalid(e.to_string()))?;
                None
            }
        },
    };
    let output = if let Some(reason) = denied {
        ToolOutput::error(reason)
    } else if let Some(tool) = tool {
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        tcx.progress = Some(progress_tx);
        let value = prepared.ok_or_else(|| Error::Invalid("missing prepared tool input".into()))?.input;
        let execution = tool.call(value, tcx.clone());
        tokio::pin!(execution);
        loop {
            tokio::select! {
                _=cancel.cancelled()=>break ToolOutput::error("cancelled; outcome may be unknown"),
                result=&mut execution=>break result.unwrap_or_else(|e|ToolOutput::error(e.to_string())),
                Some(progress)=progress_rx.recv()=>{
                    cx.emit(Event::ToolProgress { call_id:call_id.clone(), message:progress.message, output:progress.output.map(serde_json::Value::String) }, Projection::None, turn, step).await?;
                }
            }
        }
    } else {
        ToolOutput::error("unknown tool")
    };
    let mut limited = otto_tools::output::limit(output.joined_text(), &tcx).map_err(|e| Error::Invalid(e.to_string()))?;
    limited.is_error = output.is_error;
    limited.metadata = output.metadata;
    let mut content = vec![ResultPart::text(limited.joined_text())];
    for part in output.content {
        match part {
            otto_tools::OutputPart::Image { mime, data_base64 } => {
                content.push(ResultPart::Image(Media { mime: mime.clone(), source: MediaSource::Base64 { data: data_base64.clone() }, name: None }));
                limited.content.push(otto_tools::OutputPart::Image { mime, data_base64 });
            }
            otto_tools::OutputPart::Document { mime, data_base64, name } => {
                let media = Media { mime: mime.clone(), source: MediaSource::Base64 { data: data_base64.clone() }, name: name.clone() };
                let live = cx.live();
                if live.model.info().caps.pdf {
                    content.push(ResultPart::Document(media));
                    limited.content.push(otto_tools::OutputPart::Document { mime, data_base64, name });
                } else {
                    content.push(ResultPart::text(media.unsupported(&format!("{}/{}", live.row.provider, live.row.model))));
                }
            }
            otto_tools::OutputPart::File { path } => {
                content.push(ResultPart::text(format!("Output file: {}", path.display())));
                limited.content.push(otto_tools::OutputPart::File { path });
            }
            _ => {}
        }
    }
    let serialized = serde_json::to_value(&limited)?;
    let hook =
        tool_hook(&cx, "PostToolUse", &name, serde_json::json!({"tool_name":name,"tool_input":effective,"tool_response":serialized}), turn, step)
            .await?;
    let blocked = hook.deny.map(|reason| format!("PostToolUse hook on {name} ({call_id}): {reason}"));
    cx.state(move |s| {
        s.after_tools.extend(blocked);
        if hook.stop.is_some() {
            s.halt = hook.stop;
        }
    })
    .await?;
    cx.emit(
        Event::ToolResult { call_id: call_id.clone(), is_error: output.is_error, output: serialized.clone(), item_id: result_id },
        Projection::ToolOutcome {
            call_id: call_id.clone(),
            status: if output.is_error { ToolStatus::Error } else { ToolStatus::Done },
            output: serialized,
        },
        turn,
        step,
    )
    .await?;
    Ok(Part::ToolResult { call_id, content, is_error: output.is_error })
}
