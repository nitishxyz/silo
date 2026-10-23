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
            // Loaded tools leave the index (the built-in deferred ones stay).
            assert!(!requests[1].tools.iter().any(|t| t.name == "load_tools" && t.description.contains("browser_test")));
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
        assert!(!r.tools(&s.id).await.unwrap().iter().any(|t| t.name == "load_tools" && t.description.contains("browser_test")));
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
        match live.recv().await.unwrap().1 {
            Event::QuestionAsked { question_id, .. } => {
                r.set_approval_mode(&s.id, otto_proto::ApprovalMode::AskOnEffects).await.unwrap();
                assert_eq!(s.view().await.unwrap().info.approval_mode, otto_proto::ApprovalMode::AskOnEffects);
                s.answer_question(&question_id, "yes".into()).await.unwrap();
            }
            Event::ApprovalRequested { approval_id, .. } => {
                s.answer_approval(&approval_id, ApprovalDecision::Deny).await.unwrap();
                break;
            }
            _ => {}
        }
    }
    finish(&mut live).await;
    assert!(!dir.path().join("blocked").exists());
    let (events, _) = r.subscribe(&s.id, 0).await.unwrap();
    assert!(events.iter().any(|(_, e)| matches!(e,Event::SessionChanged{info} if info.approval_mode==otto_proto::ApprovalMode::AskOnEffects)));
    r.shutdown().await.unwrap();
}

/// A session stored by an otto that had the `plan` agent and mode still opens and runs.
#[tokio::test]
async fn an_old_plan_session_opens_as_auto() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let project = store.create_project(dir.path().to_str().unwrap()).unwrap();
    let row = store
        .create_session(otto_store::NewSession {
            project_id: project.id.clone(),
            parent_id: None,
            agent: "plan".into(),
            model: "test".into(),
            provider: "test".into(),
            title: String::new(),
            cwd: dir.path().to_string_lossy().into(),
            meta: serde_json::json!({"tools":["read","write"],"denied_tools":[],"approval_mode":"plan"}),
        })
        .unwrap();
    let model = ScriptedModel::new(vec![vec![call("w", "write", serde_json::json!({"path":"file","content":"ok"}))], text("done")]);
    let r = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(store)),
        models: Arc::new(Resolver(model.clone())),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    let listed = r.list_sessions(&project.id, false).await.unwrap();
    assert_eq!((listed[0].agent.as_str(), listed[0].approval_mode), ("plan", otto_proto::ApprovalMode::Auto));
    let s = r.session(&row.id).await.unwrap();
    assert_eq!(s.view().await.unwrap().info.approval_mode, otto_proto::ApprovalMode::Auto);
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    assert!(dir.path().join("file").exists());
    let mut tools: Vec<_> = model.requests.lock().unwrap()[0].tools.iter().map(|t| t.name.clone()).collect();
    tools.sort();
    // `copy_attachment` comes with `write`, behind `load_tools`.
    assert_eq!(tools, ["load_tools", "read", "write"]);
    let info = r.configure(&s.id, Configure { agent: Some("build".into()), ..Default::default() }).await.unwrap();
    assert_eq!(info.agent, "build");
    r.shutdown().await.unwrap();
}

fn write_agent(dir: &std::path::Path, name: &str, frontmatter: &str) {
    std::fs::create_dir_all(dir.join(".otto/agents")).unwrap();
    std::fs::write(dir.join(format!(".otto/agents/{name}.md")), format!("---\n{frontmatter}\n---\nWork.")).unwrap();
}

