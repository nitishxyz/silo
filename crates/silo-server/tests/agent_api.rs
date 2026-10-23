//! Hook reports and browser actions over ordinary control connections.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};
use silo_client::{Control, Events};
use silo_proto::{
    access::{Rights, Scope},
    compat::feature,
    control::{AttentionReason, ClientRole, Created, Event, GrantCreated, GrantKind, Method, OwnMode, RpcError, SpawnSpec},
    model::BlockKind,
    paths,
};
use tokio::time::timeout;

const T: Duration = Duration::from_secs(15);

fn method(name: &str, params: Value) -> Method {
    serde_json::from_value(json!({"method": name, "params": params})).unwrap()
}

fn code(result: anyhow::Result<Value>) -> String {
    result.unwrap_err().downcast::<RpcError>().unwrap().code
}

async fn connect(home: &Path, role: ClientRole, token: Option<&str>) -> (Control, Events) {
    silo_client::set_role(role);
    silo_client::set_token(token.map(str::to_owned));
    let result = Control::connect(&paths::socket(home), "agent-test").await.unwrap();
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

async fn request(events: &mut Events) -> (String, silo_proto::control::AppCall) {
    match next(events, |e| matches!(e, Event::AppRequest { .. })).await {
        Event::AppRequest { request_id, request, .. } => (request_id, request),
        _ => unreachable!(),
    }
}

async fn grant(owner: &Control, scope: Scope, rights: Rights) -> String {
    let created: GrantCreated = owner.call(Method::GrantCreate { scope, rights, kind: GrantKind::Token, ttl_secs: None, label: None }).await.unwrap();
    created.secret
}

#[tokio::test(flavor = "multi_thread")]
async fn pane_helper() {
    let Ok(socket) = std::env::var("SILO_AGENT_HELPER") else { return };
    silo_client::set_token(Some(std::env::var("SILO_AGENT_TOKEN").unwrap()));
    let (client, _) = Control::connect(Path::new(&socket), "pane-hook").await.unwrap();
    let block = std::env::var("SILO_BLOCK").unwrap();
    client.call_value(method("block.report", json!({"block_id": block, "state": "done", "message": "pane hook"}))).await.unwrap();
    assert_eq!(code(client.call_value(method("block.notify", json!({"block_id": block, "body": "not permitted"}))).await), RpcError::FORBIDDEN);
    println!("pane-hook-ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_notifications_and_browser_actions() {
    let home = PathBuf::from(format!("/tmp/silo-agent-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let config =
        silo_server::Config { home: home.clone(), idle_exit: None, relay: silo_server::RelayConfig::Disabled, mdns: false, ..Default::default() };
    let server = tokio::spawn(silo_server::run(config));
    for _ in 0..500 {
        if silo_client::server_running(&home).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (owner, mut events) = connect(&home, ClientRole::App, None).await;
    owner.subscribe().await.unwrap();
    let terminal: Created = owner
        .call(Method::SessionCreate {
            name: None,
            spawn: SpawnSpec { command: vec!["/bin/sh".into()], ..Default::default() },
            kind: BlockKind::Terminal,
            url: None,
        })
        .await
        .unwrap();
    let b = &terminal.block_id;
    let secret = grant(&owner, Scope::Block { block_id: b.clone() }, Rights::Interact).await;
    let (interact, _) = connect(&home, ClientRole::Cli, Some(&secret)).await;
    let secret = grant(&owner, Scope::Block { block_id: b.clone() }, Rights::View).await;
    let (view, _) = connect(&home, ClientRole::Cli, Some(&secret)).await;
    let report =
        || method("block.report", json!({"block_id": b, "state": "blocked", "kind": "permission", "title": "a\u{202e}b", "message": "approve"}));
    assert_eq!(code(view.call_value(report()).await), RpcError::FORBIDDEN);
    assert_eq!(interact.call_value(report()).await.unwrap(), json!({}));
    let attention = next(&mut events, |e| matches!(e, Event::BlockAttention { reason: AttentionReason::Blocked, .. })).await;
    assert!(matches!(attention, Event::BlockAttention { title, body, .. } if title == "ab" && body == "approve"));
    let status = next(&mut events, |e| matches!(e, Event::BlockStatus { status } if status.records.iter().any(|r| r.message == "approve"))).await;
    assert!(matches!(status, Event::BlockStatus { status } if status.records[0].kind == Some(silo_proto::model::StatusKind::Permission)));
    assert_eq!(code(interact.call_value(method("block.report", json!({"block_id": b, "state": "working", "progress": 101}))).await), "bad_request");
    assert!(serde_json::from_value::<Method>(json!({"method": "block.report", "params": {"block_id": b}})).is_err());
    interact.call_value(method("block.report", json!({"block_id": b, "state": "idle"}))).await.unwrap();
    let notify = || method("block.notify", json!({"block_id": b, "title": "Hook", "body": "ready\u{200b}"}));
    assert_eq!(code(view.call_value(notify()).await), RpcError::FORBIDDEN);
    interact.call_value(notify()).await.unwrap();
    let attention = next(&mut events, |e| matches!(e, Event::BlockAttention { reason: AttentionReason::Notification, .. })).await;
    assert!(matches!(attention, Event::BlockAttention { title, body, .. } if title == "Hook" && body == "ready"));

    let secret = grant(&owner, Scope::Server, Rights::View).await;
    let spawn = SpawnSpec {
        command: vec![
            "/usr/bin/env".into(),
            format!("SILO_AGENT_HELPER={}", paths::socket(&home).display()),
            format!("SILO_AGENT_TOKEN={secret}"),
            std::env::current_exe().unwrap().display().to_string(),
            "--exact".into(),
            "pane_helper".into(),
            "--nocapture".into(),
        ],
        ..Default::default()
    };
    let pane: Created = owner.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    next(
        &mut events,
        |e| matches!(e, Event::BlockStatus { status } if status.block_id == pane.block_id && status.records.iter().any(|r| r.message == "pane hook")),
    )
    .await;
    timeout(T, async {
        loop {
            let capture = owner.call_value(Method::TerminalCapture { block_id: pane.block_id.clone(), history: true }).await.unwrap();
            if capture["text"].as_str().unwrap().contains("pane-hook-ok") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let browser: Created = owner
        .call(Method::WindowCreate {
            session_id: terminal.session_id,
            spawn: Default::default(),
            kind: BlockKind::Browser,
            url: Some("https://example.com".into()),
        })
        .await
        .unwrap();
    let b = &browser.block_id;
    let snapshot = || method("browser.snapshot", json!({"block_id": b}));
    let error = owner.call_value(snapshot()).await.unwrap_err().downcast::<RpcError>().unwrap();
    assert_eq!((error.code.as_str(), error.message.as_str()), (RpcError::NO_APP, silo_server::app_bridge::NO_BROWSER_APP));
    owner.call_value(Method::OwnBlock { block_ids: vec![b.clone()], mode: OwnMode::Take }).await.unwrap();
    assert_eq!(code(owner.call_value(snapshot()).await), RpcError::NO_APP, "owner lacks feature");
    assert_eq!(code(owner.call_value(method("block.report", json!({"block_id": b, "state": "done"}))).await), "bad_request");
    owner.call_value(method("block.notify", json!({"block_id": b, "body": "browser notice"}))).await.unwrap();
    next(&mut events, |e| matches!(e, Event::BlockAttention { body, .. } if body == "browser notice")).await;
    assert_eq!(code(owner.call_value(method("browser.snapshot", json!({"block_id": terminal.block_id}))).await), "bad_request");
    silo_client::advertise(feature::BROWSER_ACTIONS);
    let (app, mut app_events) = connect(&home, ClientRole::App, None).await;
    app.subscribe().await.unwrap();
    app.call_value(Method::OwnBlock { block_ids: vec![b.clone()], mode: OwnMode::Take }).await.unwrap();
    let secret = grant(&owner, Scope::Block { block_id: b.clone() }, Rights::View).await;
    let (viewer, _) = connect(&home, ClientRole::Cli, Some(&secret)).await;
    assert_eq!(code(viewer.call_value(method("browser.click", json!({"block_id": b, "target": "e1"}))).await), RpcError::FORBIDDEN);
    let secret = grant(&owner, Scope::Block { block_id: b.clone() }, Rights::Interact).await;
    let (actor, _) = connect(&home, ClientRole::Cli, Some(&secret)).await;
    assert_eq!(code(actor.call_value(method("browser.eval", json!({"block_id": b, "function": "() => 1"}))).await), RpcError::FORBIDDEN);
    for invalid in [json!({"block_id": b}), json!({"block_id": b, "text": "x", "regex": "x"})] {
        assert_eq!(code(viewer.call_value(method("browser.find", invalid)).await), "bad_request");
    }
    let names = [
        "snapshot",
        "find",
        "click",
        "hover",
        "fill",
        "fill_form",
        "type",
        "press",
        "scroll",
        "wait_for",
        "dialog",
        "eval",
        "console",
        "network",
        "screenshot",
    ];
    let (other, _) = connect(&home, ClientRole::App, None).await;
    for name in names {
        let name = format!("browser.{name}");
        let mut example = silo_proto::api::describe(&name).unwrap().example;
        example["params"]["block_id"] = json!(b);
        let method: Method = serde_json::from_value(example.clone()).unwrap();
        let client = match silo_proto::access::need(&method) {
            silo_proto::access::Need::Rights { rights: Rights::View, .. } => viewer.clone(),
            silo_proto::access::Need::Rights { rights: Rights::Interact, .. } => actor.clone(),
            _ => owner.clone(),
        };
        let call = tokio::spawn(async move { client.call_value(method).await });
        let (id, request) = request(&mut app_events).await;
        assert_eq!(request.method, name);
        assert_eq!(request.params, example["params"]);
        assert_eq!(code(other.call_value(Method::AppReply { request_id: id.clone(), result: None, error: None }).await), "not_found");
        let result = json!({"url": "https://example.com", "title": "Example", "snapshot": "- heading Example [ref=e1]"});
        app.call_value(Method::AppReply { request_id: id, result: Some(result.clone()), error: None }).await.unwrap();
        assert_eq!(timeout(T, call).await.unwrap().unwrap().unwrap(), result);
    }
    let call = tokio::spawn({
        let viewer = viewer.clone();
        let b = b.clone();
        async move { viewer.call_value(method("browser.wait_for", json!({"block_id": b, "timeout_ms": 0}))).await }
    });
    let (id, _) = request(&mut app_events).await;
    assert_eq!(code(timeout(T, call).await.unwrap().unwrap()), RpcError::TIMEOUT);
    assert_eq!(code(app.call_value(Method::AppReply { request_id: id, result: None, error: None }).await), "not_found");

    // browser.fetch: only an app hosting off-screen browsers loads pages.
    let fetch = || method("browser.fetch", json!({"url": "https://example.com", "selector": "main"}));
    let error = owner.call_value(fetch()).await.unwrap_err().downcast::<RpcError>().unwrap();
    assert_eq!((error.code.as_str(), error.message.as_str()), (RpcError::NO_APP, silo_server::app_bridge::NO_FETCH_APP));
    silo_client::advertise(feature::BROWSER_HOST);
    let (host, mut host_events) = connect(&home, ClientRole::App, None).await;
    host.subscribe().await.unwrap();
    let call = tokio::spawn({
        let owner = owner.clone();
        async move { owner.call_value(fetch()).await }
    });
    let (id, request) = request(&mut host_events).await;
    assert_eq!((request.method.as_str(), &request.params["selector"]), ("browser.fetch", &json!("main")));
    let result = json!({"url": "https://example.com/", "title": "Example", "html": "<main></main>", "timed_out": false, "truncated": false});
    host.call_value(Method::AppReply { request_id: id, result: Some(result.clone()), error: None }).await.unwrap();
    assert_eq!(timeout(T, call).await.unwrap().unwrap().unwrap(), result);

    owner.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    timeout(T, server).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(home);
}
