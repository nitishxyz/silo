use crate::{
    CreateSession, Error, Result,
    session::{Command, Context},
};
use otto_proto::{Delivery, Event, InputPart, SessionStatus};
use otto_store::Projection;
use serde_json::Value;
pub(crate) async fn call(cx: &Context, input: Value) -> Result<String> {
    let children = cx
        .runtime
        .list_sessions(&cx.row.project_id, false)
        .await?
        .into_iter()
        .filter(|s| s.parent_id.as_deref() == Some(&cx.row.id))
        .collect::<Vec<_>>();
    let action = input["action"].as_str().unwrap_or("");
    if action == "delegate" {
        let id = cx.row.id.clone();
        let depth = cx
            .runtime
            .db(move |s| {
                let mut row = s.session(&id)?;
                let mut depth = 0;
                while let Some(parent) = row.and_then(|r| r.parent_id) {
                    depth += 1;
                    row = s.session(&parent)?;
                }
                Ok(depth)
            })
            .await?;
        if depth >= 2 {
            return Err(Error::Invalid("subagent depth limit reached".into()));
        }
        let mut busy = 0;
        for child in &children {
            if cx.runtime.view(&child.id).await?.info.status == SessionStatus::Busy {
                busy += 1;
            }
        }
        if busy >= 4 {
            return Err(Error::Invalid("subagent concurrency limit reached".into()));
        }
        let task = input["task"].as_str().ok_or_else(|| Error::Invalid("task required".into()))?;
        let child = cx
            .runtime
            .create_session(CreateSession {
                project_id: cx.row.project_id.clone(),
                cwd: cx.row.cwd.clone().into(),
                parent_id: Some(cx.row.id.clone()),
                agent: input["agent"].as_str().unwrap_or("general").into(),
                provider: Some(cx.row.provider.clone()),
                model: Some(cx.row.model.clone()),
                title: task.chars().take(80).collect(),
                ..Default::default()
            })
            .await?;
        child.prompt(vec![InputPart::Text { text: task.into() }], Delivery::Queue).await?;
        cx.emit(Event::SubagentChanged { parent: cx.row.id.clone(), child: child.id.clone(), status: SessionStatus::Busy }, Projection::None, 0, 0)
            .await?;
        return Ok(child.id);
    }
    if action == "list" {
        return Ok(serde_json::to_string(&children)?);
    }
    let id = input["session_id"].as_str().ok_or_else(|| Error::Invalid("session_id required".into()))?;
    if !children.iter().any(|c| c.id == id) {
        return Err(Error::Invalid("not a child of this session".into()));
    }
    match action {
        "status" => Ok(serde_json::to_string(&cx.runtime.view(id).await?)?),
        "stop" => {
            cx.runtime.cancel(id).await?;
            Ok("stopped".into())
        }
        "message" => {
            cx.runtime
                .prompt(
                    id,
                    vec![InputPart::Text { text: input["task"].as_str().unwrap_or_default().into() }],
                    if input["delivery"] == "interrupt" { Delivery::Interrupt } else { Delivery::Queue },
                )
                .await
        }
        _ => Err(Error::Invalid("unknown subagent action".into())),
    }
}
pub(crate) async fn completed(cx: &Context) -> Result<()> {
    let Some(parent) = cx.row.parent_id.clone() else { return Ok(()) };
    let summary = cx.items().await?.iter().rev().find(|i| i.role == otto_llm::Role::Assistant).map(otto_llm::Item::text).unwrap_or_default();
    let child = cx.row.id.clone();
    let Some(handle) = cx.runtime.0.actors.lock().await.get(&parent).cloned() else { return Ok(()) };
    let (tx, rx) = tokio::sync::oneshot::channel();
    let payload = serde_json::json!({"child":child,"result":summary});
    handle.tx.send(Command::Outbox(child, payload, tx)).await.map_err(|_| Error::Closed)?;
    rx.await.map_err(|_| Error::Closed)??;
    Ok(())
}
pub(crate) async fn deliver(cx: &Context, turn: i64, step: i64) -> Result<()> {
    let id = cx.row.id.clone();
    let entries = cx.runtime.db(move |s| Ok(s.outbox_pending(&id)?)).await?;
    for entry in entries {
        let item = otto_llm::Item::new(
            format!("outbox-{}", entry.id),
            otto_llm::Role::System,
            vec![otto_llm::Part::System { text: format!("Subagent completion: {}", entry.payload) }],
        );
        cx.item(item, turn, step).await?;
    }
    Ok(())
}
