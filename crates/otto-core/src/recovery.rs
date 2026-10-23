use crate::{Result, runtime::record};
use otto_llm::{Item, Part, ResultPart, Role};
use otto_proto::Event;
use otto_store::{Projection, SessionUpdate, Store, ToolStatus};

pub(crate) fn recover(store: &mut Store) -> Result<()> {
    let recovered = store.recover_all()?;
    // A child stopped by the restart still reports to its parent (§7.4); the
    // daemon delivers it through `Runtime::deliver_pending`.
    for (id, turn) in &recovered.interrupted_turns {
        if let Some(row) = store.session(id)?.filter(crate::subagents::subagent) {
            let payload =
                crate::subagents::payload(&row, &store.items(id)?, *turn, "interrupted", Some("the otto server stopped during this turn".into()));
            store.outbox_push(row.parent_id.as_deref().unwrap_or_default(), id, *turn, payload)?;
        }
    }
    // A turn the user stopped held its report; the hold ends with the daemon
    // (an interrupted later turn already reported instead), so nothing is lost.
    for session in store.all_sessions()?.into_iter().filter(|s| crate::subagents::subagent(s) && !s.meta["held"].is_null()) {
        if recovered.interrupted_turns.iter().any(|(id, _)| *id == session.id) {
            let mut meta = session.meta.clone();
            crate::subagents::clear(&mut meta);
            store.update_session(&session.id, SessionUpdate { meta: Some(meta), ..Default::default() })?;
        } else {
            crate::subagents::release_held(store, &session.id, false)?;
        }
    }
    for session in store.all_sessions()? {
        let items = store.items(&session.id)?;
        let calls = store.tool_calls(&session.id)?;
        let results: std::collections::HashSet<_> = items
            .iter()
            .flat_map(|i| &i.parts)
            .filter_map(|p| match p {
                Part::ToolResult { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        let mut missing = std::collections::BTreeMap::new();
        for call in &calls {
            if results.contains(&call.call_id) {
                continue;
            }
            let part = if matches!(call.status, ToolStatus::Done | ToolStatus::Error) {
                let output = call.output.clone().unwrap_or_default();
                let decoded = serde_json::from_value::<otto_tools::ToolOutput>(output.clone()).ok();
                let content = decoded.map_or_else(
                    || vec![ResultPart::text(output.to_string())],
                    |output| {
                        output
                            .content
                            .into_iter()
                            .map(|part| match part {
                                otto_tools::OutputPart::Text { text } => ResultPart::text(text),
                                otto_tools::OutputPart::Json { value } => ResultPart::text(value.to_string()),
                                otto_tools::OutputPart::Image { mime, data_base64 } => ResultPart::Image(otto_llm::Media {
                                    mime,
                                    source: otto_llm::MediaSource::Base64 { data: data_base64 },
                                    name: None,
                                }),
                                otto_tools::OutputPart::Document { mime, data_base64, name } => {
                                    ResultPart::Document(otto_llm::Media { mime, source: otto_llm::MediaSource::Base64 { data: data_base64 }, name })
                                }
                                otto_tools::OutputPart::File { path } => ResultPart::text(format!("Output file: {}", path.display())),
                            })
                            .collect()
                    },
                );
                Part::ToolResult { call_id: call.call_id.clone(), content, is_error: call.status == ToolStatus::Error }
            } else {
                unknown(&call.call_id, "outcome unknown: the process stopped while this tool ran; it was not re-run")
            };
            missing.insert(call.call_id.clone(), part);
        }
        for item in &items {
            for (id, _, _) in item.tool_calls() {
                if !results.contains(id) && !calls.iter().any(|c| c.call_id == id) {
                    missing.insert(
                        id.into(),
                        unknown(id, "outcome unknown: not started; the process stopped before tool intent was journaled; it was not re-run"),
                    );
                }
            }
        }
        if !missing.is_empty() {
            let item = Item::new(crate::id(), Role::Tool, missing.into_values().collect());
            store.append(&session.id, record(&Event::ItemDone { item: item.clone() }, 0, 0)?, Projection::PutItem(item))?;
        }
        // Shell jobs ran in the stopped process: those that never reported
        // say so now, without starting a turn.
        let reported: Vec<String> = items
            .iter()
            .filter(|i| i.role == Role::System)
            .flat_map(|i| &i.parts)
            .filter_map(|p| match p {
                Part::System { text } => Some(text.clone()),
                _ => None,
            })
            .chain(store.outbox_pending(&session.id)?.into_iter().filter_map(|e| e.payload["notice"].as_str().map(str::to_owned)))
            .collect();
        for call in calls.iter().filter(|c| c.name == "shell" && c.status == ToolStatus::Done) {
            let Some(job) = call.output.as_ref().and_then(|o| o["metadata"]["job_id"].as_str()) else { continue };
            let head = format!("Background shell {job} ");
            if reported.iter().any(|t| t.starts_with(&head)) {
                continue;
            }
            let command: String = call.input["command"].as_str().unwrap_or_default().chars().take(120).collect();
            let text = format!("{head}(`{command}`) was stopped (the otto server restarted).\n");
            let item = Item::new(crate::id(), Role::System, vec![Part::System { text }]);
            store.append(&session.id, record(&Event::ItemDone { item: item.clone() }, 0, 0)?, Projection::PutItem(item))?;
        }
        if matches!(session.status.as_str(), "busy" | "waiting_approval") {
            store.append_with_session(
                &session.id,
                record(&Event::TurnFinished { turn: 0, status: otto_proto::TurnStatus::Interrupted, usage: Default::default() }, 0, 0)?,
                Projection::None,
                SessionUpdate { status: Some("idle".into()), ..Default::default() },
            )?;
        }
    }
    Ok(())
}
fn unknown(id: &str, text: &str) -> Part {
    Part::ToolResult { call_id: id.into(), content: vec![ResultPart::text(text)], is_error: true }
}
