//! Child sessions and the durable outbox (docs/OTTO.md §7.4).
//!
//! A child's terminal turn is recorded as a versioned outbox entry for its
//! parent (one per child turn), then the parent is woken: its actor is opened
//! when it is not running. Entries are delivered at the parent's next step
//! boundary, or by a new turn when it is idle, and acknowledged in the same
//! transaction as the delivered item, so each delivers exactly once, also
//! across restarts (`Runtime::deliver_pending`).
//!
//! The user can take a child over from its own pane: their input marks it
//! (`meta.user`: `steered` / `took_over`) and its next report says so. A turn
//! the user stopped or interrupted does not report; it holds (`meta.held`)
//! for as long as the user keeps it: a later turn reports instead, the user
//! hands it back (`Runtime::hand_back`), or the next start reports it as
//! `stopped`.
//!
//! A child's pending question or approval is a notice for the parent (once per
//! item, version `-seq`); the parent answers questions (`subagent answer`),
//! approvals stay the user's.
use crate::{CreateSession, Error, Result, session::Context};
use otto_proto::{Delivery, Event, InputPart, SessionStatus};
use otto_store::Projection;
use serde_json::{Value, json};

/// Nesting below the top-level session.
pub(crate) const MAX_DEPTH: usize = 2;
/// Children of one parent that may run at the same time.
pub(crate) const MAX_RUNNING: usize = 4;
const RESULT_LIMIT: usize = 16 * 1024;

/// A delegated child, not a fork (forks also record their origin as parent).
pub(crate) fn subagent(row: &otto_store::Session) -> bool {
    row.parent_id.is_some() && row.meta.get("forked_at_seq").is_none_or(Value::is_null)
}

fn running(row: &otto_store::Session) -> bool {
    matches!(row.status.as_str(), "busy" | "waiting_approval")
}

fn summary(row: &otto_store::Session) -> Value {
    json!({"session_id":row.id,"agent":row.agent,"title":row.title,"task":row.meta["task"],"status":if row.archived {"archived"} else {row.status.as_str()}})
}

