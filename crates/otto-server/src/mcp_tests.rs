use crate::tests::*;
use otto_mcp::fake::Fake;

async fn mcp_event(events: &mut otto_client::Events, want: impl Fn(&Event) -> bool) -> Event {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let ServerMessage::Event { event, session_id: None, .. } = events.recv().await.unwrap().unwrap()
                && want(&event)
            {
                return *event;
            }
        }
    })
    .await
    .expect("mcp event")
}

fn status_is(want: McpStatus, tools: u32) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::McpStatus { name, status, tools: t } if name == "remote" && *status == want && *t == tools)
}

#[tokio::test]
async fn mcp_servers_are_added_listed_updated_and_removed() {
    let fake = Fake::start(false).await;
    let f = Fixture::new(vec![]).await;
    let _server = f.start().await;
    let client = f.connect().await;
    let mut events = client.events();
    let root = f.dir.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let project_root = Some(root.to_string_lossy().into_owned());
    let http = McpTransport::Http { url: fake.url.clone(), headers: [("X-Team".to_owned(), "a".to_owned())].into() };
    let add = Method::McpAdd { project_root: None, scope: McpScope::Global, name: "remote".into(), transport: http.clone(), enabled: true };
    let added: McpServer = client.call_typed(add.clone()).await.unwrap();
    assert_eq!((added.scope, &added.transport, added.enabled), (McpScope::Global, &http, true));
    assert!(matches!(added.status, McpStatus::Connecting | McpStatus::Connected));
    mcp_event(&mut events, |e| matches!(e, Event::McpChanged {})).await;
    mcp_event(&mut events, status_is(McpStatus::Connected, 3)).await;
    let error = client.call(add).await.unwrap_err();
    assert!(matches!(error, otto_client::ClientError::Rpc(RpcError { code, .. }) if code == RpcError::BAD_REQUEST));

    let stdio = McpTransport::Stdio { command: "/nonexistent/server".into(), args: vec!["--x".into()], env: Default::default() };
    let add = Method::McpAdd { project_root: project_root.clone(), scope: McpScope::Project, name: "local".into(), transport: stdio, enabled: false };
    let local: McpServer = client.call_typed(add).await.unwrap();
    assert_eq!((local.scope, local.status, local.enabled), (McpScope::Project, McpStatus::Disabled, false));
    assert!(root.join(".mcp.json").exists());
    let listed: Vec<McpServer> = client.call_typed(Method::McpList { project_root: project_root.clone() }).await.unwrap();
    assert_eq!(listed.iter().map(|s| (s.name.as_str(), s.scope)).collect::<Vec<_>>(), [("local", McpScope::Project), ("remote", McpScope::Global)]);
    assert_eq!((listed[1].status.clone(), listed[1].tools), (McpStatus::Connected, 3));
    let global = client.mcp_list(None).await.unwrap();
    assert_eq!(global.len(), 1);

    let update = Method::McpUpdate {
        project_root: project_root.clone(),
        scope: McpScope::Project,
        name: "local".into(),
        transport: None,
        enabled: Some(true),
    };
    let local: McpServer = client.call_typed(update).await.unwrap();
    assert!(local.enabled && local.status != McpStatus::Disabled);
    let missing = Method::McpUpdate { project_root: None, scope: McpScope::Global, name: "missing".into(), transport: None, enabled: Some(true) };
    assert!(matches!(client.call(missing).await, Err(otto_client::ClientError::Rpc(RpcError { code, .. })) if code == RpcError::NOT_FOUND));
    let relative = Method::McpList { project_root: Some("relative".into()) };
    assert!(client.call(relative).await.is_err());

    for (scope, name, project_root) in [(McpScope::Project, "local", project_root.clone()), (McpScope::Global, "remote", None)] {
        client.call(Method::McpRemove { project_root, scope, name: name.into() }).await.unwrap();
    }
    let listed: Vec<McpServer> = client.call_typed(Method::McpList { project_root }).await.unwrap();
    assert!(listed.is_empty());
}

#[tokio::test]
async fn an_mcp_server_signs_in_through_the_browser() {
    let fake = Fake::start(true).await;
    let f = Fixture::new(vec![]).await;
    let _server = f.start().await;
    let client = f.connect().await;
    let mut events = client.events();
    let http = McpTransport::Http { url: fake.url.clone(), headers: Default::default() };
    let add = Method::McpAdd { project_root: None, scope: McpScope::Global, name: "remote".into(), transport: http, enabled: true };
    client.call(add).await.unwrap();
    mcp_event(&mut events, status_is(McpStatus::NeedsSignIn, 0)).await;
    let login: McpLoginStarted = client.call_typed(Method::McpLogin { project_root: None, name: "remote".into() }).await.unwrap();
    assert_eq!(login.callback.path, "/callback");
    assert!(login.url.contains(&format!("redirect_uri=http%3A%2F%2F127.0.0.1%3A{}%2Fcallback", login.callback.port)));
    let complete = client.call(Method::McpLoginComplete { login_id: login.login_id.clone(), code: None });
    let (done, page) = tokio::join!(complete, Fake::browse(&login.url));
    assert!(page.unwrap().contains("Authorization complete"));
    let server: McpServer = serde_json::from_value(done.unwrap()).unwrap();
    assert_eq!(server.name, "remote");
    mcp_event(&mut events, status_is(McpStatus::Connected, 3)).await;
    client.mcp_logout(None, "remote").await.unwrap();
    mcp_event(&mut events, status_is(McpStatus::NeedsSignIn, 0)).await;
    let unknown = Method::McpLoginComplete { login_id: "nope".into(), code: None };
    assert!(matches!(client.call(unknown).await, Err(otto_client::ClientError::Rpc(RpcError { code, .. })) if code == RpcError::NOT_FOUND));
}
