use crate::tests::*;

#[tokio::test]
async fn discovery_defaults_and_device_login_use_runtime_and_auth() {
    let f = Fixture::new(vec![]).await;
    let project = f.runtime.open_project(f.dir.path()).await.unwrap();
    std::fs::create_dir_all(f.dir.path().join("agents")).unwrap();
    std::fs::write(f.dir.path().join("agents/custom.md"), "---\nname: custom\ndescription: custom runtime agent\n---\nHelp.").unwrap();
    std::fs::write(f.dir.path().join("config.json"), r#"{"agent":"plan","model":"startup"}"#).unwrap();
    let server = Server::start(f.config()).await.unwrap();
    let client = f.connect().await;
    let projects = client.call(Method::ProjectList {}).await.unwrap();
    assert_eq!(projects[0]["id"], project.id);
    assert!(!f.dir.path().join("projects.json").exists());
    let first = client.session_create(SessionCreateOptions { project_id: Some(project.id), ..Default::default() }).await.unwrap();
    assert_eq!(first.agent, "plan");
    assert_eq!(first.approval_mode, otto_proto::ApprovalMode::Plan);
    let error =
        client.call(Method::SessionSetMode { session_id: first.id.clone(), approval_mode: otto_proto::ApprovalMode::Auto }).await.unwrap_err();
    assert!(error.to_string().contains("bad_request"), "{error}");
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
    let pending =
        client.call(Method::AuthLogin { provider: "openai".into(), mode: AuthMode::Subscription, api_key: None, device: true }).await.unwrap();
    assert_eq!(pending["user_code"], "ABCD");
    assert_eq!(pending["interval_secs"], 5);
    assert_eq!(pending["verification_uri"], "https://example.test/device");
    assert!(
        client
            .call(Method::AuthLogin { provider: "openai".into(), mode: AuthMode::ApiKey, api_key: Some("test".into()), device: true })
            .await
            .is_err()
    );
    server.shutdown().await.unwrap();
    let restarted = Server::start(f.config()).await.unwrap();
    let client = f.connect().await;
    assert_eq!(f.session(&client).await.model, "changed");
    restarted.shutdown().await.unwrap();
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
