//! What outlives a restart: shell jobs in terminal blocks still alive, undo history, and a runtime reopened after
//! a shutdown. Offline (scripted model, fake terminal host).
use otto_core::*;
use otto_llm::*;
use otto_store::Store;
use otto_tools::builtin::shell::{Finished, Terminal, Terminals};
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
    async fn stream(&self, _: Request, _cx: CallCx) -> std::result::Result<EventStream, LlmError> {
        let script = self.scripts.lock().unwrap().pop_front();
        Ok(match script {
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
        Some(("test".into(), "test".into()))
    }
    fn list(&self) -> Vec<ModelInfo> {
        vec![self.0.info.clone()]
    }
}
fn model(scripts: Vec<Vec<StreamEvent>>) -> Arc<Scripted> {
    let info =
        ModelInfo { id: "test".into(), provider: "test".into(), name: "test".into(), caps: Caps::default(), pricing: None, list_pricing: None };
    Arc::new(Scripted { info, scripts: Mutex::new(scripts.into()) })
}
fn text(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::PartDone { part: Part::text(text) }, StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }]
}
fn call(id: &str, name: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    let part = Part::ToolCall { call_id: id.into(), name: name.into(), input, kind: CallKind::Function, opaque: None };
    vec![StreamEvent::PartDone { part }, StreamEvent::Finish { reason: FinishReason::ToolUse, response_id: None }]
}
fn config(dir: &Path, store: Arc<Mutex<Store>>, model: Arc<Scripted>) -> RuntimeConfig {
    RuntimeConfig { store, models: Arc::new(Resolver(model)), state_dir: dir.join("state"), user_config_dir: dir.join("user") }
}
/// Waits until a system item of the session satisfies `found`.
async fn system(runtime: &Runtime, id: &str, found: impl Fn(&str) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = runtime.view(id).await.unwrap();
            if view.items.iter().flat_map(|i| &i.parts).any(|p| matches!(p, Part::System { text } if found(text))) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn finished(live: &mut LiveEvents) {
    tokio::time::timeout(Duration::from_secs(10), async { while !matches!(live.recv().await.unwrap().1, otto_proto::Event::TurnFinished { .. }) {} })
        .await
        .unwrap();
}
fn texts(view: &otto_proto::SessionView) -> String {
    serde_json::to_string(&view.items).unwrap()
}

/// A terminal host whose block `b1` outlived the server; its command then exits.
struct Host;
struct Block;
#[async_trait::async_trait]
impl Terminals for Host {
    async fn start(
        &self,
        _: &str,
        _: &Path,
        _: &[(&str, &str)],
        _: &otto_tools::ToolCx,
    ) -> std::result::Result<Option<Arc<dyn Terminal>>, otto_tools::ToolError> {
        Ok(None)
    }
    async fn attach(&self, block: &str, host: &str, cx: &otto_tools::ToolCx) -> Option<Arc<dyn Terminal>> {
        assert_eq!(cx.call_id, if block == "b1" { "a" } else { "b" }, "its call");
        (block == "b1" && host == "fake").then(|| Arc::new(Block) as Arc<dyn Terminal>)
    }
}
#[async_trait::async_trait]
impl Terminal for Block {
    fn id(&self) -> &str {
        "b1"
    }
    async fn wait(&self, _: bool, _: &mut (dyn for<'s> FnMut(&'s str) + Send)) -> std::result::Result<Finished, otto_tools::ToolError> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(Finished::Exited { code: Some(0), text: "built".into() })
    }
    async fn stop(&self) -> std::result::Result<String, otto_tools::ToolError> {
        Ok(String::new())
    }
}

/// A store holding session `s` whose calls `a`, `b`, `c` started shell jobs that never reported: `a` in block `b1`
/// (alive), `b` in block `b2` (gone), `c` through pipes.
fn store_with_jobs(dir: &Path) -> (Arc<Mutex<Store>>, String) {
    let mut store = Store::open(dir.join("db")).unwrap();
    let project = store.create_project(dir.to_str().unwrap()).unwrap();
    let new = otto_store::NewSession {
        project_id: project.id,
        parent_id: None,
        agent: "build".into(),
        model: "test".into(),
        provider: "test".into(),
        title: String::new(),
        cwd: dir.to_string_lossy().into(),
        meta: serde_json::Value::Null,
    };
    let session = store.create_session(new).unwrap().id;
    let record = || otto_store::EventRecord { turn: 1, step: 1, kind: "tool.call".into(), payload: serde_json::json!({}) };
    for (call, job, metadata) in [
        ("a", "job-1", serde_json::json!({"block_id": "b1", "host": "fake", "background": true})),
        ("b", "job-2", serde_json::json!({"block_id": "b2", "host": "fake", "status": "waiting_for_input"})),
        ("c", "job-3", serde_json::json!({"background": true})),
    ] {
        let input = serde_json::json!({"command": format!("cargo {call}")});
        store.append(&session, record(), otto_store::Projection::ToolIntent { call_id: call.into(), name: "shell".into(), input }).unwrap();
        let mut metadata = metadata;
        metadata["job_id"] = serde_json::json!(job);
        let output = serde_json::to_value(otto_tools::ToolOutput::text("Started").with_metadata(metadata)).unwrap();
        let outcome = otto_store::Projection::ToolOutcome { call_id: call.into(), status: otto_store::ToolStatus::Done, output };
        store.append(&session, record(), outcome).unwrap();
    }
    (Arc::new(Mutex::new(store)), session)
}

