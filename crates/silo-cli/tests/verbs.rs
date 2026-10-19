//! `silo float` and `silo api describe` through the binary, against a
//! server on a temp home.

use std::{path::Path, process::Output, time::Duration};

use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    model::{Axis, BlockKind, SessionView},
    paths,
};

fn silo(home: &Path, args: &[&str]) -> Output {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_silo")).args(args).env("SILO_HOME", home).output().unwrap();
    assert!(output.status.success(), "silo {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    output
}

#[tokio::test(flavor = "multi_thread")]
async fn float_and_describe() {
    let home = std::path::PathBuf::from(format!("/tmp/silo-cli-verbs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let config = silo_server::Config { home: home.clone(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !silo_client::server_running(&home).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server did not start");

    let out = tokio::task::spawn_blocking({
        let home = home.clone();
        move || silo(&home, &["api", "describe", "ping"])
    })
    .await
    .unwrap();
    let described: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(described["description"].as_str().is_some_and(|d| !d.is_empty()), "{described}");

    let (control, _events) = Control::connect(&paths::socket(&home), "test").await.unwrap();
    let spawn = || SpawnSpec { command: vec!["sleep".into(), "100".into()], ..SpawnSpec::default() };
    let created: Created = control.call(Method::SessionCreate { name: None, spawn: spawn(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit { block_id: created.block_id, axis: Axis::Vertical, spawn: spawn(), kind: BlockKind::Terminal, url: None };
    let second: Created = control.call(split).await.unwrap();

    tokio::task::spawn_blocking({
        let home = home.clone();
        let block = second.block_id.clone();
        move || silo(&home, &["float", &block])
    })
    .await
    .unwrap();
    let view: SessionView = control.call(Method::SessionView { session_id: created.session_id }).await.unwrap();
    let floats: Vec<_> = view.session.windows[0].floats.iter().map(|f| f.id.clone()).collect();
    assert_eq!(floats, [second.block_id]);
    let _ = std::fs::remove_dir_all(&home);
}
