//! Test-only model, enabled explicitly by OTTO_SILO_TEST_SCRIPT (never by default).
use otto_llm::{CallCx, EventStream, LlmError, Model, ModelInfo, Request, StreamEvent};
use std::sync::Arc;

#[derive(serde::Deserialize)]
struct Script {
    steps: Vec<Vec<StreamEvent>>,
}
pub struct ScriptModel {
    info: ModelInfo,
    script: Script,
}
pub fn load() -> anyhow::Result<Option<Arc<ScriptModel>>> {
    let Some(path) = std::env::var_os("OTTO_SILO_TEST_SCRIPT") else {
        return Ok(None);
    };
    let script: Script = serde_json::from_slice(&std::fs::read(path)?)?;
    anyhow::ensure!(!script.steps.is_empty(), "test script must have steps");
    Ok(Some(Arc::new(ScriptModel {
        script,
        info: ModelInfo {
            id: "script".into(),
            provider: "test".into(),
            name: "Offline test script".into(),
            pricing: None,
            list_pricing: None,
            caps: otto_llm::Caps { context_window: 128_000, max_output: 4096, ..Default::default() },
        },
    })))
}
#[async_trait::async_trait]
impl Model for ScriptModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    async fn stream(&self, req: Request, cx: CallCx) -> Result<EventStream, LlmError> {
        if cx.cancel.is_cancelled() {
            return Err(LlmError::new(otto_llm::ErrorClass::Cancelled, "cancelled"));
        }
        let step = req.items.iter().filter(|item| item.role == otto_llm::Role::Assistant).count();
        let events = self.script.steps.get(step).cloned().unwrap_or_else(|| {
            vec![
                StreamEvent::PartDone { part: otto_llm::Part::text("script complete") },
                StreamEvent::Finish { reason: otto_llm::FinishReason::Stop, response_id: None },
            ]
        });
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
}
