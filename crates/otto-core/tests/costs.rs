//! API-equivalent costs: a step's list price live (`usage.api_cost_usd`), and every stored step priced again
//! when read (sessions recorded at $0 on a subscription, unknown models, model switches), and `usage.summary`.
use otto_core::*;
use otto_llm::*;
use otto_proto::{Delivery, Event, InputPart};
use otto_store::{EventRecord, NewSession, Projection, Store, UsageRecord};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Scripted {
    info: ModelInfo,
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
}
#[async_trait::async_trait]
impl Model for Scripted {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    async fn stream(&self, _: Request, _: CallCx) -> std::result::Result<EventStream, LlmError> {
        Ok(match self.scripts.lock().unwrap().pop_front() {
            Some(events) => Box::pin(futures::stream::iter(events.into_iter().map(Ok))),
            None => Box::pin(futures::stream::pending()),
        })
    }
}
struct Resolver(Arc<Scripted>);
impl ModelResolver for Resolver {
    fn resolve(&self, _: &str, _: &str) -> Result<Arc<dyn Model>> {
        Ok(self.0.clone())
    }
    fn default_model(&self) -> Option<(String, String)> {
        Some((self.0.info.provider.as_str().to_owned(), self.0.info.id.clone()))
    }
    fn list(&self) -> Vec<ModelInfo> {
        vec![self.0.info.clone()]
    }
}
fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-12)
}

#[tokio::test]
async fn subscription_steps_cost_nothing_but_show_their_api_price_live_and_when_read_back() {
    let entry = otto_llm::catalog::Catalog::global().models("anthropic", None).into_iter().find(|e| e.list_pricing.is_some()).unwrap();
    let price = entry.list_pricing.clone().unwrap();
    let id = entry.info.id.clone();
    // A subscription's model: billed nothing, its list price known.
    let info = ModelInfo { pricing: Some(Pricing::default()), list_pricing: Some(price.clone()), ..entry.info.clone() };
    let used = Usage { input: 1_000, output: 500, cache_read: 20_000, cache_write: 2_000, reasoning: 0 };
    let reply = vec![
        StreamEvent::Usage(used.clone()),
        StreamEvent::PartDone { part: Part::text("done") },
        StreamEvent::Finish { reason: FinishReason::Stop, response_id: None },
    ];
    let model = Arc::new(Scripted { info, scripts: Mutex::new(vec![reply].into()) });
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let runtime = Runtime::new(RuntimeConfig {
        store: store.clone(),
        models: Arc::new(Resolver(model)),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap().id;

    // A session stored before API-equivalent costs: $0 rows, the catalog model then an unknown one.
    let old = {
        let mut s = store.lock().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        let row = NewSession {
            project_id: project.clone(),
            parent_id: None,
            agent: "build".into(),
            model: id.clone(),
            provider: "anthropic".into(),
            title: "old".into(),
            cwd,
            meta: serde_json::json!({}),
        };
        let old = s.create_session(row).unwrap().id;
        let record = |provider: &str, model: &str| UsageRecord { provider: provider.into(), model: model.into(), usage: used.clone(), cost_usd: 0.0 };
        let at = |step| EventRecord { turn: 1, step, kind: "usage".into(), payload: serde_json::json!({}) };
        s.append(&old, at(1), Projection::Usage(record("anthropic", &id))).unwrap();
        s.append(&old, at(2), Projection::Usage(record("ollama", "llama"))).unwrap();
        old
    };
    let listed = runtime.list_sessions(&project, false).await.unwrap();
    let old_info = listed.iter().find(|s| s.id == old).unwrap();
    assert_eq!(old_info.usage.cost_usd, 0.0, "billed as stored");
    assert!(close(old_info.usage.api_cost_usd, price.cost(&used)), "priced retroactively; the unknown model adds nothing");
    let models: Vec<_> = old_info.usage.by_model.iter().map(|m| (m.model.as_str(), m.steps, m.api_cost_usd.is_some())).collect();
    assert_eq!(models, [(id.as_str(), 1, true), ("llama", 1, false)]);

    // A live step: billed $0, its API price on the event, and the same when read back.
    let s = runtime.create_session(CreateSession { project_id: project.clone(), cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&s.id, 0).await.unwrap();
    s.prompt(vec![InputPart::Text { text: "go".into() }], Delivery::Queue).await.unwrap();
    let mut step = None;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match live.recv().await.unwrap().1 {
                Event::Usage { cost_usd, api_cost_usd, provider, model, .. } => step = Some((cost_usd, api_cost_usd, provider, model)),
                Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let (cost, api, provider, model) = step.expect("a usage event");
    assert_eq!(cost, Some(0.0));
    assert!(close(api, price.cost(&used)));
    assert_eq!((provider.as_deref(), model.as_deref()), (Some("anthropic"), Some(id.as_str())));
    let view = runtime.view(&s.id).await.unwrap();
    assert!(close(view.info.usage.api_cost_usd, price.cost(&used)));
    assert_eq!(view.info.usage.by_model.len(), 1);
    let turn = &view.turn_usage[0];
    assert!(close(turn.api_cost_usd, price.cost(&used)));
    assert_eq!((turn.cost_usd, turn.by_model.len(), turn.by_model[0].steps), (0.0, 1, 1));

    // Across sessions: both, the unknown model listed without a price.
    let summary = runtime.usage_summary(0, 330).await.unwrap();
    assert_eq!(summary.sessions, 2);
    assert!(close(summary.totals.api_cost_usd, 2.0 * price.cost(&used)));
    assert_eq!(summary.totals.by_model[0].steps, 2);
    assert_eq!(summary.totals.by_model[1].api_cost_usd, None);
    assert_eq!(summary.days.len(), 1, "all today");
    assert!(runtime.usage_summary(i64::MAX, 0).await.unwrap().totals.by_model.is_empty());
    runtime.shutdown().await.unwrap();
}