pub(crate) async fn call(cx: &Context, input: Value) -> Result<String> {
    let id = cx.row.id.clone();
    let children: Vec<_> = cx.runtime.db(move |s| Ok(s.children(&id)?)).await?.into_iter().filter(subagent).collect();
    let action = input["action"].as_str().unwrap_or("");
    let text = input["message"].as_str().or_else(|| input["task"].as_str()).map(str::to_owned);
    let target = input["session_id"].as_str();
    // The label the parent gave the child; the task's start stands in for none.
    let title = input["title"].as_str().map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|t| !t.is_empty());
    let title = |task: &str| title.clone().map_or_else(|| task.chars().take(80).collect::<String>(), |t| t.chars().take(60).collect());
    if let Some(target) = target
        && !children.iter().any(|c| c.id == target)
    {
        return Err(Error::Invalid(format!("{target} is not a child of this session")));
    }
    match action {
        "delegate" => {
            let task = text.ok_or_else(|| Error::Invalid("task required".into()))?;
            if children.iter().filter(|c| running(c)).count() >= MAX_RUNNING {
                return Err(Error::Invalid(format!("{MAX_RUNNING} subagents are already running; wait for one or stop it")));
            }
            let tools: Option<Vec<String>> =
                serde_json::from_value(input["tools"].clone()).map_err(|_| Error::Invalid("tools must be a list of tool names".into()))?;
            if let Some(child) = target {
                if tools.is_some() {
                    return Err(Error::Invalid("tools apply to a new child only; omit session_id".into()));
                }
                // reuse_session: continue an existing child with a new task.
                if input["title"].as_str().is_some_and(|t| !t.trim().is_empty()) {
                    cx.runtime.rename(child, title(&task)).await?;
                }
                cx.runtime.prompt_child(child, vec![InputPart::Text { text: task }], Delivery::Queue).await?;
                cx.emit(
                    Event::SubagentChanged { parent: cx.row.id.clone(), child: child.into(), status: SessionStatus::Busy },
                    Projection::None,
                    0,
                    0,
                )
                .await?;
                return Ok(json!({"session_id":child,"status":"started"}).to_string());
            }
            let id = cx.row.id.clone();
            let depth = cx
                .runtime
                .db(move |s| {
                    let mut depth = 0;
                    let mut row = s.session(&id)?;
                    while let Some(current) = row.filter(subagent) {
                        depth += 1;
                        row = s.session(current.parent_id.as_deref().unwrap_or_default())?;
                    }
                    Ok(depth)
                })
                .await?;
            if depth >= MAX_DEPTH {
                return Err(Error::Invalid("subagent depth limit reached; do the task yourself".into()));
            }
            let agents = crate::agents::discover(&cx.root, &cx.runtime.0.config.user_config_dir);
            let names = || agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
            let name =
                input["agent"].as_str().filter(|n| !n.is_empty()).ok_or_else(|| Error::Invalid(format!("agent required; available: {}", names())))?;
            let agent =
                agents.iter().find(|a| a.name == name).ok_or_else(|| Error::Invalid(format!("unknown agent {name}; available: {}", names())))?;
            // The agent's own model wins; otherwise the child inherits the parent's.
            let inherit = agent.model.is_none();
            let live = cx.live();
            let child = cx
                .runtime
                .create_session(CreateSession {
                    project_id: cx.row.project_id.clone(),
                    cwd: cx.row.cwd.clone().into(),
                    parent_id: Some(cx.row.id.clone()),
                    agent: name.into(),
                    provider: inherit.then(|| live.row.provider.clone()),
                    model: inherit.then(|| live.row.model.clone()),
                    title: title(&task),
                    task: Some(task.clone()),
                    tools,
                    ..Default::default()
                })
                .await?;
            child.submit(vec![InputPart::Text { text: task }], Delivery::Queue, false).await?;
            cx.emit(
                Event::SubagentChanged { parent: cx.row.id.clone(), child: child.id.clone(), status: SessionStatus::Busy },
                Projection::None,
                0,
                0,
            )
            .await?;
            Ok(json!({"session_id":child.id,"agent":name,"status":"started","note":"The result arrives as a system message when the child finishes; keep working meanwhile."}).to_string())
        }
        "list" => Ok(Value::Array(children.iter().map(summary).collect()).to_string()),
        _ => {
            let child = target.ok_or_else(|| Error::Invalid("session_id required".into()))?;
            let row = children.iter().find(|c| c.id == child).expect("checked above");
            match action {
                "status" => {
                    let view = cx.runtime.view(child).await?;
                    let mut status = summary(row);
                    status["status"] = serde_json::to_value(view.info.status)?;
                    status["queued"] = json!(view.queue.len());
                    status["pending_approvals"] = json!(view.pending_approvals.len());
                    status["pending_questions"] = json!(view.pending_questions.iter().map(|q| asked(&q.question, &q.options)).collect::<Vec<_>>());
                    status["last_message"] = json!(last_text(&view.items, 4000));
                    Ok(status.to_string())
                }
                "stop" => {
                    cx.runtime.cancel_child(child).await?;
                    Ok("stopped".into())
                }
                "message" => {
                    let delivery = match input["delivery"].as_str() {
                        Some("interrupt") => Delivery::Interrupt,
                        Some("steer") => Delivery::Steer,
                        _ => Delivery::Queue,
                    };
                    let text = text.ok_or_else(|| Error::Invalid("message required".into()))?;
                    cx.runtime.prompt_child(child, vec![InputPart::Text { text }], delivery).await?;
                    Ok("sent".into())
                }
                "answer" => {
                    let answer = input["answer"].as_str().filter(|a| !a.trim().is_empty()).ok_or_else(|| Error::Invalid("answer required".into()))?;
                    let view = cx.runtime.view(child).await?;
                    let pending: Vec<_> = view.pending_questions.iter().map(|q| q.question_id.clone()).collect();
                    if pending.is_empty() {
                        return Err(Error::Invalid(if view.pending_approvals.is_empty() {
                            format!("{child} has no pending question")
                        } else {
                            format!("{child} waits for an approval, not a question; only the user can approve it")
                        }));
                    }
                    // Oldest first: the order the child asked them in this turn.
                    let id = child.to_owned();
                    let asked = cx.runtime.db(move |s| Ok(s.events_after_last(&id, &["turn.started"])?)).await?;
                    let question = asked
                        .iter()
                        .filter(|e| e.kind == "question.asked")
                        .filter_map(|e| e.payload["question_id"].as_str())
                        .find(|q| pending.iter().any(|p| p == q))
                        .map_or_else(|| pending[0].clone(), str::to_owned);
                    cx.runtime.answer_question(child, &question, answer.into()).await?;
                    Ok("answered".into())
                }
                "retry" => {
                    let task = row.meta["task"].as_str().map(str::to_owned).or(text).ok_or_else(|| Error::Invalid("nothing to retry".into()))?;
                    cx.runtime.prompt_child(child, vec![InputPart::Text { text: format!("Retry the task: {task}") }], Delivery::Queue).await?;
                    Ok(json!({"session_id":child,"status":"started"}).to_string())
                }
                _ => Err(Error::Invalid("unknown subagent action".into())),
            }
        }
    }
}

