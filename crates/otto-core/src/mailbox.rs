//! Text a tool sends its session after the call returned (a background
//! command's result): a durable outbox entry from the session to itself,
//! delivered like a subagent's (`subagents::deliver`).
use crate::Runtime;
use serde_json::json;

pub(crate) struct Mailbox {
    pub runtime: Runtime,
    pub session: String,
}

impl otto_tools::Mailbox for Mailbox {
    fn deliver(&self, text: String) {
        let (runtime, session) = (self.runtime.clone(), self.session.clone());
        tokio::spawn(async move {
            // Versions only need to be unique per sender.
            let version =
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos().min(i64::MAX as u128) as i64;
            let to = session.clone();
            if let Err(error) = runtime.db(move |s| Ok(s.outbox_push(&to, &to, version, json!({ "notice": text }))?)).await {
                tracing::warn!(%session, %error, "notice not recorded");
                return;
            }
            if let Err(error) = runtime.wake(session.clone()).await {
                tracing::warn!(%session, %error, "notice wake deferred to the next start");
            }
        });
    }
}

/// `id`, then its ancestors.
pub(crate) async fn lineage(runtime: &Runtime, id: &str) -> crate::Result<Vec<String>> {
    let id = id.to_owned();
    runtime
        .db(move |s| {
            let mut ids = vec![id.clone()];
            let mut next = s.session(&id)?.and_then(|r| r.parent_id);
            while let Some(parent) = next.filter(|p| !ids.contains(p)) {
                next = s.session(&parent)?.and_then(|r| r.parent_id);
                ids.push(parent);
            }
            Ok(ids)
        })
        .await
}
