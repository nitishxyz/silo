//! Forwarded `app.*` requests: with no app they fail with `no_app`;
//! otherwise one app runs them and its reply answers the call.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::json;
use silo_client::{Control, Events};
use silo_proto::{
    control::{ClientRole, Created, Event, Method, RpcError},
    paths,
};
use silo_transport::RelayConfig;
use tokio::{task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(10);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-app-requests-{name}-{}", std::process::id()));
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

/// A connection saying `role` (process-wide: this file has one test).
async fn connect(home: &Path, role: ClientRole) -> (Control, Events) {
    silo_client::set_role(role);
    let connected = Control::connect(&paths::socket(home), "app-requests-test").await.unwrap();
    silo_client::set_role(ClientRole::Cli);
    connected
}

/// The next forwarded request: its id, method and params.
async fn request(events: &mut Events) -> (String, String, serde_json::Value) {
    timeout(T, async {
        loop {
            if let Some((_, Event::AppRequest { request_id, request, .. })) = events.recv().await {
                return (request_id, request.method, request.params);
            }
        }
    })
    .await
    .expect("app.request")
}

fn code(error: anyhow::Error) -> String {
    error.downcast::<RpcError>().unwrap().code
}

fn full_screen(client_id: Option<u64>) -> Method {
    Method::AppFullScreen { on: Some(true), client_id }
}

#[tokio::test(flavor = "multi_thread")]
async fn app_requests_go_to_one_app_and_its_reply_answers() {
    let home = temp_home("flow");
    let task = start(&home).await;
    let (cli, _) = connect(&home, ClientRole::Cli).await;

    assert_eq!(code(cli.call_value(full_screen(None)).await.unwrap_err()), RpcError::NO_APP);

    let (first, mut first_events) = connect(&home, ClientRole::App).await;
    first.subscribe().await.unwrap();
    let call = tokio::spawn({
        let cli = cli.clone();
        async move { cli.call_value(full_screen(None)).await }
    });
    let (id, method, params) = request(&mut first_events).await;
    assert_eq!((method.as_str(), params), ("app.full_screen", json!({"on": true})), "client_id only routes");
    let wrong = Method::AppReply { request_id: id.clone(), result: None, error: None };
    // Another connection: a call waits for its answer before the next.
    let (other, _) = connect(&home, ClientRole::Cli).await;
    assert_eq!(code(other.call_value(wrong).await.unwrap_err()), RpcError::FORBIDDEN, "only an app replies");
    first.call_value(Method::AppReply { request_id: id.clone(), result: Some(json!({"full_screen": true})), error: None }).await.unwrap();
    assert_eq!(timeout(T, call).await.unwrap().unwrap().unwrap(), json!({"full_screen": true}));
    let again = Method::AppReply { request_id: id, result: None, error: None };
    assert_eq!(code(first.call_value(again).await.unwrap_err()), "not_found", "answered once");

    // A second app: it connected last, so it gets the next request; the
    // first one's error answers the call.
    let (second, mut second_events) = connect(&home, ClientRole::App).await;
    second.subscribe().await.unwrap();
    let call = tokio::spawn({
        let cli = cli.clone();
        async move { cli.call_value(full_screen(None)).await }
    });
    let (id, _, _) = request(&mut second_events).await;
    let not_yours = Method::AppReply { request_id: id.clone(), result: None, error: None };
    assert_eq!(code(first.call_value(not_yours).await.unwrap_err()), "not_found", "only the app it went to");
    let error = RpcError::bad_request("no window");
    second.call_value(Method::AppReply { request_id: id, result: None, error: Some(error) }).await.unwrap();
    assert_eq!(code(timeout(T, call).await.unwrap().unwrap().unwrap_err()), "bad_request");

    // Focusing a block makes the first app the most recently active.
    let created: Created =
        first.call(Method::SessionCreate { name: None, spawn: Default::default(), kind: Default::default(), url: None }).await.unwrap();
    first.call_value(Method::BlockFocus { block_id: created.block_id }).await.unwrap();
    let call = tokio::spawn({
        let cli = cli.clone();
        async move { cli.call_value(Method::AppSwitcher { client_id: None }).await }
    });
    let (id, method, _) = request(&mut first_events).await;
    assert_eq!(method, "app.switcher");
    first.call_value(Method::AppReply { request_id: id, result: None, error: None }).await.unwrap();
    assert_eq!(timeout(T, call).await.unwrap().unwrap().unwrap(), json!({}));

    // A named app gets it regardless; a CLI client is no app.
    let named = second.hello().client_id;
    let call = tokio::spawn({
        let cli = cli.clone();
        async move { cli.call_value(full_screen(Some(named))).await }
    });
    let (id, _, _) = request(&mut second_events).await;
    second.call_value(Method::AppReply { request_id: id, result: Some(json!({"full_screen": false})), error: None }).await.unwrap();
    assert_eq!(timeout(T, call).await.unwrap().unwrap().unwrap(), json!({"full_screen": false}));
    let cli_id = cli.hello().client_id;
    assert_eq!(code(cli.call_value(full_screen(Some(cli_id))).await.unwrap_err()), "not_found");

    std::fs::write(paths::log(&home), "started\nlistening\n").unwrap();
    assert_eq!(cli.call_value(Method::ServerLog { lines: Some(1) }).await.unwrap(), json!({"text": "listening\n"}));

    // A restart stops the server and leaves the binary to start again.
    assert!(!silo_server::layout_cmds::restart_requested());
    cli.call_value(Method::ServerRestart { force: false }).await.unwrap();
    drop((cli, first, second));
    timeout(T, task).await.unwrap().unwrap();
    assert!(silo_server::layout_cmds::restart_requested());
    let _ = std::fs::remove_dir_all(&home);
}
