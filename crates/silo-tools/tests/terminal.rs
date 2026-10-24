use serde_json::{Value, json};
use silo_client::{Control, transport::RelayConfig};
use silo_proto::{control::Method, paths};
use silo_tools::{Context, Part, Tools};
use std::{collections::BTreeMap, time::Duration};
use tokio_util::sync::CancellationToken;

const SHELL: &str = r#"printf '\033]133;A\007$ '; while IFS= read -r l; do printf '\033]133;C\007'; sh -c "$l"; s=$?; printf '\033]133;D;%d\007\033]133;A\007$ ' "$s"; done"#;

async fn call(tools: &Tools, name: &str, input: Value) -> Value {
    let result = tokio::time::timeout(Duration::from_secs(15), tools.call(name, input, CancellationToken::new())).await.unwrap().unwrap();
    assert!(!result.is_error);
    match result.content.into_iter().next().unwrap() {
        Part::Json(value) => value,
        other => panic!("expected JSON: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_tools_against_server() {
    scenario(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_status_waits() {
    scenario(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn command_blocks_and_hidden_terminals() {
    use silo_proto::{
        control::{Created, SpawnSpec},
        model::{Axis, BlockKind, SessionView},
    };
    use silo_tools::Watch;
    let home = tempfile::Builder::new().prefix("silo-tools-cmd-").tempdir_in("/tmp").unwrap();
    let config = silo_server::Config { home: home.path().into(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let server = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    while !silo_client::server_running(home.path()).await {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (control, events) = Control::connect(&paths::socket(home.path()), "tools-test").await.unwrap();
    let tools = Tools::new(control.clone(), events, Context { agent_block: None, ..Default::default() });
    let main: Created =
        control.call(Method::SessionCreate { name: None, spawn: SpawnSpec::default(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: SpawnSpec::default(),
        kind: BlockKind::Agent,
        url: None,
        agent: None,
    };
    let agent: Created = control.call(split).await.unwrap();
    let set = Method::AgentSet { block_id: agent.block_id.clone(), session_id: Some("ses_parent".into()), title: None, cwd: None };
    control.call_value(set).await.unwrap();
    assert_eq!(tools.agent_block(&["ses_child".into(), "ses_parent".into()]).await.unwrap().as_deref(), Some(agent.block_id.as_str()));
    assert_eq!(tools.agent_block(&["ses_other".into()]).await.unwrap(), None);

    // A prompt is reported once quiet; answered, the command exits.
    let script = "echo \"$PAGER\"; printf 'continue? '; read a; echo \"got $a\"; exit 5";
    let env = [("PAGER".to_owned(), "cat".to_owned())].into();
    let block = tools.spawn_command(&agent.block_id, "/bin/sh", script, Some("/tmp".into()), env).await.unwrap();
    let watch = |prompt| tokio::time::timeout(Duration::from_secs(15), tools.watch(&block, prompt, |_| {}));
    let Watch::Waiting { screen } = watch(true).await.unwrap().unwrap() else { panic!("expected a prompt") };
    assert!(screen.contains("cat") && screen.ends_with("continue?"), "{screen}");
    call(&tools, "terminal_send", json!({"block_id":block,"text":"yes","submit":true})).await;
    assert_eq!(watch(true).await.unwrap().unwrap(), Watch::Exited { code: Some(5), text: "cat\ncontinue? yes\ngot yes".into() });
    let view: SessionView = control.call(Method::SessionView { session_id: main.session_id.clone() }).await.unwrap();
    assert!(view.session.parked.is_empty(), "a command block closes on exit");

    // Stopped mid-run: its text so far, then gone.
    let block = tools.spawn_command(&agent.block_id, "/bin/sh", "echo started; sleep 30", None, BTreeMap::new()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), tools.watch(&block, false, |_| {})).await.unwrap_err();
    assert_eq!(tools.stop(&block).await.unwrap(), "started");

    // A hidden terminal is parked next to the anchor and closes with it.
    let hidden = tools.call_in("terminal_open", json!({"visible":false}), CancellationToken::new(), Some(agent.block_id.clone())).await.unwrap();
    let Part::Json(hidden) = &hidden.content[0] else { panic!() };
    let view: SessionView = control.call(Method::SessionView { session_id: main.session_id.clone() }).await.unwrap();
    assert_eq!(view.session.parked.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), [hidden["block_id"].as_str().unwrap()]);
    control.call_value(Method::BlockClose { block_id: agent.block_id.clone(), force: true }).await.unwrap();
    let view: SessionView = control.call(Method::SessionView { session_id: main.session_id.clone() }).await.unwrap();
    assert!(view.session.parked.is_empty());
    control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), server).await.unwrap().unwrap();
}

async fn scenario(report_status: bool) {
    let home = tempfile::Builder::new().prefix("silo-tools-").tempdir_in("/tmp").unwrap();
    let config = silo_server::Config { home: home.path().into(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let server = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(15), async {
        while !silo_client::server_running(home.path()).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (control, events) = Control::connect(&paths::socket(home.path()), "tools-test").await.unwrap();
    let tools = Tools::new(control.clone(), events, Context { agent_block: None, ..Default::default() });
    let opened = call(&tools, "terminal_open", json!({"command":SHELL,"exec":true,"place":"session"})).await;
    let block = opened["block_id"].as_str().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let result = call(&tools, "terminal_run", json!({"block_id":block,"command":"echo tools-output; exit 3","timeout_ms":10000})).await;
    assert_eq!(result["exit"], 3);
    assert!(result["output"].as_str().unwrap().contains("tools-output"));
    call(&tools, "terminal_send", json!({"block_id":block,"text":"echo sent-text","submit":true})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let read = call(&tools, "terminal_read", json!({"block_id":block,"lines":40})).await;
    assert!(read["text"].as_str().unwrap().contains("sent-text"));
    let text = call(&tools, "terminal_wait", json!({"block_id":block,"text":"sent-text","timeout_ms":2000})).await;
    assert_eq!(text["reason"], "text");
    let quiet = call(&tools, "terminal_wait", json!({"block_id":block,"quiet_ms":350,"timeout_ms":2000})).await;
    assert_eq!(quiet["reason"], "quiet");
    let timeout = call(&tools, "terminal_wait", json!({"block_id":block,"text":"never-here","timeout_ms":50})).await;
    assert_eq!(timeout["reason"], "timeout");
    let list = call(&tools, "block_list", json!({})).await;
    assert!(list["blocks"].as_array().unwrap().iter().any(|b| b["block"]["id"] == block));
    // The old name answers the same.
    let old = call(&tools, "terminal_list", json!({})).await;
    assert!(old["blocks"].as_array().unwrap().iter().any(|b| b["block"]["id"] == block));
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(tools.call("terminal_wait", json!({"block_id":block}), cancelled).await.is_err());
    if report_status {
        let first = call(&tools, "terminal_wait", json!({"block_id":block,"timeout_ms":3000}));
        let second = call(&tools, "terminal_wait", json!({"block_id":block,"timeout_ms":3000}));
        let report = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            control.call_raw("block.report", json!({"block_id":block,"state":"done"})).await.unwrap();
        };
        let (first, second, ()) = tokio::join!(first, second, report);
        assert_eq!(first["reason"], "status");
        assert_eq!(second["reason"], "status");
        tokio::time::sleep(Duration::from_millis(10)).await;
        let stale = call(&tools, "terminal_wait", json!({"block_id":block,"timeout_ms":50})).await;
        assert_eq!(stale["reason"], "timeout");
    }
    let wait_exit = call(&tools, "terminal_wait", json!({"block_id":block,"timeout_ms":3000}));
    let close = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        call(&tools, "terminal_close", json!({"block_id":block,"force":true})).await;
    };
    let (exited, ()) = tokio::join!(wait_exit, close);
    assert_eq!(exited["reason"], "exit");
    control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), server).await.unwrap().unwrap();
}