fn last_text(items: &[otto_llm::Item], limit: usize) -> String {
    let text = items.iter().rev().find(|i| i.role == otto_llm::Role::Assistant && !i.text().is_empty()).map(otto_llm::Item::text).unwrap_or_default();
    crate::compaction::bounded(&text, limit)
}

pub(crate) fn payload(row: &otto_store::Session, items: &[otto_llm::Item], turn: i64, status: &str, error: Option<String>) -> Value {
    json!({"child":row.id,"agent":row.agent,"title":row.title,"turn":turn,"status":status,"result":last_text(items, RESULT_LIMIT),"error":error,"user":row.meta["user"]})
}

/// Change the child's meta in the store and in its live row (actor only). A
/// held report starting or ending (`SessionInfo.report_held`) is announced.
async fn set_meta(cx: &Context, f: impl FnOnce(&mut Value) + Send + 'static) -> Result<()> {
    let id = cx.row.id.clone();
    let (row, event) = cx
        .runtime
        .db(move |s| {
            let mut row = s.session(&id)?.ok_or(Error::Closed)?;
            if !row.meta.is_object() {
                row.meta = json!({});
            }
            let was = held(&row.meta);
            f(&mut row.meta);
            let update = otto_store::SessionUpdate { meta: Some(row.meta.clone()), ..Default::default() };
            if was == held(&row.meta) {
                s.update_session(&id, update)?;
                return Ok((row, None));
            }
            let event = held_changed(s, &row)?;
            let seq = s.append_with_session(&id, crate::runtime::record(&event, 0, 0)?, Projection::None, update)?;
            Ok((row, Some((seq, event))))
        })
        .await?;
    cx.set_row(row);
    if let Some(event) = event {
        let _ = cx.events.send(event);
        cx.runtime.changed();
    }
    Ok(())
}

/// The child holds the report of a turn the user stopped.
pub(crate) fn held(meta: &Value) -> bool {
    meta["held"].is_i64()
}

fn held_changed(s: &otto_store::Store, row: &otto_store::Session) -> Result<Event> {
    let mut info = crate::runtime::info(s, row.clone())?;
    info.last_seq += 1;
    Ok(Event::SessionChanged { info })
}

/// The user steered the child, or stopped / interrupted it (`took_over`).
pub(crate) async fn mark(cx: &Context, took_over: bool) {
    let current = cx.live().row.meta["user"].clone();
    let value = if took_over || current == "took_over" { "took_over" } else { "steered" };
    if current == value {
        return;
    }
    if let Err(error) = set_meta(cx, move |meta| meta["user"] = json!(value)).await {
        tracing::warn!(session=%cx.row.id, %error, "subagent takeover not recorded");
    }
}

/// After a child turn: a turn the user stopped holds its report; a reported
/// one clears the takeover marks.
pub(crate) async fn turn_ended(cx: &Context, turn: i64, cancelled: bool, user_stopped: bool) {
    let meta = cx.live().row.meta.clone();
    let result = if user_stopped {
        set_meta(cx, move |meta| meta["held"] = json!(turn)).await
    } else if !cancelled && (!meta["user"].is_null() || !meta["held"].is_null()) {
        set_meta(cx, clear).await
    } else {
        Ok(())
    };
    if let Err(error) = result {
        tracing::warn!(session=%cx.row.id, %error, "subagent hold not recorded");
    }
}

pub(crate) fn clear(meta: &mut Value) {
    if let Some(meta) = meta.as_object_mut() {
        meta.remove("user");
        meta.remove("held");
    }
}

/// The user hands the idle child back: its held report goes to the parent now.
pub(crate) async fn hand_back(cx: &Context) -> Result<()> {
    if !held(&cx.live().row.meta) {
        return Err(Error::Invalid("no report is held for this session".into()));
    }
    let id = cx.row.id.clone();
    let released = cx
        .runtime
        .db(move |s| {
            let Some(row) = release_held(s, &id, true)? else { return Ok(None) };
            let event = held_changed(s, &row)?;
            let seq = s.append_with_session(&id, crate::runtime::record(&event, 0, 0)?, Projection::None, Default::default())?;
            Ok(Some((row, (seq, event))))
        })
        .await?;
    let Some((row, event)) = released else { return Err(Error::Invalid("no report is held for this session".into())) };
    cx.set_row(row);
    let _ = cx.events.send(event);
    cx.runtime.changed();
    let (runtime, parent) = (cx.runtime.clone(), cx.row.parent_id.clone().unwrap_or_default());
    tokio::spawn(async move {
        if let Err(error) = runtime.wake(parent.clone()).await {
            tracing::warn!(session=%parent, %error, "outbox wake deferred to the next start");
        }
    });
    Ok(())
}

