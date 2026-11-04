//! Provided tools (`tools.*`) over real connections: offer, list, forward,
//! reply, approval, rights, and cleanup when blocks close or providers go.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};
use silo_client::{Control, Events};
use silo_proto::{
    access::{Rights, Scope},
    control::{ClientRole, Created, Event, GrantCreated, GrantKind, Method, RpcError, SpawnSpec},
    model::BlockKind,
    paths,
    tools::{ToolList, ToolOutcome, ToolRequest},
};
use tokio::time::timeout;

const T: Duration = Duration::from_secs(15);

fn method(name: &str, params: Value) -> Method {
    serde_json::from_value(json!({"method": name, "params": params})).unwrap()
}

fn code(result: anyhow::Result<Value>) -> String {
    result.unwrap_err().downcast::<RpcError>().unwrap().code
}

async fn connect(home: &Path, role: ClientRole, token: Option<&str>, name: &str) -> (Control, Events) {
    silo_client::set_role(role);
    silo_client::set_token(token.map(str::to_owned));
    let result = Control::connect(&paths::socket(home), name).await.unwrap();
    silo_client::set_role(ClientRole::Cli);
    silo_client::set_token(None);
    result
}

async fn next(events: &mut Events, predicate: impl Fn(&Event) -> bool) -> Event {
    timeout(T, async {
        loop {
            let (_, event) = events.recv().await.expect("event stream closed");
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("event")
}

async fn tool_request(events: &mut Events) -> ToolRequest {
    match next(events, |e| matches!(e, Event::ToolRequest(_))).await {
        Event::ToolRequest(request) => request,
        _ => unreachable!(),
    }
}

async fn names(control: &Control) -> Vec<String> {
    let list: ToolList = control.call(Method::ToolsList {}).await.unwrap();
    list.tools.into_iter().filter(|t| t.provider_id != 0).map(|t| t.tool.name).collect()
}

fn reply(request_id: &str, text: &str) -> Method {
    method("tools.reply", json!({"request_id": request_id, "result": {"content": [{"type": "text", "text": text}]}}))
}

#[tokio::test(flavor = "multi_thread")]
async fn provided_tools_end_to_end() {
    let home = PathBuf::from(format!("/tmp/silo-tools-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let config = silo_server::Config {
        home: home.clone(),
        idle_exit: None,
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        require_approval: true,
        ..Default::default()
    };
    let server = tokio::spawn(silo_server::run(config));
    for _ in 0..500 {
        if silo_client::server_running(&home).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The app approves; a provider and a caller are plain clients of the user.
    let (app, mut app_events) = connect(&home, ClientRole::App, None, "app").await;
    app.subscribe().await.unwrap();
    let (provider, mut provider_events) = connect(&home, ClientRole::Cli, None, "provider").await;
    provider.subscribe().await.unwrap();
    let (caller, mut caller_events) = connect(&home, ClientRole::Cli, None, "caller").await;
    caller.subscribe().await.unwrap();
    assert!(caller.peer_supports(silo_proto::compat::feature::TOOLS));

    let created: Created = app
        .call(Method::SessionCreate {
            name: None,
            spawn: SpawnSpec { command: vec!["/bin/sh".into()], ..Default::default() },
            kind: BlockKind::Terminal,
            url: None,
        })
        .await
        .unwrap();
    let block = created.block_id.clone();
    let session = created.session_id.clone();

    let server_tools = json!({"tools": [
        {"name": "echo", "description": "Echo the input.", "read_only": true},
        {"name": "danger", "description": "Change something.", "approval": true},
    ]});
    provider.call_value(method("tools.provide", server_tools)).await.unwrap();
    next(&mut caller_events, |e| matches!(e, Event::ToolsChanged {})).await;
    provider
        .call_value(method("tools.provide", json!({"block_id": block, "tools": [{"name": "peek", "description": "Peek.", "read_only": true}]})))
        .await
        .unwrap();
    assert_eq!(names(&caller).await, ["danger", "echo", "peek"]);
    let list: ToolList = caller.call(Method::ToolsList {}).await.unwrap();
    let peek = list.tools.iter().find(|t| t.tool.name == "peek").unwrap();
    assert_eq!((peek.block_id.as_deref(), peek.provider.as_str()), (Some(block.as_str()), "provider"));

    // Another client cannot take a name already provided there.
    assert_eq!(code(caller.call_value(method("tools.provide", json!({"tools": [{"name": "echo", "description": "x"}]}))).await), "bad_request");
    assert_eq!(code(caller.call_value(method("tools.provide", json!({"tools": [{"name": "Bad", "description": "x"}]}))).await), "bad_request");

    // A call goes to the provider and its reply comes back; nobody else may answer it.
    let call = {
        let caller = caller.clone();
        tokio::spawn(async move { caller.call_value(method("tools.call", json!({"name": "echo", "input": {"say": "hi"}}))).await })
    };
    let request = tool_request(&mut provider_events).await;
    assert_eq!((request.name.as_str(), &request.input), ("echo", &json!({"say": "hi"})));
    assert_eq!(request.caller.label, "caller");
    assert_eq!(code(app.call_value(reply(&request.request_id, "forged")).await), "not_found");
    // The caller's other calls go on while it waits.
    assert_eq!(timeout(T, names(&caller)).await.unwrap().len(), 3);
    provider.call_value(reply(&request.request_id, "hi")).await.unwrap();
    let outcome: ToolOutcome = serde_json::from_value(timeout(T, call).await.unwrap().unwrap().unwrap()).unwrap();
    assert_eq!(outcome, ToolOutcome::text("hi"));

    // A provider's error is the call's.
    let call = {
        let caller = caller.clone();
        tokio::spawn(async move { caller.call_value(method("tools.call", json!({"name": "peek"}))).await })
    };
    let request = tool_request(&mut provider_events).await;
    assert_eq!(request.block_id.as_deref(), Some(block.as_str()));
    provider
        .call_value(method("tools.reply", json!({"request_id": request.request_id, "error": {"code": "bad_request", "message": "nope"}})))
        .await
        .unwrap();
    assert_eq!(code(timeout(T, call).await.unwrap().unwrap()), "bad_request");

    // An approval tool waits for the app; the app's own calls do not.
    let call = {
        let caller = caller.clone();
        tokio::spawn(async move { caller.call_value(method("tools.call", json!({"name": "danger", "input": {"x": 1}}))).await })
    };
    let asked = next(&mut app_events, |e| matches!(e, Event::ApprovalRequested { .. })).await;
    let Event::ApprovalRequested { id, action, summary, .. } = asked else { unreachable!() };
    assert_eq!(action, "tools.call");
    assert!(summary.contains("danger") && summary.contains("\"x\":1"), "{summary}");
    app.call_value(Method::ApprovalAnswer { id, allow: true }).await.unwrap();
    let request = tool_request(&mut provider_events).await;
    assert_eq!(request.name, "danger");
    provider.call_value(reply(&request.request_id, "done")).await.unwrap();
    assert!(timeout(T, call).await.unwrap().unwrap().is_ok());
    let call = {
        let caller = caller.clone();
        tokio::spawn(async move { caller.call_value(method("tools.call", json!({"name": "danger"}))).await })
    };
    let asked = next(&mut app_events, |e| matches!(e, Event::ApprovalRequested { .. })).await;
    let Event::ApprovalRequested { id, .. } = asked else { unreachable!() };
    app.call_value(Method::ApprovalAnswer { id, allow: false }).await.unwrap();
    assert_eq!(code(timeout(T, call).await.unwrap().unwrap()), RpcError::FORBIDDEN);

    // Rights: a viewer lists and calls only read-only tools.
    let created: GrantCreated = app
        .call(Method::GrantCreate { scope: Scope::Server, rights: Rights::View, kind: GrantKind::Token, ttl_secs: None, label: None })
        .await
        .unwrap();
    let (viewer, _) = connect(&home, ClientRole::Cli, Some(&created.secret), "viewer").await;
    assert_eq!(names(&viewer).await, ["echo", "peek"]);
    // Silo's own run with the viewer's rights: it may look, not act.
    assert!(viewer.call_value(method("tools.call", json!({"name": "session_list"}))).await.is_ok());
    let typed = viewer.call_value(method("tools.call", json!({"name": "terminal_send", "input": {"block_id": block, "text": "x"}}))).await;
    assert_eq!(code(typed), RpcError::FORBIDDEN);
    assert_eq!(code(viewer.call_value(method("tools.call", json!({"name": "danger"}))).await), "not_found");
    assert_eq!(code(viewer.call_value(method("tools.provide", json!({"tools": [{"name": "mine", "description": "x"}]}))).await), RpcError::FORBIDDEN);

    // Silo's own tools run here for a raw client, as that client.
    let listed: ToolList = caller.call(Method::ToolsList {}).await.unwrap();
    let click = listed.tools.iter().find(|t| t.tool.name == "browser_click").unwrap();
    assert_eq!((click.provider.as_str(), click.block_kind), ("silo", Some(BlockKind::Browser)));
    let sessions: ToolOutcome =
        serde_json::from_value(caller.call_value(method("tools.call", json!({"name": "session_list"}))).await.unwrap()).unwrap();
    assert!(!sessions.is_error);
    assert!(serde_json::to_string(&sessions).unwrap().contains(&session), "{sessions:?}");
    let blocks: ToolOutcome = serde_json::from_value(caller.call_value(method("tools.call", json!({"name": "block_list"}))).await.unwrap()).unwrap();
    assert!(serde_json::to_string(&blocks).unwrap().contains(&block));

    // A closed block takes its tools along.
    app.call_value(Method::BlockClose { block_id: block.clone(), force: true }).await.unwrap();
    next(&mut caller_events, |e| matches!(e, Event::ToolsChanged {})).await;
    assert_eq!(names(&caller).await, ["danger", "echo"]);

    // A provider that leaves fails its pending calls and withdraws its tools.
    let call = {
        let caller = caller.clone();
        tokio::spawn(async move { caller.call_value(method("tools.call", json!({"name": "echo"}))).await })
    };
    tool_request(&mut provider_events).await;
    drop(provider_events);
    drop(provider);
    assert_eq!(code(timeout(T, call).await.unwrap().unwrap()), RpcError::NO_APP);
    assert!(names(&caller).await.is_empty());
    assert_eq!(code(caller.call_value(method("tools.call", json!({"name": "echo"}))).await), "not_found");

    server.abort();
    let _ = std::fs::remove_dir_all(&home);
}
