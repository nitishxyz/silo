//! SIGTERM during a live upgrade is deferred: an aborted upgrade keeps the
//! server serving. Its own test binary, since it signals the process.

use std::{path::PathBuf, time::Duration};

use silo_client::Control;
use silo_proto::{control::Method, paths};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TIMEOUT: Duration = Duration::from_secs(15);

fn sigterm() {
    // SAFETY: plain kill(2) on our own pid.
    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_during_an_aborted_upgrade_is_ignored() {
    let home = PathBuf::from(format!("/tmp/silo-sigterm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let config = silo_server::Config {
        home: home.clone(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        ..Default::default()
    };
    let mut task = tokio::spawn(silo_server::run(config));
    for _ in 0..300 {
        if silo_client::server_running(&home).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A successor that asks for the upgrade, reads the start of the
    // transfer (so the handoff is in flight), then goes away.
    let mut successor = tokio::net::UnixStream::connect(paths::socket(&home)).await.unwrap();
    successor.write_all(b"SILO-UPGRADE 1\n").await.unwrap();
    let mut len = [0; 4];
    tokio::time::timeout(TIMEOUT, successor.read_exact(&mut len)).await.unwrap().unwrap();
    sigterm();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(successor);

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!task.is_finished(), "the server exited after an aborted upgrade");
    let control = Control::connect(&paths::socket(&home), "sigterm-test").await.unwrap().0;
    control.call_value(Method::Ping {}).await.unwrap();

    // Outside an upgrade SIGTERM still stops it.
    sigterm();
    tokio::time::timeout(TIMEOUT, &mut task).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