#[tokio::test]
async fn a_job_whose_block_lives_on_reports_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (store, id) = store_with_jobs(dir.path());
    let runtime = Runtime::new(config(dir.path(), store, model(vec![text("noted")]))).await.unwrap();
    let stopped = |job: &str| format!("Background shell {job} (`cargo ");
    let view = runtime.view(&id).await.unwrap();
    assert!(texts(&view).contains(&stopped("job-3")), "a pipe job died with the server");
    assert!(!texts(&view).contains(&stopped("job-1")) && !texts(&view).contains(&stopped("job-2")), "blocks are checked first");
    runtime.set_terminals(Arc::new(Host));
    runtime.deliver_pending().await.unwrap();
    let view = runtime.view(&id).await.unwrap();
    assert!(texts(&view).contains("Background shell job-2 (`cargo b`) was stopped (the otto server restarted)"), "its block is gone");
    let jobs = runtime.shell_jobs(&id).await.unwrap();
    assert_eq!((jobs[0].job_id.as_str(), jobs[0].call_id.as_str(), jobs[0].block_id.as_deref()), ("job-1", "a", Some("b1")), "listed again");
    system(&runtime, &id, |t| t == "Background shell job-1 (`cargo a`) in block b1 exited with code 0.\nbuilt").await;
    let view = runtime.view(&id).await.unwrap();
    assert!(!texts(&view).contains(&format!("{}a`) was stopped", stopped("job-1"))));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn undo_works_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let file = dir.path().join("f.txt");
    let edits = vec![
        call("w", "write", serde_json::json!({"path": "f.txt", "content": "one\n"})),
        call("e", "edit", serde_json::json!({"path": "f.txt", "old_string": "one", "new_string": "two"})),
        text("done"),
    ];
    let store = Arc::new(Mutex::new(Store::open(&path).unwrap()));
    let runtime = Runtime::new(config(dir.path(), store, model(edits))).await.unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap();
    let session = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let id = session.id.clone();
    let (_, mut live) = runtime.subscribe(&id, 0).await.unwrap();
    let input = vec![otto_proto::InputPart::Text { text: "edit".into() }];
    session.prompt(input.clone(), otto_proto::Delivery::Queue).await.unwrap();
    finished(&mut live).await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "two\n");
    runtime.shutdown().await.unwrap();
    drop((session, live, runtime));
    // A new daemon: the history is in the store.
    let store = Arc::new(Mutex::new(Store::open(&path).unwrap()));
    let undo = vec![call("u1", "undo_edit", serde_json::json!({"path": "f.txt"})), text("undone")];
    let runtime = Runtime::new(config(dir.path(), store.clone(), model(undo))).await.unwrap();
    let before = runtime.view(&id).await.unwrap();
    // Upkeep never breaks what a view replays.
    store.lock().unwrap().maintain().unwrap();
    let after = runtime.view(&id).await.unwrap();
    assert_eq!((texts(&before), before.seq), (texts(&after), after.seq));
    let (replay, mut live) = runtime.subscribe(&id, 0).await.unwrap();
    assert!(replay.windows(2).all(|p| p[0].0 < p[1].0) && replay.last().unwrap().0 == after.seq);
    runtime.prompt(&id, input, otto_proto::Delivery::Queue).await.unwrap();
    finished(&mut live).await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n", "the edit before the restart, taken back");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_shut_down_runtime_reopens_and_delivers_what_arrived_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let runtime = Runtime::new(config(dir.path(), store.clone(), model(vec![text("noted")]))).await.unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap();
    let id = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap().id;
    runtime.shutdown().await.unwrap();
    assert!(runtime.session(&id).await.is_err(), "closed");
    // A background job reported while it was closed: the wake failed, the entry waits.
    let notice = "Background shell job-9 (`make`) exited with code 0.\n";
    store.lock().unwrap().outbox_push(&id, &id, 1, serde_json::json!({ "notice": notice })).unwrap();
    assert_eq!(runtime.reopen().await.unwrap(), 1);
    system(&runtime, &id, |t| t == notice).await;
    runtime.shutdown().await.unwrap();
}
