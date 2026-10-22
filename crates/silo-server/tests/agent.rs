//! Agent blocks: creation, `agent.set`, persistence, takeover, status
//! reports and access.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Control, Events};
use silo_proto::{
    access::{Rights, Scope},
    agent::BlockReportParams,
    control::{BlockView, Created, Event, GrantCreated, GrantKind, Method, RpcError, SpawnSpec},
    model::{AgentState, Axis, BlockInfo, BlockKind, SessionView, StatusKind, StatusState, TermSize},
    paths,
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(15);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-agent-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn config(home: &Path) -> silo_server::Config {
    silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        ..Default::default()
    }
}

async fn start(home: &Path) -> JoinHandle<anyhow::Result<()>> {
    let task = tokio::spawn(silo_server::run(config(home)));
    for _ in 0..300 {
        if silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn connect(home: &Path) -> (Control, Events) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok(pair) = Control::connect(&paths::socket(home), "agent-test").await {
            return pair;
        }
        assert!(tokio::time::Instant::now() < deadline, "no server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop(control: Control, task: JoinHandle<anyhow::Result<()>>) {
    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap();
}

/// A session whose terminal is split with an agent block in `/tmp`.
async fn agent(control: &Control) -> Created {
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) };
    let main: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let spawn = SpawnSpec { cwd: Some("/tmp".into()), ..SpawnSpec::default() };
    let split = Method::BlockSplit { block_id: main.block_id, axis: Axis::Horizontal, spawn, kind: BlockKind::Agent, url: None };
    control.call(split).await.unwrap()
}

async fn info(control: &Control, created: &Created) -> BlockInfo {
    let view: SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    view.blocks.into_iter().find(|b| b.id == created.block_id).expect("agent block")
}

fn set(created: &Created, session_id: &str, title: &str) -> Method {
    Method::AgentSet { block_id: created.block_id.clone(), session_id: Some(session_id.into()), title: Some(title.into()), cwd: None }
}

fn move_to(created: &Created, cwd: &str) -> Method {
    Method::AgentSet { block_id: created.block_id.clone(), session_id: None, title: None, cwd: Some(cwd.into()) }
}

fn report(created: &Created, state: StatusState, kind: Option<StatusKind>) -> Method {
    Method::BlockReport(BlockReportParams {
        block_id: created.block_id.clone(),
        id: String::new(),
        state,
        kind,
        progress: None,
        app: Some("otto".into()),
        title: None,
        message: None,
    })
}

fn code(error: &anyhow::Error) -> String {
    if error.downcast_ref::<silo_client::Unauthorized>().is_some() {
        return RpcError::UNAUTHORIZED.to_owned();
    }
    error.downcast_ref::<RpcError>().map_or_else(|| format!("{error:#}"), |e| e.code.clone())
}