/// Push the held report of a stopped child and clear its marks (hand-back or
/// start); the updated row when there was one. The outbox keys it by turn,
/// so a push repeated after a crash is the same entry.
pub(crate) fn release_held(s: &mut otto_store::Store, id: &str, handed_back: bool) -> Result<Option<otto_store::Session>> {
    let Some(mut row) = s.session(id)? else { return Ok(None) };
    let Some(turn) = row.meta["held"].as_i64() else { return Ok(None) };
    let mut payload = payload(&row, &s.items(id)?, turn, "stopped", None);
    payload["handed_back"] = json!(handed_back);
    if let Some(partial) = partial(s, id, turn)? {
        payload["result"] = json!(partial);
    }
    s.outbox_push(row.parent_id.as_deref().unwrap_or_default(), id, turn, payload)?;
    clear(&mut row.meta);
    s.update_session(id, otto_store::SessionUpdate { meta: Some(row.meta.clone()), ..Default::default() })?;
    Ok(Some(row))
}

/// Text the stop cut off in turn `turn`: what streamed of a reply that never
/// completed (the stop drops the unfinished item, the journal kept its deltas).
fn partial(s: &otto_store::Store, id: &str, turn: i64) -> Result<Option<String>> {
    let events: Vec<_> = s.events_since(id, 0, usize::MAX)?.into_iter().filter(|e| e.turn == turn).collect();
    let done = |item: &str| events.iter().any(|e| e.kind == "item.done" && e.payload["item"]["id"] == item);
    let Some(item) = events.iter().rev().filter(|e| e.kind == "item.started").filter_map(|e| e.payload["item_id"].as_str()).next() else {
        return Ok(None);
    };
    if done(item) {
        return Ok(None);
    }
    let text: String =
        events.iter().filter(|e| e.kind == "text.delta" && e.payload["item_id"] == item).filter_map(|e| e.payload["text"].as_str()).collect();
    Ok((!text.trim().is_empty()).then(|| crate::compaction::bounded(&text, RESULT_LIMIT)))
}

/// Record a finished child turn for the parent, then wake the parent.
pub(crate) async fn completed(cx: &Context, turn: i64, status: &str, error: Option<String>) -> Result<()> {
    if !subagent(&cx.row) {
        return Ok(());
    }
    let parent = cx.row.parent_id.clone().unwrap_or_default();
    let payload = payload(&cx.live().row, &cx.items().await?, turn, status, error);
    let (child, to) = (cx.row.id.clone(), parent.clone());
    cx.runtime.db(move |s| Ok(s.outbox_push(&to, &child, turn, payload)?)).await?;
    let runtime = cx.runtime.clone();
    tokio::spawn(async move {
        if let Err(error) = runtime.wake(parent.clone()).await {
            tracing::warn!(session=%parent, %error, "outbox wake deferred to the next start");
        }
    });
    Ok(())
}

/// A child's question or approval (its event `seq`) waits: tell the parent
/// once. Versions are `-seq`, apart from the turns completions use.
pub(crate) async fn waiting(cx: &Context, seq: i64, kind: &str, item: &str, text: &str) {
    if !subagent(&cx.row) {
        return;
    }
    let title = cx.live().row.title.clone();
    let who = format!("Sub-agent \"{title}\" (child {})", cx.row.id);
    let notice = match kind {
        "question" => format!("{who} asks: {text} Answer with subagent answer {{session_id, answer}}."),
        _ => format!("{who} needs approval: {}. Only the user can approve it.", text.trim().trim_end_matches('.')),
    };
    let payload = json!({"notice":notice,"child":cx.row.id,"waiting":kind,"item":item,"title":title});
    let (parent, child) = (cx.row.parent_id.clone().unwrap_or_default(), cx.row.id.clone());
    let to = parent.clone();
    if let Err(error) = cx.runtime.db(move |s| Ok(s.outbox_push(&to, &child, -seq, payload)?)).await {
        tracing::warn!(session=%cx.row.id, %error, "subagent notice not recorded");
        return;
    }
    let runtime = cx.runtime.clone();
    tokio::spawn(async move {
        if let Err(error) = runtime.wake(parent.clone()).await {
            tracing::warn!(session=%parent, %error, "outbox wake deferred to the next start");
        }
    });
}

/// The question text with its options, as the parent's notice shows it.
pub(crate) fn asked(question: &str, options: &[String]) -> String {
    let question = question.trim();
    let end = if question.ends_with(['?', '.', '!']) { "" } else { "." };
    if options.is_empty() { format!("{question}{end}") } else { format!("{question} (options: {}).", options.join(", ")) }
}