/// `tools` are shown, `loadable` wait behind `load_tools`, whatever each tool's own default;
/// an agent listing neither gets every tool with its own default.
#[tokio::test]
async fn agents_choose_shown_and_loadable_tools() {
    let (dir, r, _, model) =
        setup(vec![vec![call("load", "load_tools", serde_json::json!({"names":["search"]}))], text("done"), text("done")], "build").await;
    r.add_tools(vec![deferred_tool()]);
    write_agent(dir.path(), "picky", "tools: read, browser_test, mcp__gh__issue\nloadable: search, mcp__*");
    write_agent(dir.path(), "lazy", "loadable: read");
    let project = r.list_projects().await.unwrap()[0].id.clone();
    let session = |agent: &str| {
        r.create_session(CreateSession { project_id: project.clone(), cwd: dir.path().into(), agent: agent.into(), ..Default::default() })
    };
    let picky = session("picky").await.unwrap();
    let mut granted: Vec<_> = r.tools(&picky.id).await.unwrap().into_iter().map(|t| t.name).collect();
    granted.sort();
    assert_eq!(granted, ["browser_test", "load_tools", "read", "search"]);
    let (_, mut live) = r.subscribe(&picky.id, 0).await.unwrap();
    prompt(&picky).await;
    finish(&mut live).await;
    {
        let requests = model.requests.lock().unwrap();
        let names = |i: usize| {
            let mut names: Vec<_> = requests[i].tools.iter().map(|t| t.name.clone()).collect();
            names.sort();
            names
        };
        assert_eq!(names(0), ["browser_test", "load_tools", "read"]);
        assert!(requests[0].tools.iter().any(|t| t.name == "load_tools" && t.description.contains("search:")));
        assert_eq!(names(1), ["browser_test", "read", "search"]);
    }
    let lazy = session("lazy").await.unwrap();
    let mut granted: Vec<_> = r.tools(&lazy.id).await.unwrap().into_iter().map(|t| t.name).collect();
    granted.sort();
    assert_eq!(granted, ["load_tools", "read"]);
    let build = session("build").await.unwrap();
    let (_, mut live) = r.subscribe(&build.id, 0).await.unwrap();
    prompt(&build).await;
    finish(&mut live).await;
    let names: Vec<_> = model.requests.lock().unwrap()[2].tools.iter().map(|t| t.name.clone()).collect();
    assert!(["read", "subagent", "load_tools"].iter().all(|n| names.iter().any(|m| m == n)));
    assert!(!names.iter().any(|n| n == "browser_test" || n == "web_search"));
    r.shutdown().await.unwrap();
}

/// A child's grant, shown and loadable alike, stays within its parent's.
#[tokio::test]
async fn a_child_agent_is_narrowed_to_its_parent() {
    let (dir, r, _, _) = setup(vec![], "build").await;
    write_agent(dir.path(), "narrow", "tools: read, subagent, web_fetch\ndisallowedTools: shell");
    write_agent(dir.path(), "wide", "tools: read, write, shell\nloadable: search, web_fetch");
    let project = r.list_projects().await.unwrap()[0].id.clone();
    let create = |agent: &str, parent: Option<String>| {
        r.create_session(CreateSession {
            project_id: project.clone(),
            cwd: dir.path().into(),
            agent: agent.into(),
            parent_id: parent,
            ..Default::default()
        })
    };
    let parent = create("narrow", None).await.unwrap();
    let child = create("wide", Some(parent.id.clone())).await.unwrap();
    let mut granted: Vec<_> = r.tools(&child.id).await.unwrap().into_iter().map(|t| t.name).collect();
    granted.sort();
    // `web_search` comes with the `web_fetch` grant.
    assert_eq!(granted, ["load_tools", "read", "web_fetch", "web_search"]);
    r.shutdown().await.unwrap();
}

