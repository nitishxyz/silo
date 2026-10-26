use crate::tests::*;

#[tokio::test]
async fn discovery_defaults_and_device_login_use_runtime_and_auth() {
    let mut f = Fixture::new(vec![]).await;
    let project = f.runtime.open_project(f.dir.path()).await.unwrap();
    std::fs::create_dir_all(f.dir.path().join("agents")).unwrap();
    std::fs::write(f.dir.path().join("agents/custom.md"), "---\nname: custom\ndescription: custom runtime agent\n---\nHelp.").unwrap();
    std::fs::write(f.dir.path().join("config.json"), r#"{"agent":"custom","model":"startup","approval_mode":"ask_all"}"#).unwrap();
    let server = Server::start(f.config()).await.unwrap();
    let client = f.connect().await;
    let projects = client.call(Method::ProjectList {}).await.unwrap();
    assert_eq!(projects[0]["id"], project.id);
    assert!(!f.dir.path().join("projects.json").exists());
    let first = client.session_create(SessionCreateOptions { project_id: Some(project.id), ..Default::default() }).await.unwrap();
    assert_eq!(first.agent, "custom");
    assert_eq!(first.approval_mode, otto_proto::ApprovalMode::AskAll);
    assert_eq!(first.model, "startup");
    let tools = client.call(Method::ToolsList { session_id: Some(first.id.clone()) }).await.unwrap();
    assert!(!tools.as_array().unwrap().is_empty());
    assert!(
        matches!(client.call(Method::ToolsList { session_id: None }).await, Err(otto_client::ClientError::Rpc(RpcError { code, .. })) if code == RpcError::BAD_REQUEST)
    );
    let agents = client.call(Method::AgentsList {}).await.unwrap();
    assert!(agents.as_array().unwrap().iter().any(|a| a["name"] == "custom"));
    client.call(Method::ConfigSet { key: "model".into(), value: json!("changed") }).await.unwrap();
    client.call(Method::ConfigSet { key: "agent".into(), value: json!("build") }).await.unwrap();
    client.call(Method::ConfigSet { key: "approval_mode".into(), value: json!("ask_all") }).await.unwrap();
    assert!(client.call(Method::ConfigSet { key: "approval_mode".into(), value: json!("invalid") }).await.is_err());
    assert_eq!(client.call(Method::ConfigGet { key: Some("approval_mode".into()) }).await.unwrap()["value"], "ask_all");
    let second = f.session(&client).await;
    assert_eq!(second.model, "changed");
    assert_eq!(second.agent, "build");
    assert_eq!(client.session_view(&first.id).await.unwrap().info.model, "startup");
    let pending = client
        .call(Method::AuthLogin {
            provider: "openai".into(),
            mode: AuthMode::Subscription,
            api_key: None,
            device: true,
            base_url: None,
            paste: false,
        })
        .await
        .unwrap();
    assert_eq!(pending["user_code"], "ABCD");
    assert_eq!(pending["interval_secs"], 5);
    assert_eq!(pending["verification_uri"], "https://example.test/device");
    assert!(
        client
            .call(Method::AuthLogin {
                provider: "openai".into(),
                mode: AuthMode::ApiKey,
                api_key: Some("test".into()),
                device: true,
                base_url: None,
                paste: false
            })
            .await
            .is_err()
    );
    // Keyless API-key logins are for local and custom endpoints only.
    let keyless = |base_url: Option<&str>| Method::AuthLogin {
        provider: "ollama".into(),
        mode: AuthMode::ApiKey,
        api_key: None,
        device: false,
        base_url: base_url.map(String::from),
        paste: false,
    };
    let error = client.call(keyless(None)).await.unwrap_err().to_string();
    assert!(error.contains("bad_request"), "{error}");
    let error = client.call(keyless(Some("http://127.0.0.1:9"))).await.unwrap_err().to_string();
    assert!(!error.contains("bad_request"), "{error}");
    server.shutdown().await.unwrap();
    f.reopen().await;
    let restarted = Server::start(f.config()).await.unwrap();
    let client = f.connect().await;
    assert_eq!(f.session(&client).await.model, "changed");
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn agents_and_the_tool_catalog_round_trip_over_the_wire() {
    let f = Fixture::new(vec![]).await;
    let server = Server::start(f.config()).await.unwrap();
    let client = f.connect().await;
    let root = f.dir.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let cwd = Some(root.to_string_lossy().into_owned());
    let agent = AgentDef {
        name: "reviewer".into(),
        description: "Reviews changes".into(),
        model: Some("test/other".into()),
        effort: Some("low".into()),
        approval_mode: Some(ApprovalMode::AskOnEffects),
        prompt: "Review the diff.".into(),
        tools: vec!["read".into()],
        loadable: vec!["search".into(), "mcp__*".into()],
        disallowed: vec!["shell".into()],
        scope: AgentScope::Project,
        overrides_builtin: false,
    };
    assert_eq!(client.agents_save(agent.clone(), None, cwd.clone()).await.unwrap(), agent);
    assert_eq!(client.agents_get("reviewer", cwd.clone()).await.unwrap(), agent);
    assert!(root.join(".otto/agents/reviewer.md").exists());
    let renamed = client.agents_save(AgentDef { name: "critic".into(), ..agent.clone() }, Some("reviewer".into()), cwd.clone()).await.unwrap();
    assert_eq!(renamed.name, "critic");
    let missing = client.agents_get("reviewer", cwd.clone()).await.unwrap_err();
    assert!(matches!(&missing, otto_client::ClientError::Rpc(RpcError { code, .. }) if code == RpcError::NOT_FOUND), "{missing}");
    let user = client.agents_save(AgentDef { scope: AgentScope::User, ..renamed.clone() }, None, None).await.unwrap();
    assert_eq!(user.scope, AgentScope::User);
    let listed = client.agents_list().await.unwrap();
    assert!(listed.iter().any(|a| a.name == "critic" && a.scope == AgentScope::User));
    assert!(listed.iter().any(|a| a.name == "build" && a.scope == AgentScope::Builtin));
    for bad in [
        Method::AgentsSave { agent: AgentDef { name: "../x".into(), ..agent.clone() }, previous_name: None, cwd: cwd.clone() },
        Method::AgentsSave { agent: agent.clone(), previous_name: None, cwd: None },
        Method::AgentsSave { agent: AgentDef { scope: AgentScope::Builtin, ..agent.clone() }, previous_name: None, cwd: cwd.clone() },
        Method::AgentsDelete { name: "build".into(), scope: AgentScope::Builtin, cwd: None },
    ] {
        let error = client.call(bad).await.unwrap_err();
        assert!(matches!(&error, otto_client::ClientError::Rpc(RpcError { code, .. }) if code == RpcError::BAD_REQUEST), "{error}");
        assert!(!error.to_string().contains(&*f.dir.path().to_string_lossy()), "{error}");
    }
    client.agents_delete("critic", AgentScope::Project, cwd.clone()).await.unwrap();
    assert_eq!(client.agents_get("critic", cwd.clone()).await.unwrap().scope, AgentScope::User);
    client.agents_delete("critic", AgentScope::User, None).await.unwrap();
    assert!(client.agents_delete("critic", AgentScope::User, None).await.is_err());
    let catalog = client.tools_catalog(cwd).await.unwrap();
    assert!(catalog.iter().any(|t| t.name == "read" && t.loading == "default" && t.source == "builtin"));
    assert!(catalog.iter().any(|t| t.name == "web_search" && t.loading == "deferred"));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_update_and_recent_projects_over_the_wire() {
    let f = Fixture::new(vec![]).await;
    let server = Server::start(f.config()).await.unwrap();
    let client = f.connect().await;
    let created = client
        .session_create(SessionCreateOptions {
            project_root: Some(f.dir.path().to_string_lossy().into_owned()),
            effort: Some(Effort::Medium),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(created.effort, Some(Effort::Medium));
    let mut events = client.events();
    client.session_subscribe(&created.id, None).await.unwrap();
    std::fs::create_dir_all(f.dir.path().join("agents")).unwrap();
    std::fs::write(f.dir.path().join("agents/careful.md"), "---\napproval_mode: ask_all\n---\nCareful.").unwrap();
    let update = otto_client::SessionUpdate { agent: Some("careful".into()), effort: Some(Effort::High), ..Default::default() };
    let info = client.session_update(&created.id, update).await.unwrap();
    assert_eq!((info.agent.as_str(), info.approval_mode, info.effort), ("careful", ApprovalMode::AskAll, Some(Effort::High)));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerMessage::Event { event, .. } = events.recv().await.unwrap().unwrap()
                && let Event::SessionChanged { info } = *event
                && info.effort == Some(Effort::High)
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    for update in [
        otto_client::SessionUpdate { agent: Some("nope".into()), ..Default::default() },
        otto_client::SessionUpdate { model: Some("missing".into()), ..Default::default() },
        otto_client::SessionUpdate { model: Some(" ".into()), ..Default::default() },
        otto_client::SessionUpdate { effort: Some(Effort::Low), reset_effort: true, ..Default::default() },
    ] {
        let error = client.session_update(&created.id, update).await.unwrap_err();
        assert!(matches!(&error, otto_client::ClientError::Rpc(RpcError { code, .. }) if code == RpcError::BAD_REQUEST), "{error}");
    }
    let raw = client
        .call(Method::SessionUpdate { session_id: created.id.clone(), provider: None, model: None, agent: None, effort: None, reset_effort: false })
        .await
        .unwrap();
    assert_eq!(raw["effort"], "high");
    let reset = client.session_update(&created.id, otto_client::SessionUpdate { reset_effort: true, ..Default::default() }).await.unwrap();
    assert_eq!(reset.effort, None);
    let projects = client.project_list().await.unwrap();
    assert_eq!(projects.len(), 1);
    assert!(projects[0].updated_at >= created.updated_at);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn fork_replay_and_follower_match_view_without_gaps() {
    let f = Fixture::new(vec![text("parent"), text("child")]).await;
    let server = Server::start(f.config()).await.unwrap();
    let client = f.connect().await;
    let session = f.session(&client).await;
    let parent = SessionFollower::new(client.clone(), &session.id).await.unwrap();
    prompt(&client, &session.id).await;
    finished(&parent).await;
    let fork: SessionInfo = client.call_typed(Method::SessionFork { session_id: session.id, at_seq: None }).await.unwrap();
    let view = client.session_view(&fork.id).await.unwrap();
    assert!(view.seq > 0);
    let mut events = client.events();
    client.session_subscribe(&fork.id, Some(0)).await.unwrap();
    let mut expected = 1;
    tokio::time::timeout(Duration::from_secs(5), async {
        while expected <= view.seq {
            if let ServerMessage::Event { seq, session_id: Some(id), event } = events.recv().await.unwrap().unwrap()
                && id == fork.id
            {
                assert_eq!(seq, expected);
                assert!(matches!(*event, Event::ItemDone { .. }));
                expected += 1;
            }
        }
    })
    .await
    .unwrap();
    let follower = SessionFollower::new(client.clone(), &fork.id).await.unwrap();
    assert_eq!(json!(follower.current().mirror.items), json!(view.items));
    prompt(&client, &fork.id).await;
    let mirror = finished(&follower).await;
    let view = client.session_view(&fork.id).await.unwrap();
    assert_eq!(mirror.last_seq, view.seq);
    assert_eq!(json!(mirror.items), json!(view.items));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_fork_at_an_item_is_a_top_level_session_and_a_sub_agent_is_not() {
    let f = Fixture::new(vec![text("one"), text("two")]).await;
    let server = Server::start(f.config()).await.unwrap();
    let client = f.connect().await;
    let session = f.session(&client).await;
    assert!(session.forked_from.is_none() && !session.is_subagent());
    let follower = SessionFollower::new(client.clone(), &session.id).await.unwrap();
    prompt(&client, &session.id).await;
    finished(&follower).await;
    prompt(&client, &session.id).await;
    let mut changes = follower.changes();
    let mirror = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mirror = changes.borrow_and_update().mirror.clone();
            if mirror.items.iter().filter(|i| i.role == otto_proto::Role::Assistant).count() == 2
                && mirror.info.status == otto_proto::SessionStatus::Idle
            {
                return mirror;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    // Seen live, every stored item has the sequence a fresh view gives it.
    let view = client.session_view(&session.id).await.unwrap();
    assert_eq!(view.item_seqs.len(), view.items.len());
    assert_eq!(mirror.item_seqs, view.item_seqs.clone().into_iter().collect());
    // At the first reply: the first exchange only.
    let first_reply = view.items.iter().find(|i| i.role == otto_proto::Role::Assistant).unwrap();
    let at = view.item_seqs[&first_reply.id];
    let fork = client.session_fork(&session.id, Some(at)).await.unwrap();
    assert_eq!(fork.parent_id.as_deref(), Some(session.id.as_str()));
    assert_eq!(fork.forked_from, Some(otto_proto::ForkOrigin { session_id: session.id.clone(), at_seq: at }));
    assert!(!fork.is_subagent());
    let forked = client.session_view(&fork.id).await.unwrap();
    assert_eq!(forked.items.iter().map(|i| i.text()).collect::<Vec<_>>(), ["hello", "one"]);
    client.session_rename(&fork.id, "Mine (branch)").await.unwrap();
    let listed = client.session_list(Some(session.project_id.clone()), false).await.unwrap();
    let row = listed.iter().find(|s| s.id == fork.id).unwrap();
    assert_eq!(row.title, "Mine (branch)");
    assert_eq!(row.forked_from.as_ref().map(|o| o.session_id.as_str()), Some(session.id.as_str()));
    // A sub-agent has a parent and no fork origin.
    let child = client
        .session_create(SessionCreateOptions { project_id: Some(session.project_id.clone()), parent: Some(session.id.clone()), ..Default::default() })
        .await
        .unwrap();
    assert!(child.forked_from.is_none() && child.is_subagent());
    let raw = client.call(Method::SessionView { session_id: child.id.clone(), limit: None, before_seq: None }).await.unwrap();
    assert!(raw["info"].get("forked_from").is_none(), "absent unless forked");
    // An older peer's info (no `forked_from`) and view (no `item_seqs`) still read.
    let mut old = serde_json::to_value(&forked).unwrap();
    old["info"].as_object_mut().unwrap().remove("forked_from");
    old.as_object_mut().unwrap().remove("item_seqs");
    let old: otto_proto::SessionView = serde_json::from_value(old).unwrap();
    assert!(old.info.forked_from.is_none() && old.item_seqs.is_empty());
    server.shutdown().await.unwrap();
}