/// Inject undelivered entries; each commits with its acknowledgement.
pub(crate) async fn deliver(cx: &Context, turn: i64, step: i64) -> Result<()> {
    let id = cx.row.id.clone();
    let entries = cx.runtime.db(move |s| Ok(s.outbox_pending(&id)?)).await?;
    for entry in entries {
        let p = &entry.payload;
        // A session's own notice (a background command's result): its text.
        if let Some(text) = p["notice"].as_str() {
            // The user answered the child's question or approval first: nothing to tell.
            if let (Some(kind), Some(waiting)) = (p["waiting"].as_str(), p["item"].as_str()) {
                let (child, key) = (entry.from_session.clone(), if kind == "question" { "question_id" } else { "approval_id" });
                let events = cx.runtime.db(move |s| Ok(s.events_after_last(&child, &["turn.started"])?)).await?;
                let ours = |e: &&otto_store::Event, end: &[&str]| e.payload[key] == waiting && end.iter().any(|k| e.kind.ends_with(k));
                let pending = events.iter().any(|e| ours(&e, &[".asked", ".requested"]))
                    && !events.iter().any(|e| e.kind == "turn.finished" || ours(&e, &[".resolved"]));
                if !pending {
                    let entry = entry.id.clone();
                    cx.runtime.db(move |s| Ok(s.outbox_mark_delivered(&entry)?)).await?;
                    continue;
                }
            }
            let item =
                otto_llm::Item::new(format!("outbox-{}", entry.id), otto_llm::Role::System, vec![otto_llm::Part::System { text: text.into() }]);
            match cx.item(item, turn, step).await {
                Ok(()) if p["waiting"].is_string() => {
                    let child = entry.from_session;
                    cx.emit(
                        Event::SubagentChanged { parent: cx.row.id.clone(), child, status: SessionStatus::WaitingApproval },
                        Projection::None,
                        turn,
                        step,
                    )
                    .await?;
                    continue;
                }
                Ok(()) | Err(Error::Store(otto_store::Error::InvalidTransition(_))) => continue,
                Err(error) => return Err(error),
            }
        }
        let mut text = format!(
            "Subagent completion: child {} ({}, \"{}\") finished turn {} with status {}.",
            entry.from_session,
            p["agent"].as_str().unwrap_or("agent"),
            p["title"].as_str().unwrap_or_default(),
            p["turn"],
            p["status"].as_str().unwrap_or("done")
        );
        match (p["status"].as_str(), p["user"].as_str()) {
            (Some("stopped"), _) if p["handed_back"] == true => text.push_str("\nThe user stopped this sub-agent and handed it back."),
            (Some("stopped"), _) => text.push_str("\nThe user stopped this sub-agent; the otto server restarted before they handed it back."),
            (_, Some("took_over")) => text.push_str("\nThe user took over this sub-agent; the result reflects their input."),
            (_, Some("steered")) => text.push_str("\nThe user steered this sub-agent; the result reflects their input."),
            _ => {}
        }
        if let Some(error) = p["error"].as_str() {
            text.push_str(&format!("\nError: {error}"));
        }
        if let Some(result) = p["result"].as_str().filter(|r| !r.is_empty()) {
            text.push_str(&format!("\nResult:\n{result}"));
        }
        let item = otto_llm::Item::new(format!("outbox-{}", entry.id), otto_llm::Role::System, vec![otto_llm::Part::System { text }]);
        match cx.item(item, turn, step).await {
            Ok(()) => {}
            Err(Error::Store(otto_store::Error::InvalidTransition(_))) => continue,
            Err(error) => return Err(error),
        }
        let status = if p["status"] == "done" || p["status"] == "stopped" { SessionStatus::Idle } else { SessionStatus::Error };
        cx.emit(Event::SubagentChanged { parent: cx.row.id.clone(), child: entry.from_session, status }, Projection::None, turn, step).await?;
    }
    Ok(())
}

/// Parent cancel stops its running children (and theirs, through their cancel).
pub(crate) async fn cancel_children(cx: &Context) {
    let id = cx.row.id.clone();
    let children = cx.runtime.db(move |s| Ok(s.children(&id)?)).await.unwrap_or_default();
    let runtime = cx.runtime.clone();
    tokio::spawn(async move {
        for child in children.into_iter().filter(subagent) {
            let open = runtime.0.actors.lock().await.get(&child.id).cloned();
            if let Some(handle) = open {
                let _ = handle.stop(false).await;
            }
        }
    });
}