#[tokio::test]
async fn delegating_needs_a_known_agent() {
    let (_dir, r, s, _) = setup(
        vec![
            vec![call("none", "subagent", serde_json::json!({"action":"delegate","task":"inspect","agent":""}))],
            vec![call("bad", "subagent", serde_json::json!({"action":"delegate","task":"inspect","agent":"general"}))],
            text("done"),
        ],
        "build",
    )
    .await;
    r.set_approval_mode(&s.id, otto_proto::ApprovalMode::Auto).await.unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    finish(&mut live).await;
    let items = serde_json::to_string(&s.view().await.unwrap().items).unwrap();
    assert!(items.contains("agent required; available: build"), "{items}");
    assert!(items.contains("unknown agent general; available: build"), "{items}");
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
    assert_eq!(r.agents(Some(dir.path())).iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["build"]);
    std::fs::create_dir_all(dir.path().join("user/agents")).unwrap();
    std::fs::write(dir.path().join("user/agents/helper.md"), "---\ndescription: Helps\n---\nHelp.").unwrap();
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
        agent: Some("helper".into()),
        model: Some("changed-model".into()),
        approval_mode: Some(otto_tools::policy::ApprovalMode::AskAll),
        ..Default::default()
    })
    .unwrap();
    let next = r.create_session(CreateSession { project_id: projects[0].id.clone(), cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    let info = next.view().await.unwrap().info;
    assert_eq!(info.agent, "helper");
    assert_eq!(info.model, "changed-model");
    assert_eq!(s.view().await.unwrap().info.model, "test");
    r.shutdown().await.unwrap();
    assert!(!r.busy());
}

#[tokio::test]
async fn default_effort_applies_unless_the_agent_or_session_sets_one() {
    let (dir, r, _s, _) = setup(vec![], "build").await;
    let project = r.list_projects().await.unwrap()[0].id.clone();
    std::fs::create_dir_all(dir.path().join("user/agents")).unwrap();
    std::fs::write(dir.path().join("user/agents/thinker.md"), "---\ndescription: Thinks\neffort: high\n---\nThink.").unwrap();
    r.set_config(SessionDefaults { effort: Some(otto_llm::Effort::Low), ..Default::default() }).unwrap();
    let create = |agent: &str, effort| CreateSession {
        project_id: project.clone(),
        cwd: dir.path().into(),
        agent: agent.into(),
        effort,
        ..Default::default()
    };
    let plain = r.create_session(create("build", None)).await.unwrap();
    assert_eq!(plain.view().await.unwrap().info.effort, Some(otto_llm::Effort::Low));
    let thinker = r.create_session(create("thinker", None)).await.unwrap();
    assert_eq!(thinker.view().await.unwrap().info.effort, Some(otto_llm::Effort::High));
    let chosen = r.create_session(create("thinker", Some(otto_llm::Effort::Max))).await.unwrap();
    assert_eq!(chosen.view().await.unwrap().info.effort, Some(otto_llm::Effort::Max));
    r.set_config(SessionDefaults::default()).unwrap();
    assert_eq!(r.create_session(create("build", None)).await.unwrap().view().await.unwrap().info.effort, None);
    r.shutdown().await.unwrap();
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
            vec![call("child", "subagent", serde_json::json!({"action":"delegate","task":"inspect project","agent":"build"}))],
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
async fn background_shell_result_is_delivered_to_the_session() {
    let script = vec![call("bg", "shell", serde_json::json!({"command":"sleep 0.2; printf bg-done","background":true}))];
    let (_dir, r, s, _) = setup(vec![script, text("started"), text("noted"), text("noted")], "build").await;
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    loop {
        if let Event::ApprovalRequested { approval_id, .. } = live.recv().await.unwrap().1 {
            s.answer_approval(&approval_id, ApprovalDecision::Allow).await.unwrap();
            break;
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let view = s.view().await.unwrap();
            let delivered = |text: &str| text.starts_with("Background shell job-") && text.contains("exited with code 0.\nbg-done");
            if view.items.iter().any(|i| i.parts.iter().any(|p| matches!(p, Part::System { text } if delivered(text)))) {
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

/// Streams "draft", waits for steering (native) or a go signal (boundary),
/// then finishes. Natively steered input continues the same call in "revised".
struct SteerModel {
    info: ModelInfo,
    native: bool,
    started: tokio::sync::Notify,
    signal: tokio::sync::mpsc::UnboundedSender<String>,
    inbox: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<String>>>,
    steered: Mutex<Vec<(String, Vec<Part>)>>,
    requests: Mutex<Vec<Request>>,
}
#[async_trait::async_trait]
impl Model for SteerModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    async fn stream(&self, request: Request, _cx: CallCx) -> std::result::Result<EventStream, LlmError> {
        use futures::StreamExt;
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len() == 1
        };
        if !first {
            return Ok(Box::pin(futures::stream::iter(text("revised").into_iter().map(Ok))));
        }
        self.started.notify_one();
        let (inbox, native) = (self.inbox.clone(), self.native);
        let rest = futures::stream::once(async move { inbox.lock().await.recv().await.unwrap() }).flat_map(move |id| {
            let mut events =
                vec![StreamEvent::PartDone { part: Part::text("draft") }, StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }];
            if native {
                events.push(StreamEvent::Steered { ids: vec![id] });
                events.extend(text("revised"));
            }
            futures::stream::iter(events)
        });
        Ok(Box::pin(futures::stream::once(async { StreamEvent::TextDelta { text: "dra".into() } }).chain(rest).map(Ok)))
    }
    fn session(&self) -> Option<&dyn ModelSession> {
        self.native.then_some(self as &dyn ModelSession)
    }
}
#[async_trait::async_trait]
impl ModelSession for SteerModel {
    async fn steer(&self, id: &str, input: Vec<Part>) -> std::result::Result<bool, LlmError> {
        self.steered.lock().unwrap().push((id.into(), input));
        Ok(self.signal.send(id.into()).is_ok())
    }
}
struct AnyResolver(Arc<dyn Model>);
impl ModelResolver for AnyResolver {
    fn resolve(&self, _: &str, _: &str) -> Result<Arc<dyn Model>> {
        Ok(self.0.clone())
    }
    fn default_model(&self) -> Option<(String, String)> {
        Some(("test".into(), "test".into()))
    }
    fn list(&self) -> Vec<ModelInfo> {
        vec![self.0.info().clone()]
    }
}
async fn steer_mid_stream(native: bool) -> (Arc<SteerModel>, Vec<Item>) {
    let (signal, inbox) = tokio::sync::mpsc::unbounded_channel();
    let mut info = ModelInfo { id: "test".into(), provider: "test".into(), name: "test".into(), caps: Caps::default(), pricing: None };
    info.caps.steering = native;
    let model = Arc::new(SteerModel {
        info,
        native,
        started: tokio::sync::Notify::new(),
        signal,
        inbox: Arc::new(tokio::sync::Mutex::new(inbox)),
        steered: Mutex::new(vec![]),
        requests: Mutex::new(vec![]),
    });
    let dir = tempfile::tempdir().unwrap();
    let r = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        models: Arc::new(AnyResolver(model.clone())),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    let project = r.open_project(dir.path()).await.unwrap();
    let s = r
        .create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), agent: "build".into(), ..Default::default() })
        .await
        .unwrap();
    let (_, mut live) = r.subscribe(&s.id, 0).await.unwrap();
    prompt(&s).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), model.started.notified()).await.unwrap();
    let steer = s.prompt(vec![InputPart::Text { text: "steering instruction".into() }], Delivery::Steer).await.unwrap();
    if !native {
        while s.view().await.unwrap().queue.iter().all(|q| q.id != steer) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        model.signal.send("go".into()).unwrap();
    }
    finish(&mut live).await;
    let items = s.view().await.unwrap().items;
    r.shutdown().await.unwrap();
    (model, items)
}
#[tokio::test]
async fn steer_goes_native_when_the_model_session_supports_it() {
    let (model, items) = steer_mid_stream(true).await;
    let texts: Vec<_> = items.iter().map(|i| (i.role, i.text())).collect();
    assert_eq!(
        texts,
        [
            (Role::User, "hello".into()),
            (Role::Assistant, "draft".into()),
            (Role::User, "steering instruction".into()),
            (Role::Assistant, "revised".into())
        ]
    );
    assert_eq!(model.requests.lock().unwrap().len(), 1, "the steer continued the same model call");
    let steered = model.steered.lock().unwrap();
    assert_eq!(steered.len(), 1);
    assert_eq!(steered[0].0, items[2].id);
    assert_eq!(steered[0].1, vec![Part::text("steering instruction")]);
}
#[tokio::test]
async fn steer_waits_for_the_step_boundary_without_native_steering() {
    let (model, items) = steer_mid_stream(false).await;
    let texts: Vec<_> = items.iter().map(|i| (i.role, i.text())).collect();
    assert_eq!(
        texts,
        [
            (Role::User, "hello".into()),
            (Role::Assistant, "draft".into()),
            (Role::User, "steering instruction".into()),
            (Role::Assistant, "revised".into())
        ]
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "the steer went into a new step");
    assert_eq!(requests[1].items.last().unwrap().text(), "steering instruction");
    assert!(model.steered.lock().unwrap().is_empty());
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

#[tokio::test]
async fn background_jobs_of_a_stopped_process_report_stopped() {
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
    let record = || otto_store::EventRecord { turn: 1, step: 1, kind: "tool.call".into(), payload: serde_json::json!({}) };
    for (call, job) in [("a", "job-1"), ("b", "job-2")] {
        store
            .append(
                &session.id,
                record(),
                otto_store::Projection::ToolIntent {
                    call_id: call.into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "cargo test"}),
                },
            )
            .unwrap();
        let output = otto_tools::ToolOutput::text("Started").with_metadata(serde_json::json!({"job_id": job, "background": true}));
        let output = serde_json::to_value(output).unwrap();
        store
            .append(&session.id, record(), otto_store::Projection::ToolOutcome { call_id: call.into(), status: otto_store::ToolStatus::Done, output })
            .unwrap();
    }
    let done = otto_llm::Item::new(
        "r",
        otto_llm::Role::System,
        vec![Part::System { text: "Background shell job-2 (`cargo test`) exited with code 0.\n".into() }],
    );
    store.append(&session.id, record(), otto_store::Projection::PutItem(done)).unwrap();
    drop(store);
    let model = ScriptedModel::new(vec![]);
    let open = || RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open(&path).unwrap())),
        models: Arc::new(Resolver(model.clone())),
        state_dir: dir.path().into(),
        user_config_dir: dir.path().join("user"),
    };
    let stopped = |view: &otto_proto::SessionView| {
        view.items
            .iter()
            .flat_map(|i| &i.parts)
            .filter(|p| matches!(p, Part::System { text } if text.contains("was stopped (the otto server restarted)")))
            .count()
    };
    let runtime = Runtime::new(open()).await.unwrap();
    let view = runtime.view(&session.id).await.unwrap();
    assert!(serde_json::to_string(&view.items).unwrap().contains("Background shell job-1 (`cargo test`) was stopped"));
    assert_eq!(stopped(&view), 1, "only the job that never reported");
    runtime.shutdown().await.unwrap();
    drop(runtime);
    let runtime = Runtime::new(open()).await.unwrap();
    assert_eq!(stopped(&runtime.view(&session.id).await.unwrap()), 1, "once");
    assert!(model.requests.lock().unwrap().is_empty(), "no turn");
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
