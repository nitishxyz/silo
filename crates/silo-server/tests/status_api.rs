//! Block status through the control API: `block.status` events,
//! `block.view`/`session.view` status, `block.status_seen`,
//! `block.commands` and `terminal.command_output`.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Control, Events};
use silo_proto::{
    control::{BlockView, CommandOutput, CommandRecords, Created, Event, Method, SpawnSpec},
    model::{BlockStatus, SessionView, StatusState},
    paths,
};
use silo_transport::RelayConfig;
use tokio::{task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(15);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-status-api-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> JoinHandle<()> {
    let config = silo_server::Config { home: home.to_owned(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    for _ in 0..500 {
        if silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn status(events: &mut Events, what: &str, done: impl Fn(&BlockStatus) -> bool) -> BlockStatus {
    timeout(T, async {
        loop {
            if let Some((_, Event::BlockStatus { status })) = events.recv().await
                && done(&status)
            {
                return status;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw {what}"))
}

const SCRIPT: &str = "printf '\\033]7501;state=done:app=cargo:msg=YnVpbHQ=\\033\\\\'; \
    printf '\\033]133;A\\007$ \\033]133;B\\007ls\\r\\n\\033]133;C;cmdline_url=ls\\007a.txt\\r\\n\\033]133;D;3\\007'; \
    printf '\\033]133;A\\007$ '; read l";

#[tokio::test(flavor = "multi_thread")]
async fn status_and_commands_through_the_api() {
    let home = temp_home("api");
    let task = start(&home).await;
    let (control, mut events) = Control::connect(&paths::socket(&home), "status-test").await.unwrap();
    control.subscribe().await.unwrap();
    let spawn = SpawnSpec { command: vec!["/bin/sh".into(), "-c".into(), SCRIPT.into()], ..SpawnSpec::default() };
    let created: Created = control.call(Method::SessionCreate { name: None, spawn, kind: Default::default(), url: None }).await.unwrap();

    let done = status(&mut events, "a done record", |s| s.records.iter().any(|r| r.state == StatusState::Done)).await;
    assert_eq!((done.records[0].app.as_str(), done.records[0].message.as_str()), ("cargo", "built"));
    let view: BlockView = control.call(Method::BlockView { block_id: created.block_id.clone() }).await.unwrap();
    assert_eq!(view.status.map(|s| s.records.len()), Some(1));
    let session: SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    assert_eq!(session.status.len(), 1);

    let records: CommandRecords = control.call(Method::BlockCommands { block_id: created.block_id.clone(), limit: None }).await.unwrap();
    assert_eq!(records.records.len(), 1);
    assert_eq!((records.records[0].command.as_deref(), records.records[0].exit), (Some("ls"), Some(3)));
    let output: CommandOutput = control.call(Method::TerminalCommandOutput { block_id: created.block_id.clone(), index: None }).await.unwrap();
    assert_eq!(output.text, "a.txt");

    control.call_value(Method::BlockStatusSeen { block_id: created.block_id.clone(), ids: None }).await.unwrap();
    status(&mut events, "seen clears it", |s| s.records.is_empty()).await;
    let view: BlockView = control.call(Method::BlockView { block_id: created.block_id }).await.unwrap();
    assert_eq!(view.status, None);

    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    let _ = timeout(T, task).await;
    let _ = std::fs::remove_dir_all(home);
}
