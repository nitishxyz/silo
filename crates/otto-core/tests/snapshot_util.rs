//! Shared by the turn snapshot tests: a scripted turn in a folder, a temp repo.
#![allow(dead_code)]
use otto_core::*;
use otto_llm::*;
use otto_proto::{Delivery, Event, InputPart, TurnChanges, TurnStatus};
use otto_store::Store;
use std::{
    collections::VecDeque,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

pub struct Scripted {
    info: ModelInfo,
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
}
#[async_trait::async_trait]
impl Model for Scripted {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    async fn stream(&self, _: Request, _: CallCx) -> std::result::Result<EventStream, LlmError> {
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
pub fn text(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::PartDone { part: Part::text(text) }, StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }]
}
pub fn call(id: &str, name: &str, input: serde_json::Value) -> StreamEvent {
    StreamEvent::PartDone { part: Part::ToolCall { call_id: id.into(), name: name.into(), input, kind: CallKind::Function, opaque: None } }
}
/// Run git in `dir`, its trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
/// A repo at `dir` with one commit (`tracked.txt`), an ignored file and a staged change.
pub fn repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    git(dir, &["config", "user.name", "Otto Test"]);
    git(dir, &["config", "user.email", "otto@example.com"]);
    std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "ignored.log\n").unwrap();
    git(dir, &["add", "tracked.txt", ".gitignore"]);
    git(dir, &["commit", "-q", "-m", "init"]);
    std::fs::write(dir.join("ignored.log"), "noise\n").unwrap();
    // Staged but not committed: the snapshot covers it, the user's index keeps it staged.
    std::fs::write(dir.join("staged.txt"), "s\n").unwrap();
    git(dir, &["add", "staged.txt"]);
}
pub struct Turn {
    pub runtime: Runtime,
    pub store: Arc<Mutex<Store>>,
    pub session: String,
    pub events: Vec<Event>,
}
impl Turn {
    pub fn finished(&self) -> (TurnStatus, Option<TurnChanges>) {
        self.events
            .iter()
            .find_map(|e| if let Event::TurnFinished { status, changes, .. } = e { Some((*status, changes.clone())) } else { None })
            .unwrap()
    }
    pub fn trees(&self) -> (Option<String>, Option<String>) {
        self.store.lock().unwrap().turn_trees(&self.session, 1).unwrap()
    }
}
/// One turn in `cwd` (state kept in `state`) running `steps`, then "done".
pub async fn turn(state: &Path, cwd: &Path, steps: Vec<Vec<StreamEvent>>, defaults: SessionDefaults) -> Turn {
    let mut scripts: VecDeque<_> = steps.into();
    scripts.push_back(text("done"));
    let model = Arc::new(Scripted {
        info: ModelInfo { id: "test".into(), provider: "test".into(), name: "test".into(), caps: Caps::default(), pricing: None, list_pricing: None },
        scripts: Mutex::new(scripts),
    });
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let config = RuntimeConfig {
        store: store.clone(),
        models: Arc::new(Resolver(model)),
        state_dir: state.join("state"),
        user_config_dir: state.join("user"),
    };
    let runtime = Runtime::new(config).await.unwrap();
    runtime.set_config(defaults).unwrap();
    let project = runtime.open_project(cwd).await.unwrap();
    let s = runtime.create_session(CreateSession { project_id: project.id, cwd: cwd.into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&s.id, 0).await.unwrap();
    s.prompt(vec![InputPart::Text { text: "go".into() }], Delivery::Queue).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async { while !matches!(live.recv().await.unwrap().1, Event::TurnFinished { .. }) {} })
        .await
        .unwrap();
    let (events, _) = runtime.subscribe(&s.id, 0).await.unwrap();
    Turn { runtime, store, session: s.id, events: events.into_iter().map(|(_, e)| e).collect() }
}
/// A write with the edit tool and a change by `shell`.
pub fn edits() -> Vec<Vec<StreamEvent>> {
    vec![vec![
        call("w", "write", serde_json::json!({"path":"written.txt","content":"a\nb\n"})),
        call("s", "shell", serde_json::json!({"command":"printf 'two\\n' >> tracked.txt && echo hi > made.txt"})),
    ]]
}
