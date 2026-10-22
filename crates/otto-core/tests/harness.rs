//! O4 harness depth: durable outbox, queue edits, hooks, skills/commands/agents,
//! project tool sources and deferred loading. Offline (scripted model).
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
    requests: Mutex<Vec<Request>>,
}
impl Scripted {
    fn new(scripts: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            info: ModelInfo { id: "test".into(), provider: "test".into(), name: "test".into(), caps: Caps::default(), pricing: None },
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(vec![]),
        })
    }
}
#[async_trait::async_trait]
impl Model for Scripted {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    async fn stream(&self, request: Request, _cx: CallCx) -> std::result::Result<EventStream, LlmError> {
        self.requests.lock().unwrap().push(request);
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
fn text(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::PartDone { part: Part::text(text) }, StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }]
}
fn call(id: &str, name: &str, input: serde_json::Value) -> StreamEvent {
    StreamEvent::PartDone { part: Part::ToolCall { call_id: id.into(), name: name.into(), input, kind: CallKind::Function, opaque: None } }
}
fn config(dir: &Path, store: Store, model: Arc<Scripted>) -> RuntimeConfig {
    RuntimeConfig {
        store: Arc::new(Mutex::new(store)),
        models: Arc::new(Resolver(model)),
        state_dir: dir.join("state"),
        user_config_dir: dir.join("user"),
    }
}
async fn finish(live: &mut LiveEvents) {
    tokio::time::timeout(Duration::from_secs(5), async { while !matches!(live.recv().await.unwrap().1, Event::TurnFinished { .. }) {} })
        .await
        .unwrap();
}
async fn say(session: &SessionHandle, text: &str, delivery: Delivery) -> String {
    session.prompt(vec![InputPart::Text { text: text.into() }], delivery).await.unwrap()
}
/// Poll a snapshot until `done` holds (the actor is opened by the first poll).
async fn until(runtime: &Runtime, id: &str, done: impl Fn(&otto_proto::SessionView) -> bool) -> otto_proto::SessionView {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = runtime.view(id).await.unwrap();
            if done(&view) {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}
fn deliveries(view: &otto_proto::SessionView) -> usize {
    view.items.iter().filter(|i| i.id.starts_with("outbox-")).count()
}

#[tokio::test]
async fn child_completion_wakes_a_parent_whose_actor_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let model = Scripted::new(vec![]);
    let runtime = Runtime::new(config(dir.path(), Store::open(&path).unwrap(), model.clone())).await.unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap();
    let parent =
        runtime.create_session(CreateSession { project_id: project.id.clone(), cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let child = runtime
        .create_session(CreateSession {
            project_id: project.id,
            cwd: dir.path().into(),
            parent_id: Some(parent.id.clone()),
            task: Some("inspect".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    drop(runtime);
    // Fresh runtime: only the child's actor is opened.
    let model = Scripted::new(vec![text("child result"), text("parent incorporated")]);
    let runtime = Runtime::new(config(dir.path(), Store::open(&path).unwrap(), model.clone())).await.unwrap();
    let child = runtime.session(&child.id).await.unwrap();
    say(&child, "inspect", Delivery::Queue).await;
    let view = until(&runtime, &parent.id, |v| v.items.last().is_some_and(|i| i.text() == "parent incorporated")).await;
    assert_eq!(deliveries(&view), 1);
    assert!(serde_json::to_string(&view.items).unwrap().contains("child result"));
    let (events, _) = runtime.subscribe(&parent.id, 0).await.unwrap();
    assert!(
        events.iter().any(|(_, e)| matches!(e, Event::SubagentChanged { child: c, status: otto_proto::SessionStatus::Idle, .. } if *c == child.id))
    );
    assert_eq!(runtime.deliver_pending().await.unwrap(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn outbox_is_delivered_once_across_restarts_including_interrupted_children() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (parent, done_child, busy_child) = {
        let mut store = Store::open(&path).unwrap();
        let project = store.create_project(dir.path().to_str().unwrap()).unwrap();
        let session = |store: &mut Store, parent: Option<String>| {
            store
                .create_session(otto_store::NewSession {
                    project_id: project.id.clone(),
                    parent_id: parent,
                    agent: "build".into(),
                    model: "test".into(),
                    provider: "test".into(),
                    title: "t".into(),
                    cwd: dir.path().to_string_lossy().into(),
                    meta: serde_json::json!({}),
                })
                .unwrap()
                .id
        };
        let parent = session(&mut store, None);
        let done_child = session(&mut store, Some(parent.clone()));
        let busy_child = session(&mut store, Some(parent.clone()));
        // A completion recorded just before the daemon stopped, plus a duplicate push.
        let payload = serde_json::json!({"child":done_child,"status":"done","turn":1,"result":"finished work"});
        let first = store.outbox_push(&parent, &done_child, 1, payload.clone()).unwrap();
        assert_eq!(store.outbox_push(&parent, &done_child, 1, payload).unwrap(), first);
        // A child that was mid-turn when the daemon stopped.
        store
            .append_with_session(
                &busy_child,
                otto_store::EventRecord { turn: 7, step: 0, kind: "turn.started".into(), payload: serde_json::json!({"turn":7}) },
                otto_store::Projection::TurnStatus(otto_store::TurnStatus::Running),
                otto_store::SessionUpdate { status: Some("busy".into()), ..Default::default() },
            )
            .unwrap();
        (parent, done_child, busy_child)
    };
    let model = Scripted::new(vec![text("parent woke")]);
    let runtime = Runtime::new(config(dir.path(), Store::open(&path).unwrap(), model.clone())).await.unwrap();
    assert_eq!(runtime.deliver_pending().await.unwrap(), 1);
    let view = until(&runtime, &parent, |v| v.items.last().is_some_and(|i| i.text() == "parent woke")).await;
    assert_eq!(deliveries(&view), 2);
    let transcript = serde_json::to_string(&view.items).unwrap();
    assert!(transcript.contains("finished work") && transcript.contains(&done_child));
    assert!(transcript.contains(&busy_child) && transcript.contains("interrupted"));
    assert_eq!(model.requests.lock().unwrap().len(), 1, "one wake turn for both entries");
    runtime.shutdown().await.unwrap();
    drop(runtime);
    let model = Scripted::new(vec![]);
    let runtime = Runtime::new(config(dir.path(), Store::open(&path).unwrap(), model.clone())).await.unwrap();
    assert_eq!(runtime.deliver_pending().await.unwrap(), 0);
    assert_eq!(deliveries(&runtime.view(&parent).await.unwrap()), 2);
    assert!(model.requests.lock().unwrap().is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn queue_remove_and_redeliver_keep_order_and_journal() {
    let dir = tempfile::tempdir().unwrap();
    let model = Scripted::new(vec![]);
    let runtime = Runtime::new(config(dir.path(), Store::open_in_memory().unwrap(), model.clone())).await.unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap();
    let s = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&s.id, 0).await.unwrap();
    say(&s, "first", Delivery::Queue).await;
    while !matches!(live.recv().await.unwrap().1, Event::ItemStarted { .. }) {}
    let a = say(&s, "a", Delivery::Queue).await;
    let b = say(&s, "b", Delivery::Queue).await;
    let c = say(&s, "c", Delivery::Steer).await;
    let queue = |view: otto_proto::SessionView| view.queue.into_iter().map(|q| (q.id, q.delivery)).collect::<Vec<_>>();
    assert_eq!(queue(s.view().await.unwrap()), [(c.clone(), Delivery::Steer), (a.clone(), Delivery::Queue), (b.clone(), Delivery::Queue)]);
    runtime.queue_remove(&s.id, &a).await.unwrap();
    assert!(runtime.queue_remove(&s.id, &a).await.is_err());
    runtime.queue_update(&s.id, &b, Delivery::Steer).await.unwrap();
    runtime.queue_update(&s.id, &c, Delivery::Queue).await.unwrap();
    assert_eq!(queue(s.view().await.unwrap()), [(b.clone(), Delivery::Steer), (c.clone(), Delivery::Queue)]);
    let (events, _) = runtime.subscribe(&s.id, 0).await.unwrap();
    let last = events.iter().rev().find_map(|(_, e)| if let Event::QueueChanged { queued } = e { Some(queued.clone()) } else { None }).unwrap();
    assert_eq!(last.iter().map(|q| (q.id.clone(), q.delivery)).collect::<Vec<_>>(), [(b.clone(), Delivery::Steer), (c.clone(), Delivery::Queue)]);
    // Interrupting with c cancels the turn; c runs first, then the steer the turn never reached.
    model.scripts.lock().unwrap().extend([text("for c"), text("for b")]);
    runtime.queue_update(&s.id, &c, Delivery::Interrupt).await.unwrap();
    for _ in 0..3 {
        finish(&mut live).await;
    }
    let inputs: Vec<String> = s.view().await.unwrap().items.iter().filter(|i| i.role == Role::User).map(Item::text).collect();
    assert_eq!(inputs, ["first", "c", "b"]);
    runtime.shutdown().await.unwrap();
}

fn write(path: impl AsRef<Path>, text: &str) {
    std::fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn claude_code_hooks_block_prompts_continue_stops_and_match_tool_names() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root.join(".claude/settings.json"),
        &serde_json::json!({"hooks":{
            "UserPromptSubmit":[{"hooks":[{"type":"command","command":"grep -q forbidden && { echo 'no forbidden prompts' >&2; exit 2; }; echo 'prompt context'"}]}],
            "Stop":[{"hooks":[{"type":"command","command":"cat >/dev/null; if [ -e stopped ]; then exit 0; fi; touch stopped; echo '{\"decision\":\"block\",\"reason\":\"run the tests\"}'"}]}],
            "PreToolUse":[{"matcher":"Write|Edit","hooks":[{"type":"command","command":"echo '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"read-only repo\"}}'"}]}]
        }})
        .to_string(),
    );
    let model = Scripted::new(vec![
        text("first answer"),
        vec![call("w", "write", serde_json::json!({"path":"blocked.txt","content":"x"}))],
        text("second answer"),
    ]);
    let runtime = Runtime::new(config(root, Store::open_in_memory().unwrap(), model.clone())).await.unwrap();
    let project = runtime.open_project(root).await.unwrap();
    let s = runtime.create_session(CreateSession { project_id: project.id, cwd: root.into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&s.id, 0).await.unwrap();
    say(&s, "do the forbidden thing", Delivery::Queue).await;
    finish(&mut live).await;
    assert!(model.requests.lock().unwrap().is_empty());
    say(&s, "hello", Delivery::Queue).await;
    finish(&mut live).await;
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let first = serde_json::to_string(&requests[0].items).unwrap();
    assert!(first.contains("prompt context") && !first.contains("forbidden"));
    assert!(requests[1].items.last().unwrap().text().contains("run the tests"));
    assert!(!root.join("blocked.txt").exists());
    assert!(serde_json::to_string(&requests[2].items).unwrap().contains("read-only repo"));
    let (events, _) = runtime.subscribe(&s.id, 0).await.unwrap();
    assert!(
        events.iter().any(|(_, e)| matches!(e, Event::Error { class, message, .. } if class == "hook" && message.contains("no forbidden prompts")))
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn skills_commands_and_agents_are_discovered_claude_style() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root.join(".claude/skills/release/SKILL.md"), "---\nname: release\ndescription: >\n  Cut a\n  release\n---\nSteps: tag it.");
    write(root.join(".otto/skills/private/SKILL.md"), "---\nname: private\ndescription: Hidden\ndisable-model-invocation: true\n---\nx");
    write(root.join(".claude/commands/ship.md"), "---\ndescription: Ship it\nargument-hint: <version>\n---\nShip version $1 now");
    write(
        root.join(".claude/agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Reviews\ntools: Read, Grep\nmodel: inherit\npermissionMode: plan\n---\nReview carefully.",
    );
    let model = Scripted::new(vec![vec![call("s", "skill", serde_json::json!({"name":"release"}))], text("done")]);
    let runtime = Runtime::new(config(root, Store::open_in_memory().unwrap(), model.clone())).await.unwrap();
    let skills = runtime.skills(Some(root));
    assert_eq!(
        skills.iter().map(|s| (s.name.as_str(), s.description.as_str())).collect::<Vec<_>>(),
        [("private", "Hidden"), ("release", "Cut a release")]
    );
    let commands = runtime.commands(Some(root));
    assert_eq!(commands[0].name, "ship");
    assert_eq!(commands[0].argument_hint.as_deref(), Some("<version>"));
    assert!(runtime.agents(Some(root)).iter().any(|a| a.name == "reviewer" && a.model.is_none()));
    let project = runtime.open_project(root).await.unwrap();
    let reviewer = runtime
        .create_session(CreateSession { project_id: project.id.clone(), cwd: root.into(), agent: "reviewer".into(), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(reviewer.view().await.unwrap().info.approval_mode, otto_proto::ApprovalMode::Plan);
    let mut tools: Vec<_> = runtime.tools(&reviewer.id).await.unwrap().into_iter().map(|t| t.name).collect();
    tools.sort();
    assert_eq!(tools, ["read", "search"]);
    let s = runtime.create_session(CreateSession { project_id: project.id, cwd: root.into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&s.id, 0).await.unwrap();
    say(&s, "/ship 1.2", Delivery::Queue).await;
    finish(&mut live).await;
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests[0].items[0].text(), "Ship version 1.2 now");
    let system = requests[0].system.join("\n");
    assert!(system.contains("release: Cut a release") && !system.contains("private: Hidden"));
    assert!(serde_json::to_string(&requests[1].items).unwrap().contains("Steps: tag it."));
    runtime.shutdown().await.unwrap();
}

/// A project source whose tool list grows after its first tool call.
struct Source {
    grown: Arc<std::sync::atomic::AtomicBool>,
}
struct Fake {
    spec: otto_tools::ToolSpec,
    grown: Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait::async_trait]
impl otto_tools::Tool for Fake {
    fn spec(&self) -> &otto_tools::ToolSpec {
        &self.spec
    }
    async fn call(&self, _: serde_json::Value, _: otto_tools::ToolCx) -> std::result::Result<otto_tools::ToolOutput, otto_tools::ToolError> {
        self.grown.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(otto_tools::ToolOutput::text(format!("{} ran", self.spec.name)))
    }
}
#[async_trait::async_trait]
impl otto_tools::ProjectSources for Source {
    async fn tools(&self, _: &Path) -> Vec<Arc<dyn otto_tools::Tool>> {
        let mut names = vec![("mcp__fake__alpha", "deferred"), ("mcp__fake__beta", "deferred")];
        if self.grown.load(std::sync::atomic::Ordering::SeqCst) {
            names.push(("mcp__fake__late", "eager"));
        }
        names
            .into_iter()
            .map(|(name, loading)| {
                let spec = serde_json::from_value(serde_json::json!({"name":name,"description":format!("Fake {name}."),"input_schema":{"type":"object","properties":{}},"loading":loading,"effects":{"read":true}})).unwrap();
                Arc::new(Fake { spec, grown: self.grown.clone() }) as Arc<dyn otto_tools::Tool>
            })
            .collect()
    }
}

#[tokio::test]
async fn project_sources_load_by_prefix_and_list_changes_reach_the_next_step() {
    let dir = tempfile::tempdir().unwrap();
    let model = Scripted::new(vec![
        vec![call("l", "load_tools", serde_json::json!({"names":["mcp__fake__*"]}))],
        vec![call("a", "mcp__fake__alpha", serde_json::json!({}))],
        text("done"),
    ]);
    let runtime = Runtime::new(config(dir.path(), Store::open_in_memory().unwrap(), model.clone())).await.unwrap();
    runtime.add_source(Arc::new(Source { grown: Arc::default() }));
    let project = runtime.open_project(dir.path()).await.unwrap();
    let s = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&s.id, 0).await.unwrap();
    say(&s, "go", Delivery::Queue).await;
    finish(&mut live).await;
    let requests = model.requests.lock().unwrap().clone();
    let names = |i: usize| requests[i].tools.iter().map(|t| t.name.clone()).filter(|n| n.starts_with("mcp__")).collect::<Vec<_>>();
    assert!(names(0).is_empty());
    assert_eq!(names(1), ["mcp__fake__alpha", "mcp__fake__beta"]);
    assert_eq!(names(2), ["mcp__fake__alpha", "mcp__fake__beta", "mcp__fake__late"]);
    assert!(!requests[2].tools.iter().any(|t| t.name == "load_tools"));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn old_tool_results_are_cleared_in_requests_with_pairs_intact() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.txt"), "y".repeat(30_000)).unwrap();
    let read = |id: &str| vec![call(id, "read", serde_json::json!({"path":"big.txt"}))];
    let model = Scripted::new(vec![read("r1"), read("r2"), read("r3"), read("r4"), read("r5"), text("done")]);
    let runtime = Runtime::new(config(dir.path(), Store::open_in_memory().unwrap(), model.clone())).await.unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap();
    let s = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&s.id, 0).await.unwrap();
    say(&s, "read it", Delivery::Queue).await;
    finish(&mut live).await;
    let last = model.requests.lock().unwrap().last().cloned().unwrap();
    let results: Vec<(String, bool)> = last
        .items
        .iter()
        .flat_map(|i| &i.parts)
        .filter_map(|p| {
            if let Part::ToolResult { call_id, content, .. } = p {
                Some((call_id.clone(), serde_json::to_string(content).unwrap().contains("cleared")))
            } else {
                None
            }
        })
        .collect();
    let calls: Vec<String> = last.items.iter().flat_map(|i| i.tool_calls().map(|(id, ..)| id.to_owned()).collect::<Vec<_>>()).collect();
    assert_eq!(calls, ["r1", "r2", "r3", "r4", "r5"]);
    assert_eq!(results.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), calls);
    assert!(results[0].1, "the oldest result is cleared");
    assert!(results[2..].iter().all(|r| !r.1), "the newest results stay");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_compaction_is_opt_in_and_replays_the_opaque_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let checkpoint = Part::Compaction {
        summary: Some("native summary".into()),
        opaque: Some(Opaque { provider: "test".into(), model: "test".into(), data: serde_json::json!({"blob":"x"}) }),
    };
    let model = Scripted::new(vec![
        text("first"),
        vec![StreamEvent::PartDone { part: checkpoint.clone() }, StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }],
        text("second"),
    ]);
    let caps = Caps { native_compaction: true, ..Default::default() };
    let model = Arc::new(Scripted {
        info: ModelInfo { caps, ..model.info.clone() },
        scripts: Mutex::new(std::mem::take(&mut *model.scripts.lock().unwrap())),
        requests: Mutex::new(vec![]),
    });
    let runtime = Runtime::new(config(dir.path(), Store::open_in_memory().unwrap(), model.clone())).await.unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap();
    runtime.set_config(SessionDefaults { native_compaction: true, ..Default::default() }).unwrap();
    let native = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&native.id, 0).await.unwrap();
    say(&native, "hello", Delivery::Queue).await;
    finish(&mut live).await;
    native.compact().await.unwrap();
    say(&native, "again", Delivery::Queue).await;
    finish(&mut live).await;
    let requests = model.requests.lock().unwrap().clone();
    assert!(requests[0].compaction.is_none());
    assert!(matches!(requests[1].compaction, Some(Compaction::Now { .. })));
    assert_eq!(requests[2].items[0].parts[0], checkpoint);
    assert_eq!(requests[2].items.last().unwrap().text(), "again");
    runtime.shutdown().await.unwrap();
}

fn with_caps(caps: Caps, scripts: Vec<Vec<StreamEvent>>) -> Arc<Scripted> {
    Arc::new(Scripted {
        info: ModelInfo { id: "test".into(), provider: "test".into(), name: "test".into(), caps, pricing: None },
        scripts: Mutex::new(scripts.into()),
        requests: Mutex::new(vec![]),
    })
}
async fn turn(dir: &Path, model: Arc<Scripted>, defaults: SessionDefaults) -> (Runtime, SessionHandle, Vec<(i64, Event)>) {
    let runtime = Runtime::new(config(dir, Store::open_in_memory().unwrap(), model)).await.unwrap();
    runtime.set_config(defaults).unwrap();
    let project = runtime.open_project(dir).await.unwrap();
    let s = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&s.id, 0).await.unwrap();
    say(&s, "go", Delivery::Queue).await;
    finish(&mut live).await;
    let (events, _) = runtime.subscribe(&s.id, 0).await.unwrap();
    (runtime, s, events)
}
fn tool_calls(events: &[(i64, Event)]) -> Vec<String> {
    events.iter().filter_map(|(_, e)| if let Event::ToolCall { name, .. } = e { Some(name.clone()) } else { None }).collect()
}
fn results(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .flat_map(|i| &i.parts)
        .filter_map(|p| if let Part::ToolResult { content, .. } = p { Some(serde_json::to_string(content).unwrap()) } else { None })
        .collect()
}

#[tokio::test]
async fn claude_native_editor_and_bash_map_onto_builtins() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    let caps =
        Caps { tool_family: ToolFamily::Anthropic, native_tools: vec!["text_editor_20250728".into(), "bash_20250124".into()], ..Default::default() };
    let model = with_caps(
        caps,
        vec![
            vec![call("v", "str_replace_based_edit_tool", serde_json::json!({"command":"view","path":"a.txt","view_range":[2,2]}))],
            vec![call("c", "str_replace_based_edit_tool", serde_json::json!({"command":"create","path":"new.txt","file_text":"hi\n"}))],
            vec![call(
                "i",
                "str_replace_based_edit_tool",
                serde_json::json!({"command":"insert","path":"a.txt","insert_line":1,"insert_text":"mid"}),
            )],
            vec![call(
                "s",
                "str_replace_based_edit_tool",
                serde_json::json!({"command":"str_replace","path":"a.txt","old_str":"two","new_str":"TWO"}),
            )],
            vec![call("b", "bash", serde_json::json!({"command":"echo native-bash"}))],
            vec![call("u", "str_replace_based_edit_tool", serde_json::json!({"command":"undo_edit","path":"a.txt"}))],
            text("done"),
        ],
    );
    let (runtime, s, events) = turn(dir.path(), model.clone(), SessionDefaults::default()).await;
    let first = model.requests.lock().unwrap()[0].clone();
    let kind = |name: &str| first.tools.iter().find(|t| t.name == name).map(|t| t.kind.clone());
    assert_eq!(kind("str_replace_based_edit_tool"), Some(ToolKind::Native { tool: "text_editor_20250728".into(), config: serde_json::Value::Null }));
    assert_eq!(kind("bash"), Some(ToolKind::Native { tool: "bash_20250124".into(), config: serde_json::Value::Null }));
    assert!(kind("edit").is_none() && kind("shell").is_none());
    // An unmappable call keeps its native name and fails without running anything.
    assert_eq!(tool_calls(&events), ["read", "write", "write", "edit", "shell", "str_replace_based_edit_tool"]);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\nmid\nTWO\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("new.txt")).unwrap(), "hi\n");
    let results = results(&s.view().await.unwrap().items);
    assert!(results[0].contains("2: two"));
    assert!(results[4].contains("native-bash"));
    assert!(results[5].contains("undo_edit is not supported"));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn gpt_native_patch_and_shell_and_async_tools() {
    let dir = tempfile::tempdir().unwrap();
    let caps =
        Caps { tool_family: ToolFamily::Openai, async_tools: true, native_tools: vec!["apply_patch".into(), "shell".into()], ..Default::default() };
    let model = with_caps(
        caps,
        vec![
            vec![call("p", "apply_patch", serde_json::json!({"type":"create_file","path":"n.py","diff":"+x = 1\n"}))],
            vec![call("sh", "shell", serde_json::json!({"commands":["echo out; exit 3"],"timeout_ms":5000}))],
            text("done"),
        ],
    );
    let (runtime, s, events) = turn(dir.path(), model.clone(), SessionDefaults::default()).await;
    let first = model.requests.lock().unwrap()[0].clone();
    let kind = |name: &str| first.tools.iter().find(|t| t.name == name).map(|t| t.kind.clone());
    assert_eq!(kind("apply_patch"), Some(ToolKind::Native { tool: "apply_patch".into(), config: serde_json::Value::Null }));
    assert_eq!(kind("shell"), Some(ToolKind::Native { tool: "shell".into(), config: serde_json::json!({"environment":{"type":"local"}}) }));
    assert_eq!(first.provider.openai.as_ref().unwrap().async_tools, ["web_fetch"]);
    assert_eq!(tool_calls(&events), ["apply_patch", "shell"]);
    assert_eq!(std::fs::read_to_string(dir.path().join("n.py")).unwrap(), "x = 1\n");
    let items = s.view().await.unwrap().items;
    let shell = items.iter().flat_map(|i| &i.parts).find_map(|p| match p {
        Part::ToolResult { call_id, content, .. } if call_id == "sh" => Some(content.clone()),
        _ => None,
    });
    let Some(ResultPart::Text { text }) = shell.unwrap().into_iter().next() else { panic!("text result") };
    let array: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(array[0]["outcome"], serde_json::json!({"type":"exit","exit_code":3}));
    assert!(array[0]["stdout"].as_str().unwrap().contains("out"));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn gpt_without_native_caps_gets_the_builtin_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let model = with_caps(Caps { tool_family: ToolFamily::Openai, custom_tools: true, ..Default::default() }, vec![text("done")]);
    let (runtime, _, _) = turn(dir.path(), model.clone(), SessionDefaults::default()).await;
    let first = model.requests.lock().unwrap()[0].clone();
    let kind = |name: &str| first.tools.iter().find(|t| t.name == name).map(|t| t.kind.clone());
    assert_eq!(kind("apply_patch"), Some(ToolKind::Custom { format: Some(serde_json::json!({"type":"text"})) }));
    assert_eq!(kind("shell"), Some(ToolKind::Function));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn threshold_compaction_is_requested_and_its_checkpoint_replaces_history() {
    let dir = tempfile::tempdir().unwrap();
    let checkpoint = Part::Compaction {
        summary: Some("threshold summary".into()),
        opaque: Some(Opaque { provider: "test".into(), model: "test".into(), data: serde_json::json!({"blob":1}) }),
    };
    let caps = Caps { native_compaction: true, context_window: 1000, ..Default::default() };
    let model = with_caps(
        caps,
        vec![
            vec![
                StreamEvent::PartDone { part: checkpoint.clone() },
                StreamEvent::Finish { reason: FinishReason::Other("compaction".into()), response_id: None },
            ],
            text("after compaction"),
        ],
    );
    let (runtime, s, _) = turn(dir.path(), model.clone(), SessionDefaults { native_compaction: true, ..Default::default() }).await;
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "a compaction finish continues the turn");
    assert_eq!(requests[0].compaction, Some(Compaction::Auto { threshold: 750, instructions: None }));
    assert_eq!(requests[1].items[0].parts[0], checkpoint);
    // The checkpoint drops what came before it: the turn's prompt follows it again.
    assert_eq!(requests[1].items.len(), 2);
    assert_eq!(requests[1].items[1].role, Role::User);
    assert_eq!(requests[1].items[1].parts, requests[0].items.last().unwrap().parts);
    assert_eq!(s.view().await.unwrap().items.last().unwrap().text(), "after compaction");
    // Not opted in: no provider compaction.
    let dir = tempfile::tempdir().unwrap();
    let model = with_caps(Caps { native_compaction: true, context_window: 1000, ..Default::default() }, vec![text("plain")]);
    let (other, _, _) = turn(dir.path(), model.clone(), SessionDefaults::default()).await;
    assert!(model.requests.lock().unwrap()[0].compaction.is_none());
    other.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}
