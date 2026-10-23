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
            // A reply that streams, then never ends.
            Some(mut events) if matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::Other(r), .. }) if r == "hang") => {
                events.pop();
                Box::pin(futures::StreamExt::chain(futures::stream::iter(events.into_iter().map(Ok)), futures::stream::pending()))
            }
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

/// A parent and its delegated child.
async fn family(runtime: &Runtime, dir: &Path) -> (String, String) {
    let project = runtime.open_project(dir).await.unwrap();
    let parent = runtime.create_session(CreateSession { project_id: project.id.clone(), cwd: dir.into(), ..Default::default() }).await.unwrap();
    let child = runtime
        .create_session(CreateSession {
            project_id: project.id,
            cwd: dir.into(),
            parent_id: Some(parent.id.clone()),
            task: Some("inspect".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    (parent.id, child.id)
}
/// Start a child turn on a model that never answers, then stop it as the user.
async fn user_stops(runtime: &Runtime, child: &str) {
    let (_, mut live) = runtime.subscribe(child, 0).await.unwrap();
    runtime.prompt(child, vec![InputPart::Text { text: "inspect".into() }], Delivery::Queue).await.unwrap();
    while !matches!(live.recv().await.unwrap().1, Event::TurnStarted { .. }) {}
    runtime.cancel(child).await.unwrap();
    finish(&mut live).await;
}
/// The child starts replying (`text` streams), then the user stops it.
async fn user_stops_mid_reply(runtime: &Runtime, model: &Scripted, child: &str, text: &str) {
    model.scripts.lock().unwrap().push_back(vec![
        StreamEvent::TextDelta { text: text.into() },
        StreamEvent::Finish { reason: FinishReason::Other("hang".into()), response_id: None },
    ]);
    let (_, mut live) = runtime.subscribe(child, 0).await.unwrap();
    runtime.prompt(child, vec![InputPart::Text { text: "inspect".into() }], Delivery::Queue).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async { while !matches!(live.recv().await.unwrap().1, Event::TextDelta { .. }) {} }).await.unwrap();
    runtime.cancel(child).await.unwrap();
    finish(&mut live).await;
}
fn outbox_texts(view: &otto_proto::SessionView) -> Vec<String> {
    view.items.iter().filter(|i| i.id.starts_with("outbox-")).map(|i| serde_json::to_string(&i.parts).unwrap()).collect()
}

#[tokio::test]
async fn user_takeover_holds_the_report_until_a_finished_turn_and_notes_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let model = Scripted::new(vec![]);
    let runtime = Runtime::new(config(dir.path(), Store::open(&path).unwrap(), model.clone())).await.unwrap();
    let (parent, child) = family(&runtime, dir.path()).await;
    user_stops(&runtime, &child).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(deliveries(&runtime.view(&parent).await.unwrap()), 0, "a user-stopped turn does not report");
    // New instructions: that turn reports once, noting the takeover.
    model.scripts.lock().unwrap().extend([text("child result"), text("parent incorporated")]);
    runtime.prompt(&child, vec![InputPart::Text { text: "do it this way".into() }], Delivery::Queue).await.unwrap();
    let view = until(&runtime, &parent, |v| v.items.last().is_some_and(|i| i.text() == "parent incorporated")).await;
    let texts = outbox_texts(&view);
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("child result") && texts[0].contains("The user took over this sub-agent") && !texts[0].contains("stopped"));
    // A message to the finished child starts a new turn that reports again.
    model.scripts.lock().unwrap().extend([text("second result"), text("parent again")]);
    runtime.prompt(&child, vec![InputPart::Text { text: "one more thing".into() }], Delivery::Queue).await.unwrap();
    let view = until(&runtime, &parent, |v| v.items.last().is_some_and(|i| i.text() == "parent again")).await;
    let texts = outbox_texts(&view);
    assert_eq!(texts.len(), 2);
    assert!(texts[1].contains("second result") && texts[1].contains("The user steered this sub-agent"));
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
async fn a_held_report_waits_for_the_hand_back_and_goes_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let model = Scripted::new(vec![]);
    let runtime = Runtime::new(config(dir.path(), Store::open(&path).unwrap(), model.clone())).await.unwrap();
    let (parent, child) = family(&runtime, dir.path()).await;
    user_stops_mid_reply(&runtime, &model, &child, "half of the findings").await;
    // No timer: it holds while the user keeps it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(deliveries(&runtime.view(&parent).await.unwrap()), 0, "held, not released by time");
    assert!(runtime.view(&child).await.unwrap().info.report_held);
    // Only a held sub-agent can be handed back.
    assert!(runtime.hand_back(&parent).await.is_err());
    model.scripts.lock().unwrap().push_back(text("parent ack"));
    runtime.hand_back(&child).await.unwrap();
    assert!(runtime.hand_back(&child).await.is_err(), "a second hand-back has nothing to send");
    assert!(!runtime.view(&child).await.unwrap().info.report_held);
    let view = until(&runtime, &parent, |v| v.items.last().is_some_and(|i| i.text() == "parent ack")).await;
    let texts = outbox_texts(&view);
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("status stopped") && texts[0].contains("The user stopped this sub-agent and handed it back."));
    assert!(texts[0].contains("half of the findings"), "carries what it produced so far");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(deliveries(&runtime.view(&parent).await.unwrap()), 1);
    runtime.shutdown().await.unwrap();
    drop(runtime);
    // A restart sends nothing more.
    let model = Scripted::new(vec![]);
    let runtime = Runtime::new(config(dir.path(), Store::open(&path).unwrap(), model.clone())).await.unwrap();
    assert_eq!(runtime.deliver_pending().await.unwrap(), 0);
    assert_eq!(deliveries(&runtime.view(&parent).await.unwrap()), 1);
    assert!(model.requests.lock().unwrap().is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_view_times_reasoning_and_tool_calls_from_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let model = Scripted::new(vec![
        vec![
            StreamEvent::ReasoningDelta { text: "thinking".into(), summary: false },
            StreamEvent::PartDone { part: Part::Reasoning { summary: None, text: Some("thinking".into()), opaque: None } },
            call("c1", "no_such_tool", serde_json::json!({})),
            StreamEvent::Finish { reason: FinishReason::ToolUse, response_id: None },
        ],
        text("done"),
    ]);
    let runtime = Runtime::new(config(dir.path(), Store::open_in_memory().unwrap(), model.clone())).await.unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap();
    let session = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&session.id, 0).await.unwrap();
    say(&session, "go", Delivery::Queue).await;
    finish(&mut live).await;
    let view = runtime.view(&session.id).await.unwrap();
    let asked = view.items.iter().find(|i| i.tool_calls().next().is_some()).unwrap();
    let time = |part: u32| view.part_times.iter().find(|t| t.item_id == asked.id && t.part == part).cloned().unwrap();
    let (reasoning, call) = (time(0), time(1));
    assert!(reasoning.finished_at.is_some_and(|end| end >= reasoning.started_at));
    assert!(call.started_at >= reasoning.started_at && call.finished_at.is_some_and(|end| end >= call.started_at));
    assert_eq!(view.part_times.len(), 2, "text parts are not timed");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_view_taken_mid_reply_holds_the_text_streamed_so_far() {
    let dir = tempfile::tempdir().unwrap();
    let hang = StreamEvent::Finish { reason: FinishReason::Other("hang".into()), response_id: None };
    let model = Scripted::new(vec![vec![
        StreamEvent::ReasoningDelta { text: "hmm".into(), summary: false },
        StreamEvent::TextDelta { text: "Hello, ".into() },
        StreamEvent::TextDelta { text: "world".into() },
        hang,
    ]]);
    let runtime = Runtime::new(config(dir.path(), Store::open_in_memory().unwrap(), model.clone())).await.unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap();
    let session = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    say(&session, "go", Delivery::Queue).await;
    let view = until(&runtime, &session.id, |v| v.items.last().is_some_and(|i| i.role == Role::Assistant && i.text() == "Hello, world")).await;
    let open = view.items.last().unwrap();
    assert!(matches!(&open.parts[0], Part::Reasoning { text: Some(t), .. } if t == "hmm"));
    assert_eq!(view.items.iter().filter(|i| i.id == open.id).count(), 1);
    // A reply stopped unfinished isn't history: an idle view holds only finished items.
    let id = open.id.clone();
    session.cancel().await.unwrap();
    let view = until(&runtime, &session.id, |v| v.turn_started_at.is_none()).await;
    assert!(view.items.iter().all(|i| i.id != id));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn closing_a_child_that_holds_nothing_changes_nothing() {
    // Closing a sub-agent's block hands it back only while its report is held:
    // a child that already reported, or never stopped, has nothing to send.
    let dir = tempfile::tempdir().unwrap();
    let model = Scripted::new(vec![text("child result"), text("parent incorporated")]);
    let runtime = Runtime::new(config(dir.path(), Store::open_in_memory().unwrap(), model.clone())).await.unwrap();
    let (parent, child) = family(&runtime, dir.path()).await;
    assert!(runtime.hand_back(&child).await.is_err(), "never stopped");
    runtime.prompt(&child, vec![InputPart::Text { text: "inspect".into() }], Delivery::Queue).await.unwrap();
    let view = until(&runtime, &parent, |v| v.items.last().is_some_and(|i| i.text() == "parent incorporated")).await;
    assert_eq!(deliveries(&view), 1);
    assert!(!runtime.view(&child).await.unwrap().info.report_held);
    assert!(runtime.hand_back(&child).await.is_err(), "already reported");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(deliveries(&runtime.view(&parent).await.unwrap()), 1);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_held_report_is_delivered_once_at_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let model = Scripted::new(vec![]);
    let runtime = Runtime::new(config(dir.path(), Store::open(&path).unwrap(), model.clone())).await.unwrap();
    let (parent, child) = family(&runtime, dir.path()).await;
    user_stops(&runtime, &child).await;
    runtime.shutdown().await.unwrap();
    drop(runtime);
    for (woken, delivered) in [(1, 1), (0, 1)] {
        let model = Scripted::new(vec![text("parent ack")]);
        let runtime = Runtime::new(config(dir.path(), Store::open(&path).unwrap(), model.clone())).await.unwrap();
        assert_eq!(runtime.deliver_pending().await.unwrap(), woken);
        let view = if woken == 1 {
            until(&runtime, &parent, |v| v.items.last().is_some_and(|i| i.text() == "parent ack")).await
        } else {
            runtime.view(&parent).await.unwrap()
        };
        assert_eq!(deliveries(&view), delivered);
        assert!(outbox_texts(&view)[0].contains(&child));
        assert!(outbox_texts(&view)[0].contains("the otto server restarted before they handed it back"));
        runtime.shutdown().await.unwrap();
    }
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
    assert!(!requests[2].tools.iter().any(|t| t.name == "load_tools" && t.description.contains("mcp__fake__")));
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
async fn every_family_gets_our_own_tools() {
    for (family, patch) in [(ToolFamily::Anthropic, false), (ToolFamily::Openai, true), (ToolFamily::Generic, false)] {
        let dir = tempfile::tempdir().unwrap();
        let model = with_caps(Caps { tool_family: family, custom_tools: true, ..Default::default() }, vec![text("done")]);
        let (runtime, _, _) = turn(dir.path(), model.clone(), SessionDefaults::default()).await;
        let first = model.requests.lock().unwrap()[0].clone();
        let tool = |name: &str| first.tools.iter().find(|t| t.name == name).cloned();
        let shell = tool("shell").unwrap();
        assert_eq!(shell.kind, ToolKind::Function);
        assert!(["command", "cwd", "timeout_ms", "background"].iter().all(|k| shell.input_schema["properties"].get(k).is_some()));
        assert!(shell.description.contains("waiting_for_input") && shell.description.contains("terminal_open"));
        assert!(tool("bash").is_none() && tool("str_replace_based_edit_tool").is_none());
        if patch {
            assert_eq!(tool("apply_patch").unwrap().kind, ToolKind::Custom { format: Some(serde_json::json!({"type":"text"})) });
            assert!(tool("edit").is_none());
        } else {
            assert_eq!(tool("edit").unwrap().kind, ToolKind::Function);
            assert!(tool("apply_patch").is_none());
        }
        assert!(first.system[0].contains("don't cd into it"));
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn old_provider_defined_calls_replay_as_our_tools() {
    let raw = |kind: &str| Some(Opaque { provider: "test".into(), model: "test".into(), data: serde_json::json!({"type":kind}) });
    let old = |id: &str, name: &str, input: serde_json::Value, opaque| StreamEvent::PartDone {
        part: Part::ToolCall { call_id: id.into(), name: name.into(), input, kind: CallKind::Function, opaque },
    };
    let dir = tempfile::tempdir().unwrap();
    let model = with_caps(
        Caps { tool_family: ToolFamily::Openai, custom_tools: true, ..Default::default() },
        vec![
            vec![
                old("b", "bash", serde_json::json!({"command":"echo hi"}), None),
                old("v", "str_replace_based_edit_tool", serde_json::json!({"command":"view","path":"a.txt","view_range":[2,3]}), None),
                old("s", "shell", serde_json::json!({"commands":["ls","pwd"]}), raw("shell_call")),
                old("p", "apply_patch", serde_json::json!({"type":"update_file","path":"a.py","diff":"@@\n-x\n+y\n"}), raw("apply_patch_call")),
                StreamEvent::Finish { reason: FinishReason::ToolUse, response_id: None },
            ],
            text("done"),
        ],
    );
    let (runtime, s, events) = turn(dir.path(), model.clone(), SessionDefaults::default()).await;
    // Nothing runs under an old name; the history then shows our calls.
    assert!(!dir.path().join("a.py").exists());
    assert_eq!(tool_calls(&events), ["bash", "str_replace_based_edit_tool", "shell", "apply_patch"]);
    let unknown = &results(&s.view().await.unwrap().items)[0];
    assert!(unknown.contains("unknown tool: bash. Available tools: ") && unknown.contains("shell"), "{unknown}");
    let calls = |items: &[Item]| -> Vec<(String, serde_json::Value, CallKind, bool)> {
        items
            .iter()
            .flat_map(|i| &i.parts)
            .filter_map(|p| match p {
                Part::ToolCall { name, input, kind, opaque, .. } => Some((name.clone(), input.clone(), *kind, opaque.is_some())),
                _ => None,
            })
            .collect()
    };
    let expected = vec![
        ("shell".to_owned(), serde_json::json!({"command":"echo hi"}), CallKind::Function, false),
        ("read".to_owned(), serde_json::json!({"path":"a.txt","offset":2,"limit":2}), CallKind::Function, false),
        ("shell".to_owned(), serde_json::json!({"command":"ls\npwd"}), CallKind::Function, false),
        ("apply_patch".to_owned(), serde_json::json!("*** Begin Patch\n*** Update File: a.py\n@@\n-x\n+y\n*** End Patch"), CallKind::Custom, false),
    ];
    assert_eq!(calls(&model.requests.lock().unwrap()[1].items), expected);
    assert_eq!(calls(&s.view().await.unwrap().items), expected);
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
