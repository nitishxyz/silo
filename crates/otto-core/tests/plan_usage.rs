//! Subscription plan usage: kept per provider, saved in the state folder,
//! announced live and shown on the session's info. Offline (scripted model).
use otto_core::*;
use otto_llm::*;
use otto_proto::{Delivery, Event, InputPart};
use otto_store::Store;
use std::{
    collections::VecDeque,
    path::Path,
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
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}
struct Resolver(Arc<Scripted>);
impl ModelResolver for Resolver {
    fn resolve(&self, _: &str, _: &str) -> Result<Arc<dyn Model>> {
        Ok(self.0.clone())
    }
    fn default_model(&self) -> Option<(String, String)> {
        Some(("anthropic".into(), "test".into()))
    }
    fn list(&self) -> Vec<ModelInfo> {
        vec![self.0.info.clone()]
    }
}
fn config(dir: &Path, scripts: Vec<Vec<StreamEvent>>) -> RuntimeConfig {
    let model = Arc::new(Scripted {
        info: ModelInfo { id: "test".into(), provider: "anthropic".into(), name: "test".into(), caps: Caps::default(), pricing: None },
        scripts: Mutex::new(scripts.into()),
    });
    RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open(dir.join("db")).unwrap())),
        models: Arc::new(Resolver(model)),
        state_dir: dir.join("state"),
        user_config_dir: dir.join("user"),
    }
}
fn window(name: &str, used: f32) -> PlanWindow {
    PlanWindow { name: name.into(), label: plan_usage::window_label(name), used, resets_at: Some(1_800_000_000) }
}
fn reply(plan: PlanUsage) -> Vec<StreamEvent> {
    vec![
        StreamEvent::PlanUsage(plan),
        StreamEvent::PartDone { part: Part::text("ok") },
        StreamEvent::Finish { reason: FinishReason::Stop, response_id: None },
    ]
}

#[tokio::test]
async fn plan_usage_is_kept_per_provider_announced_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first = PlanUsage { windows: vec![window("5h", 0.62), window("7d", 0.18)] };
    let second = PlanUsage { windows: vec![window("5h", 0.7)] };
    let runtime = Runtime::new(config(dir.path(), vec![reply(first), reply(second)])).await.unwrap();
    let mut global = runtime.events();
    assert!(runtime.plan_usage(None).is_empty(), "nothing known yet");
    let project = runtime.open_project(dir.path()).await.unwrap();
    let session = runtime
        .create_session(CreateSession {
            project_id: project.id.clone(),
            cwd: dir.path().into(),
            provider: Some("anthropic".into()),
            model: Some("test".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(runtime.view(&session.id).await.unwrap().info.plan, None);
    for text in ["one", "two"] {
        let seq = runtime.view(&session.id).await.unwrap().seq;
        let (_, mut live) = runtime.subscribe(&session.id, seq).await.unwrap();
        session.prompt(vec![InputPart::Text { text: text.into() }], Delivery::Queue).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async { while !matches!(live.recv().await.unwrap().1, Event::TurnFinished { .. }) {} })
            .await
            .unwrap();
    }
    // Each response's report goes out live, merged with what was known.
    let mut announced = Vec::new();
    while let Ok(event) = global.try_recv() {
        if let Event::PlanUsage { provider, usage } = event {
            announced.push((provider, usage));
        }
    }
    let merged = PlanUsage { windows: vec![window("5h", 0.7), window("7d", 0.18)] };
    assert_eq!(announced.len(), 2);
    assert_eq!(announced[1], ("anthropic".to_owned(), merged.clone()));
    // On the session's info and from `plan.usage`, for its provider only.
    assert_eq!(runtime.view(&session.id).await.unwrap().info.plan.as_ref(), Some(&merged));
    let all = runtime.plan_usage(None);
    assert_eq!(all, vec![otto_proto::ProviderPlan { provider: "anthropic".into(), usage: merged.clone() }]);
    assert!(runtime.plan_usage(Some("openai")).is_empty());
    let mut other = runtime.view(&session.id).await.unwrap().info;
    other.provider = "openai".into();
    assert_eq!(runtime.with_plan(other).plan, None, "another provider's session gets none");
    // Not journaled: replay has no plan events.
    let (replay, _) = runtime.subscribe(&session.id, 0).await.unwrap();
    assert!(!replay.iter().any(|(_, e)| matches!(e, Event::PlanUsage { .. })));
    runtime.shutdown().await.unwrap();
    drop(runtime);
    assert!(dir.path().join("state/plan_usage.json").exists());
    // A restart reads it back.
    let runtime = Runtime::new(config(dir.path(), vec![])).await.unwrap();
    assert_eq!(runtime.plan_usage(Some("anthropic"))[0].usage, merged);
    assert_eq!(runtime.view(&session.id).await.unwrap().info.plan, Some(merged));
    runtime.shutdown().await.unwrap();
}
