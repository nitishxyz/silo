use otto_core::*;
use otto_llm::*;
use otto_proto::{ApprovalDecision, Delivery, Event, InputPart};
use otto_store::Store;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct DeferredTool(otto_tools::ToolSpec);
#[async_trait::async_trait]
impl otto_tools::Tool for DeferredTool {
    fn spec(&self) -> &otto_tools::ToolSpec {
        &self.0
    }
    async fn call(&self, _: serde_json::Value, _: otto_tools::ToolCx) -> std::result::Result<otto_tools::ToolOutput, otto_tools::ToolError> {
        Ok(otto_tools::ToolOutput::text("deferred executed"))
    }
}
fn deferred_tool() -> Arc<dyn otto_tools::Tool> {
    Arc::new(DeferredTool(
        serde_json::from_value(serde_json::json!({
            "name":"browser_test", "description":"Inspect a page. More detail not in the index.",
            "input_schema":{"type":"object"}, "loading":"deferred", "effects":{"read":true}
        }))
        .unwrap(),
    ))
}

#[tokio::test]
async fn legacy_deferred_grant_gets_loader_without_expanding_grant() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let project = store.create_project(dir.path().to_str().unwrap()).unwrap();
    let row = store
        .create_session(otto_store::NewSession {
            project_id: project.id,
            parent_id: None,
            agent: "build".into(),
            model: "test".into(),
            provider: "test".into(),
            title: String::new(),
            cwd: dir.path().to_string_lossy().into(),
            meta: serde_json::json!({"tools":["browser_test"]}),
        })
        .unwrap();
    let model = ScriptedModel::new(vec![vec![call("load", "load_tools", serde_json::json!({"query":"BROWSER"}))], text("done")]);
    let r = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(store)),
        models: Arc::new(Resolver(model.clone())),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    r.add_tools(vec![deferred_tool()]);
    let s = r.session(&row.id).await.unwrap();
    assert!(r.tools(&s.id).await.unwrap().iter().any(|t| t.name == "load_tools"));
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests[0].tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["load_tools"]);
        assert_eq!(requests[1].tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["browser_test"]);
    }
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn deferred_tools_load_next_step_and_survive_resume() {
    for input in [serde_json::json!({"names":["browser_test", "missing"]}), serde_json::json!({"query":"PAGE"})] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let model = ScriptedModel::new(vec![vec![call("load", "load_tools", input.clone())], text("done")]);
        let config = |model: Arc<ScriptedModel>| RuntimeConfig {
            store: Arc::new(Mutex::new(Store::open(&path).unwrap())),
            models: Arc::new(Resolver(model)),
            state_dir: dir.path().join("state"),
            user_config_dir: dir.path().join("user"),
        };
        let r = Runtime::new(config(model.clone())).await.unwrap();
        r.add_tools(vec![deferred_tool()]);
        let project = r.open_project(dir.path()).await.unwrap();
        let s = r.create_session(CreateSession { project_id: project.id.clone(), cwd: dir.path().into(), ..Default::default() }).await.unwrap();
        assert!(r.tools(&s.id).await.unwrap().iter().any(|t| t.name == "browser_test"));
        let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
        prompt(&s).await;
        finish(&mut live).await;
        {
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(!requests[0].tools.iter().any(|t| t.name == "browser_test"));
            let loader = requests[0].tools.iter().find(|t| t.name == "load_tools").unwrap();
            assert!(loader.description.contains("browser_test: Inspect a page."));
            assert!(!loader.description.contains("More detail"));
            assert!(requests[1].tools.iter().any(|t| t.name == "browser_test"));
            assert!(!requests[1].tools.iter().any(|t| t.name == "load_tools"));
        }
        if input.get("names").is_some() {
            let view = s.view().await.unwrap();
            assert!(serde_json::to_string(&view.items).unwrap().contains("Unknown or ungranted deferred tools: missing"));
            assert!(!view.items.iter().flat_map(|i| &i.parts).any(|p| matches!(p, Part::ToolResult { is_error: true, .. })));
        }
        let child = r
            .create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), parent_id: Some(s.id.clone()), ..Default::default() })
            .await
            .unwrap();
        assert!(r.tools(&child.id).await.unwrap().iter().any(|t| t.name == "load_tools"));
        r.shutdown().await.unwrap();
        drop(r);
        let resumed_model = ScriptedModel::new(vec![text("resumed")]);
        let r = Runtime::new(config(resumed_model.clone())).await.unwrap();
        r.add_tools(vec![deferred_tool()]);
        let s = r.session(&s.id).await.unwrap();
        let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
        prompt(&s).await;
        finish(&mut live).await;
        assert!(resumed_model.requests.lock().unwrap()[0].tools.iter().any(|t| t.name == "browser_test"));
        assert!(!r.tools(&s.id).await.unwrap().iter().any(|t| t.name == "load_tools"));
        r.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn unloaded_deferred_tool_still_executes() {
    let (dir, r, original, model) = setup(vec![vec![call("direct", "browser_test", serde_json::json!({}))], text("done")], "build").await;
    r.add_tools(vec![deferred_tool()]);
    let s = r
        .create_session(CreateSession { project_id: original.view().await.unwrap().info.project_id, cwd: dir.path().into(), ..Default::default() })
        .await
        .unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    assert!(serde_json::to_string(&s.view().await.unwrap().items).unwrap().contains("deferred executed"));
    assert!(model.requests.lock().unwrap().iter().all(|r| !r.tools.iter().any(|t| t.name == "browser_test")));
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_mode_change_applies_to_next_call_and_is_journaled() {
    let (dir, r, s, _) = setup(
        vec![
            vec![call("q", "ask_user", serde_json::json!({"question":"Ready?"}))],
            vec![call("w", "write", serde_json::json!({"path":"blocked","content":"no"}))],
            text("done"),
        ],
        "build",
    )
    .await;
    r.set_approval_mode(&s.id, otto_proto::ApprovalMode::Auto).await.unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    loop {
        if let Event::QuestionAsked { question_id, .. } = live.recv().await.unwrap().1 {
            r.set_approval_mode(&s.id, otto_proto::ApprovalMode::Plan).await.unwrap();
            assert_eq!(s.view().await.unwrap().info.approval_mode, otto_proto::ApprovalMode::Plan);
            s.answer_question(&question_id, "yes".into()).await.unwrap();
            break;
        }
    }
    finish(&mut live).await;
    assert!(!dir.path().join("blocked").exists());
    let (events, _) = r.subscribe(&s.id, 0).await.unwrap();
    assert!(events.iter().any(|(_, e)| matches!(e,Event::SessionChanged{info} if info.approval_mode==otto_proto::ApprovalMode::Plan)));
    let project = s.view().await.unwrap().info.project_id;
    let plan = r
        .create_session(CreateSession {
            project_id: project,
            cwd: dir.path().into(),
            agent: "plan".into(),
            approval_mode: Some(otto_proto::ApprovalMode::Auto),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(plan.view().await.unwrap().info.approval_mode, otto_proto::ApprovalMode::Plan);
    assert!(r.set_approval_mode(&plan.id, otto_proto::ApprovalMode::Auto).await.is_err());
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn fresh_runtime_defaults_to_auto_write() {
    assert_eq!(otto_tools::policy::ApprovalMode::default(), otto_tools::policy::ApprovalMode::Auto);
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModel::new(vec![vec![call("write", "write", serde_json::json!({"path":"auto-file","content":"ok"}))], text("done")]);
    let r = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        models: Arc::new(Resolver(model)),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    let project = r.open_project(dir.path()).await.unwrap();
    let s = r.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    assert_eq!(std::fs::read_to_string(dir.path().join("auto-file")).unwrap(), "ok");
    let (replay, _) = r.subscribe(&s.id, 0).await.unwrap();
    assert!(!replay.iter().any(|(_, event)| matches!(event, Event::ApprovalRequested { .. })));
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn auto_keeps_deny_rules_without_prompting() {
    let (dir, r, s, _) = setup(vec![vec![call("w", "write", serde_json::json!({"path":"denied","content":"bad"}))], text("done")], "build").await;
    std::fs::create_dir_all(dir.path().join(".otto")).unwrap();
    std::fs::write(dir.path().join(".otto/rules.json"), r#"[{"tool":"write","pattern":null,"decision":"deny"}]"#).unwrap();
    r.set_config(SessionDefaults::default()).unwrap();
    let project = s.view().await.unwrap().info.project_id;
    let s = r.create_session(CreateSession { project_id: project, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    assert!(!dir.path().join("denied").exists());
    let (replay, _) = r.subscribe(&s.id, 0).await.unwrap();
    assert!(!replay.iter().any(|(_, e)| matches!(e, Event::ApprovalRequested { .. })));
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn approved_symlink_swap_is_rejected() {
    let (dir, r, s, _) =
        setup(vec![vec![call("w", "write", serde_json::json!({"path":"link/file","content":"unsafe"}))], text("done")], "build").await;
    let outside = tempfile::tempdir().unwrap();
    let inside = dir.path().join("inside");
    std::fs::create_dir(&inside).unwrap();
    std::os::unix::fs::symlink(&inside, dir.path().join("link")).unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    loop {
        if let Event::ApprovalRequested { approval_id, input, .. } = live.recv().await.unwrap().1 {
            assert_eq!(input["original_input"]["path"], "link/file");
            assert_eq!(input["resolved_paths"][0]["path"], std::fs::canonicalize(&inside).unwrap().join("file").to_string_lossy().as_ref());
            assert!(!input.to_string().contains("__otto_paths"));
            std::fs::remove_file(dir.path().join("link")).unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
            s.answer_approval(&approval_id, ApprovalDecision::Allow).await.unwrap();
            break;
        }
    }
    finish(&mut live).await;
    let view = s.view().await.unwrap();
    assert!(serde_json::to_string(&view.items).unwrap().contains("Approved path changed before execution"));
    assert!(!outside.path().join("file").exists());
    assert!(!inside.join("file").exists());
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn shell_spills_under_session_and_archive_removes_outputs() {
    let (dir, r, s, _) = setup(vec![vec![call("shell", "shell", serde_json::json!({"command":"printf spill-test"}))], text("done")], "build").await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    loop {
        if let Event::ApprovalRequested { approval_id, .. } = live.recv().await.unwrap().1 {
            s.answer_approval(&approval_id, ApprovalDecision::Allow).await.unwrap();
            break;
        }
    }
    finish(&mut live).await;
    let (replay, _) = r.subscribe(&s.id, 0).await.unwrap();
    let output = replay
        .iter()
        .find_map(|(_, e)| match e {
            Event::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();
    let spill = std::path::PathBuf::from(output["metadata"]["spill_path"].as_str().unwrap());
    let session_dir = dir.path().join("state/outputs").join(&s.id);
    assert!(spill.starts_with(&session_dir));
    assert_eq!(std::fs::read_to_string(&spill).unwrap(), "spill-test");
    r.archive(&s.id, true).await.unwrap();
    assert!(!session_dir.exists());
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_tool_schema_input_is_error_result() {
    let (dir, r, s, _) = setup(vec![vec![call("bad", "write", serde_json::json!({"path":"file","content":42}))], text("done")], "build").await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    assert!(!dir.path().join("file").exists());
    let items = s.view().await.unwrap().items;
    assert!(serde_json::to_string(&items).unwrap().contains("invalid tool input"));
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn remaining_parallel_approval_keeps_waiting_status() {
    let (_dir, r, s, _) = setup(
        vec![
            vec![
                call("a", "web_fetch", serde_json::json!({"url":"https://example.invalid"})),
                call("b", "web_fetch", serde_json::json!({"url":"https://example.invalid"})),
            ],
            text("done"),
        ],
        "build",
    )
    .await;
    // AskAll gates these calls before any network operation.
    r.set_config(SessionDefaults { approval_mode: Some(otto_tools::policy::ApprovalMode::AskAll), ..Default::default() }).unwrap();
    let info = s.view().await.unwrap().info;
    let s = r.create_session(CreateSession { project_id: info.project_id.clone(), cwd: info.cwd.into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    let mut ids = Vec::new();
    while ids.len() < 2 {
        if let Event::ApprovalRequested { approval_id, .. } = live.recv().await.unwrap().1 {
            ids.push(approval_id);
        }
    }
    s.answer_approval(&ids[0], ApprovalDecision::Deny).await.unwrap();
    assert_eq!(
        r.list_sessions(&info.project_id, false).await.unwrap().iter().find(|i| i.id == s.id).unwrap().status,
        otto_proto::SessionStatus::WaitingApproval
    );
    s.answer_approval(&ids[1], ApprovalDecision::Deny).await.unwrap();
    finish(&mut live).await;
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn server_discovery_defaults_and_fork_replay() {
    let (dir, r, s, _) = setup(vec![text("answer")], "build").await;
    let projects = r.list_projects().await.unwrap();
    assert_eq!(projects.len(), 1);
    assert!(r.agents(Some(dir.path())).iter().any(|a| a.name == "plan"));
    let tools = r.tools(&s.id).await.unwrap();
    assert!(tools.iter().any(|t| t.name == "write" && t.effects.contains(&"write".into())));
    assert!(tools.iter().any(|t| t.name == "update_todos"));
    assert!(!r.busy());
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    assert!(r.busy());
    finish(&mut live).await;
    let view = s.view().await.unwrap();
    let fork = r.fork(&s.id, view.seq).await.unwrap();
    let (replay, _) = r.subscribe(&fork.id, 0).await.unwrap();
    assert_eq!(replay.len(), view.items.len());
    assert!(replay.iter().all(|(_, e)| matches!(e, Event::ItemDone { .. })));
    assert!(replay.windows(2).all(|p| p[1].0 == p[0].0 + 1));
    r.set_config(SessionDefaults {
        agent: Some("general".into()),
        model: Some("changed-model".into()),
        approval_mode: Some(otto_tools::policy::ApprovalMode::AskAll),
        ..Default::default()
    })
    .unwrap();
    let next = r.create_session(CreateSession { project_id: projects[0].id.clone(), cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let info = next.view().await.unwrap().info;
    assert_eq!(info.agent, "general");
    assert_eq!(info.model, "changed-model");
    assert_eq!(s.view().await.unwrap().info.model, "test");
    r.shutdown().await.unwrap();
    assert!(!r.busy());
}

#[tokio::test]
async fn all_crash_windows_reconciled_at_startup() {
    for point in ["not_started", "unknown", "outcome"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let mut store = Store::open(&path).unwrap();
        let project = store.create_project(dir.path().to_str().unwrap()).unwrap();
        let session = store
            .create_session(otto_store::NewSession {
                project_id: project.id,
                parent_id: None,
                agent: "build".into(),
                model: "test".into(),
                provider: "test".into(),
                title: String::new(),
                cwd: dir.path().to_string_lossy().into(),
                meta: serde_json::Value::Null,
            })
            .unwrap();
        let item = Item::new(
            "assistant",
            Role::Assistant,
            vec![Part::ToolCall { call_id: "c".into(), name: "write".into(), input: serde_json::json!({}), kind: CallKind::Function, opaque: None }],
        );
        let event = || otto_store::EventRecord { turn: 1, step: 1, kind: "test".into(), payload: serde_json::Value::Null };
        store.append(&session.id, event(), otto_store::Projection::PutItem(item)).unwrap();
        if point != "not_started" {
            store
                .append(
                    &session.id,
                    event(),
                    otto_store::Projection::ToolIntent { call_id: "c".into(), name: "write".into(), input: serde_json::json!({}) },
                )
                .unwrap();
        }
        if point == "unknown" {
            store.recover_all().unwrap();
        }
        if point == "outcome" {
            store
                .append(
                    &session.id,
                    event(),
                    otto_store::Projection::ToolOutcome {
                        call_id: "c".into(),
                        status: otto_store::ToolStatus::Done,
                        output: serde_json::to_value(otto_tools::ToolOutput {
                            content: vec![
                                otto_tools::OutputPart::Text { text: "completed output".into() },
                                otto_tools::OutputPart::Image { mime: "image/png".into(), data_base64: "aW1hZ2U=".into() },
                            ],
                            ..Default::default()
                        })
                        .unwrap(),
                    },
                )
                .unwrap();
        }
        drop(store);
        let store = Arc::new(Mutex::new(Store::open(path).unwrap()));
        let model = ScriptedModel::new(vec![]);
        let runtime = Runtime::new(RuntimeConfig {
            store: store.clone(),
            models: Arc::new(Resolver(model.clone())),
            state_dir: dir.path().into(),
            user_config_dir: dir.path().join("user"),
        })
        .await
        .unwrap();
        let items = store.lock().unwrap().items(&session.id).unwrap();
        let result = items.last().unwrap();
        assert!(matches!(&result.parts[0],Part::ToolResult{call_id,..}if call_id=="c"));
        let text = serde_json::to_string(result).unwrap();
        assert!(text.contains(if point == "outcome" {
            "completed output"
        } else if point == "not_started" {
            "not started"
        } else {
            "outcome unknown"
        }));
        if point == "outcome" {
            assert!(matches!(&result.parts[0],Part::ToolResult{content,..} if content.iter().any(|p|matches!(p,ResultPart::Image(_)))));
        }
        assert!(model.requests.lock().unwrap().is_empty());
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn root_hook_denies_from_nested_cwd() {
    let (dir, r, parent, m) =
        setup(vec![vec![call("w", "write", serde_json::json!({"path":"blocked","content":"bad"}))], text("done")], "build").await;
    std::fs::create_dir_all(dir.path().join(".otto")).unwrap();
    std::fs::write(dir.path().join(".otto/hooks.json"),serde_json::json!({"hooks":{"PreToolUse":[{"matcher":"write","hooks":[{"type":"command","command":"printf '%s' '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\"}}'"}]}]}}).to_string()).unwrap();
    let cwd = dir.path().join("nested");
    std::fs::create_dir(&cwd).unwrap();
    let project = parent.view().await.unwrap().info.project_id;
    let s = r.create_session(CreateSession { project_id: project, cwd: cwd.clone(), ..Default::default() }).await.unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    assert!(!cwd.join("blocked").exists());
    assert_eq!(m.requests.lock().unwrap().len(), 2);
    assert!(m.requests.lock().unwrap()[0].system[0].contains(&format!("cwd: {}", cwd.display())));
    assert!(matches!(&s.view().await.unwrap().items[2].parts[0], Part::ToolResult { is_error: true, .. }));
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn hook_modifies_input_and_command_expands() {
    let (dir, r, s, m) = setup(vec![vec![call("r", "read", serde_json::json!({"path":"missing"}))], text("done")], "build").await;
    std::fs::create_dir_all(dir.path().join(".otto/commands")).unwrap();
    std::fs::write(dir.path().join(".otto/commands/check.md"), "Check $ARGUMENTS").unwrap();
    std::fs::write(dir.path().join("good"), "modified input worked").unwrap();
    let response =
        serde_json::json!({"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":{"path":"good"},"additionalContext":"hook context"}});
    let command = format!("printf '%s' '{}'", response);
    std::fs::write(
        dir.path().join(".otto/hooks.json"),
        serde_json::json!({"hooks":{"PreToolUse":[{"matcher":"read","hooks":[{"type":"command","command":command}]}]}}).to_string(),
    )
    .unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    s.prompt(vec![InputPart::Text { text: "/check files".into() }], Delivery::Queue).await.unwrap();
    finish(&mut live).await;
    {
        let requests = m.requests.lock().unwrap();
        assert_eq!(requests[0].items[0].text(), "Check files");
        let transcript = serde_json::to_string(&requests[1].items).unwrap();
        assert!(transcript.contains("modified input worked"));
        assert!(transcript.contains("hook context"));
    }
    r.shutdown().await.unwrap();
}

struct ScriptedModel {
    info: ModelInfo,
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<Request>>,
}
impl ScriptedModel {
    fn new(scripts: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            info: ModelInfo { id: "test".into(), provider: "test".into(), name: "test".into(), caps: Caps::default(), pricing: None },
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(vec![]),
        })
    }
}
#[async_trait::async_trait]
impl Model for ScriptedModel {
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
#[tokio::test]
async fn delegate_tool_delivers_outbox() {
    let (_dir, r, s, _) = setup(
        vec![
            vec![call("child", "subagent", serde_json::json!({"action":"delegate","task":"inspect project"}))],
            text("done"),
            text("done"),
            text("done"),
        ],
        "build",
    )
    .await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    loop {
        if let Event::ApprovalRequested { approval_id, .. } = live.recv().await.unwrap().1 {
            s.answer_approval(&approval_id, ApprovalDecision::Allow).await.unwrap();
            break;
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let view = s.view().await.unwrap();
            if view.items.iter().any(|i| i.parts.iter().any(|p| matches!(p,Part::System{text}if text.contains("Subagent completion")))) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn question_and_steer_at_boundary() {
    let (_dir, r, s, m) =
        setup(vec![vec![call("q", "ask_user", serde_json::json!({"question":"Which?","options":["one","two"]}))], text("done")], "build").await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    loop {
        if let Event::QuestionAsked { question_id, .. } = live.recv().await.unwrap().1 {
            assert_eq!(s.view().await.unwrap().pending_questions.len(), 1);
            s.prompt(vec![InputPart::Text { text: "steering instruction".into() }], Delivery::Steer).await.unwrap();
            s.answer_question(&question_id, "two".into()).await.unwrap();
            break;
        }
    }
    finish(&mut live).await;
    let requests = m.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].items.last().unwrap().text(), "steering instruction");
}

#[tokio::test]
async fn ephemeral_deltas_and_durable_coalescing() {
    let mut script = vec![];
    for _ in 0..10 {
        script.push(StreamEvent::TextDelta { text: "x".into() });
    }
    script.push(StreamEvent::PartDone { part: Part::text("xxxxxxxxxx") });
    let (_dir, r, s, _) = setup(vec![script], "build").await;
    let mut deltas = s.deltas();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    for _ in 0..10 {
        assert!(matches!(deltas.try_recv().unwrap(), Event::TextDelta { .. }));
    }
    let (replay, _) = r.subscribe(&s.id, 0).await.unwrap();
    let text = replay
        .iter()
        .filter_map(|(_, e)| match e {
            Event::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "xxxxxxxxxx");
    r.shutdown().await.unwrap();
}

struct Resolver(Arc<ScriptedModel>);
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
    vec![
        StreamEvent::TextDelta { text: text.into() },
        StreamEvent::PartDone { part: Part::text(text) },
        StreamEvent::Finish { reason: FinishReason::Stop, response_id: None },
    ]
}
fn call(id: &str, name: &str, input: serde_json::Value) -> StreamEvent {
    StreamEvent::PartDone { part: Part::ToolCall { call_id: id.into(), name: name.into(), input, kind: CallKind::Function, opaque: None } }
}
async fn setup(scripts: Vec<Vec<StreamEvent>>, agent: &str) -> (tempfile::TempDir, Runtime, SessionHandle, Arc<ScriptedModel>) {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModel::new(scripts);
    let runtime = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        models: Arc::new(Resolver(model.clone())),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    runtime.set_config(SessionDefaults { approval_mode: Some(otto_tools::policy::ApprovalMode::AskOnEffects), ..Default::default() }).unwrap();
    let project = runtime.open_project(dir.path()).await.unwrap();
    let session = runtime
        .create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), agent: agent.into(), ..Default::default() })
        .await
        .unwrap();
    (dir, runtime, session, model)
}
async fn finish(live: &mut LiveEvents) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(live.recv().await.unwrap().1, Event::TurnFinished { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
}
async fn prompt(session: &SessionHandle) {
    session.prompt(vec![InputPart::Text { text: "hello".into() }], Delivery::Queue).await.unwrap();
}
#[tokio::test]
async fn plain_text_and_replay() {
    let (_dir, r, s, m) = setup(vec![text("hello back")], "build").await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    let view = s.view().await.unwrap();
    assert_eq!(view.items.len(), 2);
    assert_eq!(view.items[1].text(), "hello back");
    assert_eq!(m.requests.lock().unwrap().len(), 1);
    let (replay, _) = r.subscribe(&s.id, 0).await.unwrap();
    assert!(replay.windows(2).all(|p| p[1].0 == p[0].0 + 1));
    assert_eq!(replay.last().unwrap().0, view.seq);
}
#[tokio::test]
async fn parallel_calls_preserve_order() {
    let (_dir, r, s, m) =
        setup(vec![vec![call("a", "ls", serde_json::json!({"path":"."})), call("b", "ls", serde_json::json!({"path":"."}))], text("done")], "build")
            .await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    let view = s.view().await.unwrap();
    assert_eq!(view.items.iter().map(|i| i.role).collect::<Vec<_>>(), vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
    let results = &view.items[2].parts;
    assert!(matches!(&results[0],Part::ToolResult{call_id,..}if call_id=="a"));
    assert!(matches!(&results[1],Part::ToolResult{call_id,..}if call_id=="b"));
    assert_eq!(m.requests.lock().unwrap()[1].items.len(), 3);
}
#[tokio::test]
async fn approval_allow_and_deny() {
    for decision in [ApprovalDecision::Allow, ApprovalDecision::Deny] {
        let (dir, r, s, _) = setup(vec![vec![call("w", "write", serde_json::json!({"path":"file","content":"ok"}))], text("done")], "build").await;
        let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
        prompt(&s).await;
        loop {
            if let Event::ApprovalRequested { approval_id, .. } = live.recv().await.unwrap().1 {
                let view = s.view().await.unwrap();
                assert_eq!(view.pending_approvals.len(), 1);
                let listed = r.list_sessions(&view.info.project_id, false).await.unwrap();
                assert_eq!(listed.iter().find(|i| i.id == s.id).unwrap().status, otto_proto::SessionStatus::WaitingApproval);
                r.answer_approval_with_note(&s.id, &approval_id, decision, Some("reviewed scope".into())).await.unwrap();
                break;
            }
        }
        finish(&mut live).await;
        assert_eq!(dir.path().join("file").exists(), decision == ApprovalDecision::Allow);
        assert!(serde_json::to_string(&s.view().await.unwrap().items).unwrap().contains("reviewed scope"));
    }
}
#[tokio::test]
async fn plan_blocks_writes() {
    let (dir, r, s, _) = setup(vec![vec![call("w", "write", serde_json::json!({"path":"file","content":"bad"}))], text("done")], "plan").await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    assert!(!dir.path().join("file").exists());
    assert!(matches!(&s.view().await.unwrap().items[2].parts[0], Part::ToolResult { is_error: true, .. }));
}
#[tokio::test]
async fn cancel_pending_stream() {
    let (_dir, r, s, _) = setup(vec![], "build").await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    loop {
        if matches!(live.recv().await.unwrap().1, Event::ItemStarted { .. }) {
            break;
        }
    }
    s.cancel().await.unwrap();
    finish(&mut live).await;
}
#[tokio::test]
async fn compaction_trims_history() {
    let (_dir, r, s, m) = setup(vec![text("first"), text("checkpoint"), text("second")], "build").await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    s.compact().await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    let requests = m.requests.lock().unwrap();
    assert_eq!(requests[2].items.len(), 2);
    assert!(requests[2].items[0].text().contains("checkpoint"));
    assert!(requests[2].items[0].text().contains("hello"));
}
#[tokio::test]
async fn queue_waits_and_interrupt_cancels() {
    let (_dir, r, s, m) = setup(vec![], "build").await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    loop {
        if matches!(live.recv().await.unwrap().1, Event::ItemStarted { .. }) {
            break;
        }
    }
    s.prompt(vec![InputPart::Text { text: "queued".into() }], Delivery::Queue).await.unwrap();
    assert_eq!(s.view().await.unwrap().queue.len(), 1);
    assert_eq!(m.requests.lock().unwrap().len(), 1);
    m.scripts.lock().unwrap().extend([text("interrupted response"), text("queued response")]);
    s.prompt(vec![InputPart::Text { text: "interrupt".into() }], Delivery::Interrupt).await.unwrap();
    finish(&mut live).await;
    finish(&mut live).await;
    finish(&mut live).await;
    let view = s.view().await.unwrap();
    let inputs = view.items.iter().filter(|i| i.role == Role::User).map(Item::text).collect::<Vec<_>>();
    assert_eq!(inputs, vec!["hello", "interrupt", "queued"]);
}

#[tokio::test]
async fn child_completion_wakes_parent() {
    let (dir, r, parent, m) = setup(vec![text("parent first")], "build").await;
    let (_, mut events) = r.subscribe(&parent.id, 0).await.unwrap();
    prompt(&parent).await;
    finish(&mut events).await;
    let view = parent.view().await.unwrap();
    m.scripts.lock().unwrap().extend([text("child result"), text("parent incorporated result")]);
    let child = r
        .create_session(CreateSession {
            project_id: view.info.project_id,
            cwd: dir.path().into(),
            parent_id: Some(parent.id.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    prompt(&child).await;
    finish(&mut events).await;
    let view = parent.view().await.unwrap();
    assert!(view.items.iter().any(|i| i.parts.iter().any(|p| matches!(p,Part::System{text}if text.contains("child result")))));
    assert_eq!(view.items.last().unwrap().text(), "parent incorporated result");
}

#[tokio::test]
async fn recover_unknown_without_rerun() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut store = Store::open(&path).unwrap();
    let project = store.create_project(dir.path().to_str().unwrap()).unwrap();
    let session = store
        .create_session(otto_store::NewSession {
            project_id: project.id,
            parent_id: None,
            agent: "build".into(),
            model: "test".into(),
            provider: "test".into(),
            title: String::new(),
            cwd: dir.path().to_string_lossy().into(),
            meta: serde_json::Value::Null,
        })
        .unwrap();
    store
        .append(
            &session.id,
            otto_store::EventRecord { turn: 1, step: 1, kind: "tool.call".into(), payload: serde_json::json!({}) },
            otto_store::Projection::ToolIntent { call_id: "crashed".into(), name: "write".into(), input: serde_json::json!({}) },
        )
        .unwrap();
    drop(store);
    let model = ScriptedModel::new(vec![]);
    let runtime = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open(path).unwrap())),
        models: Arc::new(Resolver(model.clone())),
        state_dir: dir.path().into(),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    let view = runtime.view(&session.id).await.unwrap();
    assert!(matches!(&view.items[0].parts[0], Part::ToolResult { is_error: true, .. }));
    assert!(serde_json::to_string(&view.items).unwrap().contains("outcome unknown"));
    assert!(model.requests.lock().unwrap().is_empty());
}

/// Rules Anthropic `input_schema` and OpenAI function `parameters` both enforce.
fn schema_problems(name: &str, schema: &serde_json::Value) -> Vec<String> {
    fn refs<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a str>) {
        match v {
            serde_json::Value::Object(map) => map.iter().for_each(|(k, v)| if k == "$ref" { out.extend(v.as_str()) } else { refs(v, out) }),
            serde_json::Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
            _ => {}
        }
    }
    let mut problems = vec![];
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        problems.push(format!("{name}: invalid tool name"));
    }
    if schema["type"] != "object" {
        problems.push(format!("{name}: top-level type must be object"));
    }
    for key in ["oneOf", "anyOf", "allOf", "not", "enum", "const", "if", "then", "else"] {
        if schema.get(key).is_some() {
            problems.push(format!("{name}: top-level {key}"));
        }
    }
    if schema.get("properties").is_some_and(|p| !p.is_object()) {
        problems.push(format!("{name}: properties must be an object"));
    }
    for required in schema["required"].as_array().into_iter().flatten() {
        if schema["properties"].get(required.as_str().unwrap_or_default()).is_none() {
            problems.push(format!("{name}: required {required} is not a property"));
        }
    }
    let mut found = vec![];
    refs(schema, &mut found);
    for r in found {
        if !r.strip_prefix('#').is_some_and(|p| p.is_empty() || schema.pointer(p).is_some()) {
            problems.push(format!("{name}: unresolved $ref {r}"));
        }
    }
    problems
}
#[tokio::test]
async fn every_offered_tool_schema_is_provider_safe() {
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModel::new(vec![vec![call("load", "load_tools", serde_json::json!({"query":"browser"}))], text("done")]);
    let r = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        models: Arc::new(Resolver(model.clone())),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    r.add_tools(otto_silo_tools::tools(otto_silo_tools::Config { home: dir.path().join("silo"), client_name: "otto test".into(), token: None }));
    let project = r.open_project(dir.path()).await.unwrap();
    let s = r.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    let requests = model.requests.lock().unwrap().clone();
    let mut problems = vec![];
    for def in requests.iter().flat_map(|r| &r.tools) {
        problems.extend(schema_problems(&def.name, &def.input_schema));
    }
    for tool in otto_tools::registry::tools_for_family("openai") {
        let spec = tool.spec();
        problems.extend(schema_problems(&spec.name, &spec.input_schema));
        for p in &spec.presentations {
            problems.extend(p.input_schema.as_ref().map(|schema| schema_problems(&p.name, schema)).unwrap_or_default());
        }
    }
    let names = |i: usize| requests[i].tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    for name in ["read", "edit", "skill", "subagent", "load_tools", "terminal_run", "notify"] {
        assert!(names(0).iter().any(|n| n == name), "{name} missing from {:?}", names(0));
    }
    assert!(names(1).iter().any(|n| n == "browser_fill_form"), "{:?}", names(1));
    assert!(problems.is_empty(), "{problems:#?}");
    r.shutdown().await.unwrap();
}
