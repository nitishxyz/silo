//! `silo do` through the binary, against a server on a temp home.

use std::{path::Path, process::Output, time::Duration};

use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    model::{BlockKind, SessionView},
    paths,
};

fn silo(home: &Path, args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_silo")).args(args).env("SILO_HOME", home).stdin(std::process::Stdio::null()).output().unwrap()
}

async fn run(home: &Path, args: &[&str]) -> Output {
    let (home, args): (std::path::PathBuf, Vec<String>) = (home.to_owned(), args.iter().map(|&a| a.to_owned()).collect());
    tokio::task::spawn_blocking(move || silo(&home, &args.iter().map(String::as_str).collect::<Vec<_>>())).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn do_calls_methods_with_generated_flags() {
    let home = std::path::PathBuf::from(format!("/tmp/silo-cli-do-{}", std::process::id()));
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

    let (control, _events) = Control::connect(&paths::socket(&home), "test").await.unwrap();
    let spawn = || SpawnSpec { command: vec!["sleep".into(), "100".into()], ..SpawnSpec::default() };
    let created: Created = control.call(Method::SessionCreate { name: None, spawn: spawn(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let session = created.session_id.clone();
    let second: Created =
        control.call(Method::WindowCreate { session_id: session.clone(), spawn: spawn(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let active = || async {
        let view: SessionView = control.call(Method::SessionView { session_id: session.clone() }).await.unwrap();
        view.session.active_window
    };
    assert_eq!(active().await, second.window_id);

    let out = run(&home, &["do", "window.step", "--session-id", &session, "--delta", "1"]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(), serde_json::json!({}));
    assert_eq!(active().await, created.window_id);
    let out = run(&home, &["do", "window.step", &session, "-1"]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(active().await, second.window_id);

    let out = run(&home, &["do", "app.full_screen"]).await;
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no_app"), "{stderr}");

    let out = run(&home, &["do", "window.step", "--help"]).await;
    let usage = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && usage.contains("--session-id") && usage.contains("--delta"), "{usage}");
    let out = run(&home, &["do", "nope.nothing"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown method nope.nothing"));
    let out = run(&home, &["do", "window.step", &session]).await;
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing --delta"));

    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
