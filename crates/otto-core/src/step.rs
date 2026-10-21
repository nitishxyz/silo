use crate::{Error, Result, approvals::Bridge, session::Context};
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
        if let Err(error) = apply_hook(&cx, "Stop", "", serde_json::Value::Null, turn, 0).await {
            result = Err(error);
        }
        if let Err(error) = crate::subagents::completed(&cx).await {
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
        let class = match error {
            Error::Model(e) => serde_json::to_value(e.class)?.as_str().unwrap_or("other").to_owned(),
            _ => "other".into(),
        };
        cx.emit(Event::Error { class, message: error.to_string(), retryable: false }, Projection::None, turn, 0).await?;
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
async fn input(cx: &Context, input: QueuedInput, turn: i64, step: i64) -> Result<()> {
    let mut parts = Vec::new();
    for part in input.input {
        parts.push(match part {
            InputPart::Text { text } => Part::text(crate::hooks::expand(&cx.root, &cx.runtime.0.config.user_config_dir, &text)?),
            InputPart::Image { mime, data_base64 } => Part::Image(Media { mime, source: MediaSource::Base64 { data: data_base64 }, name: None }),
            InputPart::File { path } => {
                Part::text(format!("Attached file {path}:\n{}", tokio::fs::read_to_string(std::path::Path::new(&cx.row.cwd).join(&path)).await?))
            }
        });
    }
    cx.item(Item::new(input.id, Role::User, parts), turn, step).await
}
async fn run_inner(cx: Arc<Context>, first: QueuedInput, turn: i64, cancel: &CancellationToken) -> Result<()> {
    input(&cx, first, turn, 0).await?;
    if cx
        .state(|s| {
            let first = !s.started;
            s.started = true;
            first
        })
        .await?
    {
        apply_hook(&cx, "SessionStart", "", serde_json::Value::Null, turn, 0).await?;
    }
    let family = serde_json::to_value(cx.model.info().caps.tool_family)?.as_str().unwrap_or("generic").to_owned();
    let limit = cx.row.meta["max_steps"].as_u64().filter(|n| *n > 0).unwrap_or(200) as i64;
    let mut continuations = 0;
    let mut overflow = false;
    let mut base_tools = otto_tools::registry::tools_for_family(&family);
    base_tools.extend(cx.runtime.extra_tools());
    'steps: for step in 1..=limit {
        let loaded = crate::tools::loaded(&cx).await?;
        let mut tools = base_tools.clone();
        tools.extend(crate::tools::tools(cx.clone(), &loaded));
        let mut defs = Vec::new();
        let mut lookup = std::collections::HashMap::new();
        for tool in tools {
            let spec = tool.spec();
            if !crate::tools::granted(&cx, spec) {
                continue;
            }
            let presentation = spec.presentations.iter().find(|p| p.family == family);
            let name = presentation.map_or_else(|| spec.name.clone(), |p| p.name.clone());
            if spec.loading != otto_tools::Loading::Deferred || loaded.contains(&spec.name) {
                defs.push(ToolDef {
                    name: name.clone(),
                    description: presentation.and_then(|p| p.description.clone()).unwrap_or_else(|| spec.description.clone()),
                    input_schema: presentation.and_then(|p| p.input_schema.clone()).unwrap_or_else(|| spec.input_schema.clone()),
                    kind: if let Some(format) = presentation.and_then(|p| p.freeform.clone()) {
                        ToolKind::Custom { format: Some(format) }
                    } else {
                        ToolKind::Function
                    },
                    defer_loading: false,
                });
            }
            lookup.insert(name, tool);
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        crate::subagents::deliver(&cx, turn, step).await?;
        let steer = cx.state(|s| std::mem::take(&mut s.steer)).await?;
        for queued in steer {
            input(&cx, queued, turn, step).await?;
        }
        let request = Request {
            model: cx.row.model.clone(),
            system: cx.system.clone(),
            items: crate::compaction::trim(cx.items().await?),
            tools: defs.clone(),
            parallel_tools: Some(cx.model.info().caps.parallel_tools),
            reasoning: serde_json::from_value::<Effort>(cx.row.meta["effort"].clone()).ok().map(Reasoning::Effort),
            ..Default::default()
        };
        let stream = tokio::select! {_=cancel.cancelled()=>return Ok(()),s=cx.model.stream(request,CallCx{cancel:cancel.clone(),session:Some(cx.row.id.clone())})=>s};
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) if e.class == ErrorClass::ContextOverflow && !overflow => {
                overflow = true;
                crate::compaction::compact(&cx, cancel, turn).await?;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let mut item = Item::new(crate::id(), Role::Assistant, vec![]);
        item.origin = Some(Origin { provider: cx.row.provider.as_str().into(), model: cx.row.model.clone(), response_id: None });
        cx.emit(Event::ItemStarted { item_id: item.id.clone(), role: Role::Assistant }, Projection::None, turn, step).await?;
        let mut usage = Usage::default();
        let mut reason = FinishReason::Stop;
        let mut deltas = crate::deltas::Deltas::new(&cx.row.id);
        let mut tick = tokio::time::interval(otto_store::Coalescer::MAX_DELAY);
        loop {
            let event = tokio::select! {
                _=cancel.cancelled()=>{deltas.flush(&cx).await?;return Ok(())},
                _=tick.tick()=>{deltas.flush(&cx).await?;continue},
                event=stream.next()=>event
            };
            let Some(event) = event else { break };
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    deltas.flush(&cx).await?;
                    if error.class == ErrorClass::ContextOverflow && !overflow {
                        overflow = true;
                        crate::compaction::compact(&cx, cancel, turn).await?;
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
                StreamEvent::ToolInputDelta { call_id, delta } => {
                    cx.emit(Event::ToolInputDelta { call_id, delta }, Projection::None, turn, step).await?;
                }
                StreamEvent::Usage(u) => usage = u,
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
        let cost = cx.model.info().pricing.as_ref().map(|p| {
            (usage.input as f64 * p.input
                + usage.output as f64 * p.output
                + usage.cache_read as f64 * p.cache_read
                + usage.cache_write as f64 * p.cache_write)
                / 1_000_000.0
        });
        cx.emit(
            Event::Usage { turn, step, usage: usage.clone(), cost_usd: cost },
            Projection::Usage(UsageRecord {
                provider: cx.row.provider.clone(),
                model: cx.row.model.clone(),
                usage: usage.clone(),
                cost_usd: cost.unwrap_or_default(),
            }),
            turn,
            step,
        )
        .await?;
        let calls = item.tool_calls().map(|(id, name, value)| (id.to_owned(), name.to_owned(), value.clone())).collect::<Vec<_>>();
        if calls.is_empty() {
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
            return Ok(());
        }
        let exclusive = calls.iter().any(|(_, name, _)| lookup.get(name).is_some_and(|t| t.spec().exclusive));
        let mut jobs = Vec::new();
        let result_id = crate::id();
        for (call_id, name, value) in calls {
            jobs.push(execute(cx.clone(), lookup.get(&name).cloned(), call_id, name, value, turn, step, cancel.clone(), result_id.clone()));
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
        let window = cx.model.info().caps.context_window;
        if window > 0 && usage.total_input() > window * 85 / 100 {
            crate::compaction::compact(&cx, cancel, turn).await?;
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
    Ok(outcome)
}
#[allow(clippy::too_many_arguments)]
async fn execute(
    cx: Arc<Context>,
    tool: Option<Arc<dyn otto_tools::Tool>>,
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
    let mut effective = original.clone();
    let mut prepared = None;
    let decision = if let Some(tool) = &tool {
        match crate::prepared::prepare(&cx, tool.clone(), original.clone(), tcx.clone()).await {
            Err(error) => Decision::Deny { reason: error.to_string() },
            Ok(initial) => match apply_hook(&cx, "PreToolUse", &name, crate::prepared::public(initial.input.clone()), turn, step).await {
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
                            let decision = hooks.deny.map_or_else(|| p.decision.clone(), |reason| Decision::Deny { reason });
                            prepared = Some(p);
                            decision
                        }
                    }
                }
            },
        }
    } else {
        Decision::Deny { reason: format!("unknown tool: {name}") }
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
            otto_tools::OutputPart::File { path } => {
                content.push(ResultPart::text(format!("Output file: {}", path.display())));
                limited.content.push(otto_tools::OutputPart::File { path });
            }
            _ => {}
        }
    }
    let serialized = serde_json::to_value(&limited)?;
    apply_hook(&cx, "PostToolUse", &name, serialized.clone(), turn, step).await?;
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