#[tokio::test]
async fn split_creates_agent_and_set_updates_it() {
    let home = temp_home("set");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let created = agent(&control).await;
    let block = info(&control, &created).await;
    assert_eq!((block.kind, block.title.as_str(), block.cwd.as_deref()), (BlockKind::Agent, "Agent", Some("/tmp")));
    assert_eq!((block.agent, block.browser, block.pid), (Some(AgentState::default()), None, None));

    control.call_value(set(&created, "ses_1", "Fix the lockfile")).await.unwrap();
    let block = info(&control, &created).await;
    assert_eq!(block.title, "Fix the lockfile");
    assert_eq!(block.agent, Some(AgentState { session_id: Some("ses_1".into()), title: "Fix the lockfile".into() }));

    let empty = Method::AgentSet { block_id: created.block_id.clone(), session_id: Some(String::new()), title: None, cwd: None };
    assert_eq!(code(&control.call_value(empty).await.unwrap_err()), "bad_request");
    let view: SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    let terminal = view.blocks.iter().find(|b| b.kind == BlockKind::Terminal).unwrap();
    let wrong = Method::AgentSet { block_id: terminal.id.clone(), session_id: Some("ses_1".into()), title: None, cwd: None };
    assert_eq!(code(&control.call_value(wrong).await.unwrap_err()), "bad_request");
    let send = Method::TerminalSend { block_id: created.block_id.clone(), text: "x".into() };
    assert_eq!(code(&control.call_value(send).await.unwrap_err()), "bad_request");

    control.call_value(Method::BlockClose { block_id: created.block_id.clone(), force: false }).await.unwrap();
    let view: SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    assert!(view.blocks.iter().all(|b| b.kind == BlockKind::Terminal));
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn set_cwd_moves_the_block_and_persists() {
    let home = temp_home("cwd");
    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.call_value(Method::EventsSubscribe {}).await.unwrap();
    let created = agent(&control).await;
    control.call_value(set(&created, "ses_6", "Old project")).await.unwrap();

    // A new folder: the block shows a new session there.
    control.call_value(move_to(&created, "/usr")).await.unwrap();
    let changed = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let (_, Event::BlockChanged { block }) = events.recv().await.expect("events")
                && block.id == created.block_id
                && block.cwd.as_deref() == Some("/usr")
            {
                return block;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!((changed.title.as_str(), changed.agent), ("Agent", Some(AgentState::default())));
    let block = info(&control, &created).await;
    assert_eq!((block.cwd.as_deref(), block.agent), (Some("/usr"), Some(AgentState::default())));

    // The same folder keeps the session; a folder with a session sets both.
    control.call_value(set(&created, "ses_7", "New project")).await.unwrap();
    control.call_value(move_to(&created, "/usr")).await.unwrap();
    assert_eq!(info(&control, &created).await.agent.and_then(|a| a.session_id).as_deref(), Some("ses_7"));
    let both = Method::AgentSet { block_id: created.block_id.clone(), session_id: Some("ses_8".into()), title: None, cwd: Some("/tmp".into()) };
    control.call_value(both).await.unwrap();
    let block = info(&control, &created).await;
    assert_eq!((block.cwd.as_deref(), block.agent.and_then(|a| a.session_id).as_deref()), (Some("/tmp"), Some("ses_8")));

    for bad in ["", "relative/dir", "/tmp/a\nb"] {
        assert_eq!(code(&control.call_value(move_to(&created, bad)).await.unwrap_err()), "bad_request", "{bad:?}");
    }

    control.call_value(move_to(&created, "/usr")).await.unwrap();
    stop(control, task).await;
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let after = info(&control, &created).await;
    assert_eq!((after.cwd.as_deref(), after.title.as_str(), after.agent), (Some("/usr"), "Agent", Some(AgentState::default())));
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn report_shows_status_and_raises_attention() {
    let home = temp_home("report");
    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.call_value(Method::EventsSubscribe {}).await.unwrap();
    let created = agent(&control).await;

    control.call_value(report(&created, StatusState::Working, None)).await.unwrap();
    let view: BlockView = control.call(Method::BlockView { block_id: created.block_id.clone() }).await.unwrap();
    let records = view.status.expect("status").records;
    assert_eq!(records.iter().map(|r| (r.state, r.app.as_str())).collect::<Vec<_>>(), [(StatusState::Working, "otto")]);

    control.call_value(report(&created, StatusState::Blocked, Some(StatusKind::Question))).await.unwrap();
    let attention = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let (_, Event::BlockAttention { block_id, reason, .. }) = events.recv().await.expect("events") {
                return (block_id, reason);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(attention, (created.block_id.clone(), silo_proto::control::AttentionReason::Blocked));
    let view: SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    let status = view.status.iter().find(|s| s.block_id == created.block_id).expect("agent status in the view");
    assert_eq!((status.records[0].state, status.records[0].kind), (StatusState::Blocked, Some(StatusKind::Question)));

    control.call_value(report(&created, StatusState::Done, None)).await.unwrap();
    control.call_value(Method::BlockStatusSeen { block_id: created.block_id.clone(), ids: None }).await.unwrap();
    let view: BlockView = control.call(Method::BlockView { block_id: created.block_id.clone() }).await.unwrap();
    assert!(view.status.is_none(), "seen done records go");
    let notify =
        Method::BlockNotify(silo_proto::agent::BlockNotifyParams { block_id: created.block_id.clone(), title: None, body: "Finished".into() });
    control.call_value(notify).await.unwrap();
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn state_survives_restart() {
    let home = temp_home("persist");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let created = agent(&control).await;
    control.call_value(set(&created, "ses_2", "Design")).await.unwrap();
    let before = info(&control, &created).await;
    stop(control, task).await;

    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let after = info(&control, &created).await;
    assert_eq!((after.kind, after.title.as_str(), after.cwd.as_deref()), (BlockKind::Agent, "Design", Some("/tmp")));
    assert_eq!(after.agent, before.agent);
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_survives_takeover() {
    let home = temp_home("takeover");
    let old = start(&home).await;
    let (control, _) = connect(&home).await;
    let created = agent(&control).await;
    control.call_value(set(&created, "ses_3", "Takeover")).await.unwrap();
    let before = info(&control, &created).await;

    let new = tokio::spawn(silo_server::takeover(config(&home), false));
    tokio::time::timeout(TIMEOUT, old).await.unwrap().unwrap().expect("old server commits and exits");
    drop(control);
    let (control, _) = connect(&home).await;
    assert_eq!(info(&control, &created).await.agent, before.agent);
    control.call_value(set(&created, "ses_4", "After")).await.unwrap();
    assert_eq!(info(&control, &created).await.title, "After");
    stop(control, new).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn set_needs_interact() {
    let home = temp_home("access");
    let task = start(&home).await;
    let (owner, _) = connect(&home).await;
    let created = agent(&owner).await;
    let grant = |rights| Method::GrantCreate {
        scope: Scope::Session { session_id: created.session_id.clone() },
        rights,
        kind: GrantKind::Token,
        ttl_secs: None,
        label: Some("test".into()),
    };
    let view: GrantCreated = owner.call(grant(Rights::View)).await.unwrap();
    let interact: GrantCreated = owner.call(grant(Rights::Interact)).await.unwrap();

    silo_client::set_token(Some(view.secret));
    let (viewer, _) = Control::connect(&paths::socket(&home), "agent-test").await.unwrap();
    silo_client::set_token(Some(interact.secret));
    let (typist, _) = Control::connect(&paths::socket(&home), "agent-test").await.unwrap();
    silo_client::set_token(None);

    assert_eq!(code(&viewer.call_value(set(&created, "ses_5", "No")).await.unwrap_err()), RpcError::FORBIDDEN);
    assert_eq!(code(&viewer.call_value(report(&created, StatusState::Working, None)).await.unwrap_err()), RpcError::FORBIDDEN);
    typist.call_value(set(&created, "ses_5", "Yes")).await.unwrap();
    typist.call_value(report(&created, StatusState::Working, None)).await.unwrap();
    assert_eq!(info(&owner, &created).await.title, "Yes");
    stop(owner, task).await;
    let _ = std::fs::remove_dir_all(&home);
}
