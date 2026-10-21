use crate::{Result, runtime::record};
use otto_llm::{Item, Part, ResultPart, Role};
use otto_proto::Event;
use otto_store::{Projection, SessionUpdate, Store, ToolStatus};

pub(crate) fn recover(store: &mut Store) -> Result<()> {
    store.recover_all()?;
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
