//! A connection's requests don't wait on each other needlessly: one stuck
//! (here an `app.*` request no app answers) holds up neither reads nor
//! block changes behind it on the same connection.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::Control;
use silo_proto::{
    control::{ClientRole, Created, FileList, FilesListParams, Method, SpawnSpec},
    model::BlockKind,
    paths,
};
use silo_transport::RelayConfig;
use tokio::time::timeout;

async fn start(home: &Path) {
    let config = silo_server::Config { home: home.to_owned(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    for _ in 0..500 {
        if silo_client::server_running(home).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stuck_request_holds_up_no_listing_behind_it() {
    let dir = PathBuf::from(format!("/tmp/silo-lanes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (home, root) = (dir.join("home"), dir.join("root"));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("a.txt"), b"a").unwrap();
    start(&home).await;

    // An app that hears requests and never answers them.
    silo_client::set_role(ClientRole::App);
    let (app, _app_events) = Control::connect(&paths::socket(&home), "lanes-app").await.unwrap();
    silo_client::set_role(ClientRole::Cli);
    app.subscribe().await.unwrap();

    let (cli, _) = Control::connect(&paths::socket(&home), "lanes-cli").await.unwrap();
    let spawn = SpawnSpec { cwd: Some(root.display().to_string()), ..SpawnSpec::default() };
    let files: Created = cli.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Files, url: None }).await.unwrap();

    // Waits for the app: the connection's ordered lane is stuck behind it.
    let stuck = tokio::spawn({
        let cli = cli.clone();
        async move { cli.call_value(Method::AppFullScreen { on: Some(true), client_id: None }).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!stuck.is_finished(), "the app never answers");

    let list = Method::FilesList(FilesListParams { block_id: files.block_id.clone(), path: String::new() });
    // Well inside the ~10 s an unanswered app request waits before it fails.
    let listed: FileList = timeout(Duration::from_secs(2), cli.call(list)).await.expect("the listing doesn't wait behind it").unwrap();
    assert_eq!(listed.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["src", "a.txt"]);
    stuck.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
