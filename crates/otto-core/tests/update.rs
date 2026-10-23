//! `session.update` (model, agent, effort), effort on requests, automatic titles, recent projects.
use otto_core::*;
use otto_llm::*;
use otto_proto::{ApprovalDecision, Delivery, Event, InputPart};
use otto_store::Store;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Scripted {
    info: ModelInfo,
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<Request>>,
}
impl Scripted {
    fn new(id: &str, reasoning: bool, price: Option<f64>, scripts: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            info: ModelInfo {
                id: id.into(),
                provider: "test".into(),
                name: id.into(),
                caps: Caps { reasoning, ..Caps::default() },
                pricing: price.map(|p| Pricing { input: p, output: p, ..Default::default() }),
            },
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(vec![]),
        })
    }
    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}
#[async_trait::async_trait]
impl Model for Scripted {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    async fn stream(&self, request: Request, _: CallCx) -> std::result::Result<EventStream, LlmError> {
        self.requests.lock().unwrap().push(request);
        Ok(match self.scripts.lock().unwrap().pop_front() {
            Some(events) => Box::pin(futures::stream::iter(events.into_iter().map(Ok))),
            None => Box::pin(futures::stream::pending()),
        })
    }
}
/// `test/<id>` resolves listed models (unknown ids fall back to the first, as
/// API-key catalogs do); provider `other` is not signed in.
struct Models {
    models: Vec<Arc<Scripted>>,
    resolved: AtomicUsize,
}
impl ModelResolver for Models {
    fn resolve(&self, provider: &str, model: &str) -> Result<Arc<dyn Model>> {
        if provider != "test" {
            return Err(LlmError::new(ErrorClass::Auth, format!("run `otto-silo auth login {provider}`")).into());
        }
        self.resolved.fetch_add(1, Ordering::SeqCst);
        Ok(self.models.iter().find(|m| m.info.id == model).unwrap_or(&self.models[0]).clone())
    }
    fn default_model(&self) -> Option<(String, String)> {
        Some(("test".into(), self.models[0].info.id.clone()))
    }
    fn list(&self) -> Vec<ModelInfo> {
        self.models.iter().map(|m| m.info.clone()).collect()
    }
}
fn text(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::PartDone { part: Part::text(text) }, StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }]
}
fn call(id: &str, name: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::PartDone { part: Part::ToolCall { call_id: id.into(), name: name.into(), input, kind: CallKind::Function, opaque: None } },
        StreamEvent::Finish { reason: FinishReason::ToolUse, response_id: None },
    ]
}
struct Fixture {
    dir: tempfile::TempDir,
    runtime: Runtime,
    models: Arc<Models>,
    project: String,
}
async fn fixture(models: Vec<Arc<Scripted>>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let models = Arc::new(Models { models, resolved: AtomicUsize::new(0) });
    let runtime = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        models: models.clone(),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap().id;
    Fixture { dir, runtime, models, project }
}
impl Fixture {
    async fn session(&self, options: CreateSession) -> SessionHandle {
        self.runtime.create_session(CreateSession { project_id: self.project.clone(), cwd: self.dir.path().into(), ..options }).await.unwrap()
    }
}
async fn turn(r: &Runtime, s: &SessionHandle) {
    let (_, mut live) = r.subscribe(&s.id, s.view().await.unwrap().seq).await.unwrap();
    s.prompt(vec![InputPart::Text { text: "fix the login bug".into() }], Delivery::Queue).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async { while !matches!(live.recv().await.unwrap().1, Event::TurnFinished { .. }) {} })
        .await
        .unwrap();
}
async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn update_changes_model_and_effort_from_the_next_request() {
    let a = Scripted::new("a", true, None, vec![text("one")]);
    let b = Scripted::new("b", true, None, vec![text("two"), text("three")]);
    let plain = Scripted::new("plain", false, None, vec![text("four")]);
    let f = fixture(vec![a.clone(), b.clone(), plain.clone()]).await;
    let s = f.session(CreateSession { effort: Some(Effort::High), ..Default::default() }).await;
    assert_eq!(s.view().await.unwrap().info.effort, Some(Effort::High));
    turn(&f.runtime, &s).await;
    assert_eq!(a.requests()[0].reasoning, Some(Reasoning::Effort(Effort::High)));
    let resolved = f.models.resolved.load(Ordering::SeqCst);
    let info = f.runtime.configure(&s.id, Configure { model: Some("b".into()), effort: Some(Effort::Low), ..Default::default() }).await.unwrap();
    assert_eq!((info.provider.as_str(), info.model.as_str(), info.effort), ("test", "b", Some(Effort::Low)));
    // A new model is a new instance: the old one's provider-side state is gone.
    assert_eq!(f.models.resolved.load(Ordering::SeqCst), resolved + 1);
    assert_eq!(s.view().await.unwrap().info.model, "b");
    turn(&f.runtime, &s).await;
    assert_eq!(a.requests().len(), 1);
    let request = &b.requests()[0];
    assert_eq!((request.model.as_str(), request.reasoning), ("b", Some(Reasoning::Effort(Effort::Low))));
    assert_eq!(request.items.iter().filter(|i| i.role == Role::User).count(), 2, "the new model gets the whole log");
    // Effort alone keeps the model instance.
    f.runtime.configure(&s.id, Configure { effort: Some(Effort::Max), ..Default::default() }).await.unwrap();
    assert_eq!(f.models.resolved.load(Ordering::SeqCst), resolved + 1);
    turn(&f.runtime, &s).await;
    assert_eq!(b.requests()[1].reasoning, Some(Reasoning::Effort(Effort::Max)));
    // Models without reasoning get no effort.
    f.runtime.configure(&s.id, Configure { model: Some("plain".into()), ..Default::default() }).await.unwrap();
    turn(&f.runtime, &s).await;
    assert_eq!(plain.requests()[0].reasoning, None);
    let listed = f.runtime.list_sessions(&f.project, false).await.unwrap();
    assert_eq!((listed[0].model.as_str(), listed[0].effort), ("plain", Some(Effort::Max)));
    // Reset: back to the agent's effort (build has none), the model's default.
    let info = f.runtime.configure(&s.id, Configure { reset_effort: true, ..Default::default() }).await.unwrap();
    assert_eq!(info.effort, None);
    f.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn running_turn_picks_up_the_update_at_its_next_step() {
    let a = Scripted::new("a", false, None, vec![call("w", "write", serde_json::json!({"path":"file","content":"ok"}))]);
    let b = Scripted::new("b", false, None, vec![text("done")]);
    let f = fixture(vec![a.clone(), b.clone()]).await;
    let s = f.session(CreateSession { approval_mode: Some(otto_proto::ApprovalMode::AskAll), ..Default::default() }).await;
    let (_, mut live) = f.runtime.subscribe(&s.id, 0).await.unwrap();
    s.prompt(vec![InputPart::Text { text: "write it".into() }], Delivery::Queue).await.unwrap();
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Event::ApprovalRequested { approval_id, .. } = live.recv().await.unwrap().1 {
                break approval_id;
            }
        }
    })
    .await
    .unwrap();
    f.runtime.configure(&s.id, Configure { model: Some("b".into()), ..Default::default() }).await.unwrap();
    f.runtime.answer_approval(&s.id, &approval, ApprovalDecision::Allow).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async { while !matches!(live.recv().await.unwrap().1, Event::TurnFinished { .. }) {} })
        .await
        .unwrap();
    assert_eq!(a.requests().len(), 1);
    assert_eq!(b.requests().len(), 1);
    let items = s.view().await.unwrap().items;
    assert_eq!(items.last().unwrap().origin.as_ref().unwrap().model, "b");
    assert_eq!(items[1].origin.as_ref().unwrap().model, "a");
    f.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn update_rejects_unknown_agent_model_and_signed_out_provider() {
    let f = fixture(vec![Scripted::new("a", false, None, vec![])]).await;
    let s = f.session(CreateSession::default()).await;
    for change in [
        Configure { agent: Some("nope".into()), ..Default::default() },
        Configure { model: Some("missing".into()), ..Default::default() },
        Configure { provider: Some("other".into()), model: Some("x".into()), ..Default::default() },
    ] {
        let error = f.runtime.configure(&s.id, change.clone()).await.unwrap_err();
        assert!(matches!(error, Error::Invalid(_)), "{change:?}: {error:?}");
    }
    let info = s.view().await.unwrap().info;
    assert_eq!((info.agent.as_str(), info.model.as_str()), ("build", "a"));
    f.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_agent_brings_its_model_effort_and_mode_at_start_and_on_change() {
    let a = Scripted::new("a", true, None, vec![text("one")]);
    let b = Scripted::new("b", true, None, vec![text("two"), text("three")]);
    let f = fixture(vec![a.clone(), b.clone()]).await;
    let agents = f.dir.path().join(".otto/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("deep.md"), "---\nmodel: test/b\neffort: high\napproval_mode: ask_all\ntools: read\n---\nThink.").unwrap();
    std::fs::write(agents.join("bare.md"), "---\neffort: low\n---\nx").unwrap();
    let s = f.session(CreateSession { agent: "deep".into(), ..Default::default() }).await;
    let info = s.view().await.unwrap().info;
    assert_eq!((info.model.as_str(), info.effort, info.approval_mode), ("b", Some(Effort::High), otto_proto::ApprovalMode::AskAll));
    turn(&f.runtime, &s).await;
    assert_eq!(b.requests()[0].reasoning, Some(Reasoning::Effort(Effort::High)));
    assert_eq!(b.requests()[0].tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["read"]);
    // An agent without a model keeps the session's.
    let info = f.runtime.configure(&s.id, Configure { agent: Some("bare".into()), ..Default::default() }).await.unwrap();
    assert_eq!((info.model.as_str(), info.effort), ("b", Some(Effort::Low)));
    let plain = f.session(CreateSession::default()).await;
    let info = f.runtime.configure(&plain.id, Configure { agent: Some("deep".into()), ..Default::default() }).await.unwrap();
    assert_eq!((info.agent.as_str(), info.model.as_str(), info.effort), ("deep", "b", Some(Effort::High)));
    assert_eq!(info.approval_mode, otto_proto::ApprovalMode::AskAll);
    turn(&f.runtime, &plain).await;
    assert_eq!(b.requests().len(), 2);
    // A model given with the agent wins over the agent's.
    let info =
        f.runtime.configure(&plain.id, Configure { agent: Some("build".into()), model: Some("a".into()), ..Default::default() }).await.unwrap();
    assert_eq!(info.model, "a");
    f.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn first_turn_names_the_session_with_the_cheapest_model_once() {
    let a = Scripted::new("a", false, None, vec![text("Looking into it."), text("again")]);
    let small = Scripted::new("small", false, Some(0.1), vec![text("\"Fix the login bug.\"\n")]);
    let f = fixture(vec![a.clone(), small.clone()]).await;
    f.runtime.set_auto_title(true);
    let s = f.session(CreateSession::default()).await;
    let mut index = f.runtime.index_events();
    turn(&f.runtime, &s).await;
    eventually("the title", async || s.view().await.unwrap().info.title == "Fix the login bug").await;
    let request = &small.requests()[0];
    assert!(request.system[0].contains("title"));
    assert!(request.items[0].text().contains("fix the login bug") && request.items[0].text().contains("Looking into it."));
    assert!(matches!(index.recv().await.unwrap().1, Event::SessionsChanged {}));
    turn(&f.runtime, &s).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(small.requests().len(), 1);
    f.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn automatic_titles_never_overwrite_and_failures_stay_empty() {
    let a = Scripted::new("a", false, None, vec![text("ok"), text("ok")]);
    let f = fixture(vec![a.clone()]).await;
    f.runtime.set_auto_title(true);
    let named = f.session(CreateSession { title: "Mine".into(), ..Default::default() }).await;
    turn(&f.runtime, &named).await;
    let renamed = f.session(CreateSession::default()).await;
    f.runtime.rename(&renamed.id, "Renamed".into()).await.unwrap();
    turn(&f.runtime, &renamed).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(a.requests().len(), 2, "titled sessions make no title call");
    assert_eq!(named.view().await.unwrap().info.title, "Mine");
    assert_eq!(renamed.view().await.unwrap().info.title, "Renamed");
    // A title call that returns nothing leaves the title empty, without retrying.
    let b =
        Scripted::new("b", false, None, vec![text("ok"), vec![StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }], text("ok")]);
    let f = fixture(vec![b.clone()]).await;
    f.runtime.set_auto_title(true);
    let s = f.session(CreateSession::default()).await;
    turn(&f.runtime, &s).await;
    eventually("the title call", async || b.requests().len() == 2).await;
    turn(&f.runtime, &s).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(b.requests().len(), 3);
    assert_eq!(s.view().await.unwrap().info.title, "");
    f.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_use_is_the_last_step_survives_reload_and_follows_the_model() {
    let windowed = |id: &str, window: u64, scripts: Vec<Vec<StreamEvent>>| {
        Arc::new(Scripted {
            info: ModelInfo {
                id: id.into(),
                provider: "test".into(),
                name: id.into(),
                caps: Caps { context_window: window, ..Caps::default() },
                pricing: None,
            },
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(vec![]),
        })
    };
    let usage = |input| StreamEvent::Usage(Usage { input, output: 5, cache_read: 20, cache_write: 3, reasoning: 0 });
    let step = |mut events: Vec<StreamEvent>, input| {
        events.insert(events.len() - 1, usage(input));
        events
    };
    let a = windowed(
        "a",
        1000,
        vec![step(call("l", "ls", serde_json::json!({"path":"."})), 100), step(text("done"), 200), text("no usage"), text("summary")],
    );
    let b = windowed("b", 5000, vec![]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("otto.db");
    let models = Arc::new(Models { models: vec![a.clone(), b.clone()], resolved: AtomicUsize::new(0) });
    let open = || RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open(&path).unwrap())),
        models: models.clone(),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    };
    let runtime = Runtime::new(open()).await.unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap().id;
    let s = runtime.create_session(CreateSession { project_id: project, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    assert_eq!(s.view().await.unwrap().info.context, otto_proto::ContextUse { used: 0, window: 1000 });
    turn(&runtime, &s).await;
    let (replay, _) = runtime.subscribe(&s.id, 0).await.unwrap();
    let reported: Vec<_> = replay.iter().filter_map(|(_, e)| if let Event::Usage { context, .. } = e { Some(context.used) } else { None }).collect();
    assert_eq!(reported, [123, 223], "each step reports its own prompt, not the sum");
    let id = s.id.clone();
    runtime.shutdown().await.unwrap();
    let runtime = Runtime::new(open()).await.unwrap();
    let info = runtime.view(&id).await.unwrap().info;
    assert_eq!(info.context, otto_proto::ContextUse { used: 223, window: 1000 });
    assert_eq!(info.usage.usage.input, 300);
    // A step whose provider reports no usage keeps the last known size.
    turn(&runtime, &runtime.session(&id).await.unwrap()).await;
    assert_eq!(runtime.view(&id).await.unwrap().info.context, otto_proto::ContextUse { used: 223, window: 1000 });
    let info = runtime.configure(&id, Configure { model: Some("b".into()), ..Default::default() }).await.unwrap();
    assert_eq!(info.context, otto_proto::ContextUse { used: 223, window: 5000 });
    runtime.configure(&id, Configure { model: Some("a".into()), ..Default::default() }).await.unwrap();
    runtime.compact(&id).await.unwrap();
    assert_eq!(runtime.view(&id).await.unwrap().info.context, otto_proto::ContextUse { used: 0, window: 1000 });
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn recent_projects_order_by_use_and_skip_missing_roots() {
    let f = fixture(vec![Scripted::new("a", false, None, vec![])]).await;
    let (old, gone) = (f.dir.path().join("old"), f.dir.path().join("gone"));
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&gone).unwrap();
    let old = f.runtime.open_project(&old).await.unwrap();
    f.runtime.open_project(&gone).await.unwrap();
    std::fs::remove_dir(&gone).unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    f.session(CreateSession::default()).await;
    let projects = f.runtime.recent_projects().await.unwrap();
    assert_eq!(projects.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), [f.project.as_str(), old.id.as_str()]);
    assert!(projects[0].updated_at > projects[1].updated_at && projects[1].updated_at == old.created_at);
    assert_eq!(f.runtime.list_projects().await.unwrap().len(), 3);
    f.runtime.shutdown().await.unwrap();
}
