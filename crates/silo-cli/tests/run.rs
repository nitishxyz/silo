//! `silo run <block> -- <command…>` (Phase 7 §7.4, P7-6): prints the
//! command's output and exits with its code.

use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    model::BlockKind,
    paths,
};
use tokio::task::JoinHandle;

const T: Duration = Duration::from_secs(15);

/// A tiny shell that marks each command with OSC 133 C and D (its exit
/// code), like Silo's shell integration does.
const SHELL: &str = r#"printf '\033]133;A\007$ '; while IFS= read -r l; do printf '\033]133;C\007'; sh -c "$l"; s=$?; printf '\033]133;D;%d\007\033]133;A\007$ ' "$s"; done"#;

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-cli-run-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> JoinHandle<()> {
    let config = silo_server::Config { home: home.to_owned(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    tokio::time::timeout(T, async {
        while !silo_client::server_running(home).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server did not start");
    task
}

async fn silo(home: &Path, args: &[&str]) -> Output {
    let (home, args) = (home.to_owned(), args.iter().map(|&a| a.to_owned()).collect::<Vec<_>>());
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_silo")).args(&args).env("SILO_HOME", &home).stdin(Stdio::null()).output().unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn run_prints_the_output_and_exits_with_the_code() {
    let home = temp_home("codes");
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "run-test").await.unwrap();
    let spawn = SpawnSpec { command: vec!["/bin/sh".into(), "-c".into(), SHELL.into()], ..SpawnSpec::default() };
    let created: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let block = created.block_id.as_str();

    let failed = silo(&home, &["run", block, "--timeout", "15s", "--", "false"]).await;
    assert_eq!(failed.status.code(), Some(1), "{}", String::from_utf8_lossy(&failed.stderr));

    let echo = silo(&home, &["run", block, "--timeout", "15s", "--", "echo", "hi there"]).await;
    assert_eq!(echo.status.code(), Some(0), "{}", String::from_utf8_lossy(&echo.stderr));
    assert_eq!(String::from_utf8_lossy(&echo.stdout), "hi there\n");

    let three = silo(&home, &["run", block, "--timeout", "15s", "--", "sh", "-c", "echo out; exit 3"]).await;
    assert_eq!(three.status.code(), Some(3), "{}", String::from_utf8_lossy(&three.stderr));
    assert_eq!(String::from_utf8_lossy(&three.stdout), "out\n");

    control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    let _ = tokio::time::timeout(T, task).await;
    let _ = std::fs::remove_dir_all(home);
}
