//! Approvals: an owner action from a client that is not a Silo app waits
//! for an app to allow it; with no app, only a caller outside every pane
//! with a terminal may confirm it itself.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Control, Events};
use silo_proto::{
    control::{ClientRole, Event, InvitePurpose, Method, PairInvite, RpcError},
    paths,
};
use silo_transport::RelayConfig;
use tokio::{task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(10);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-approval-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> JoinHandle<()> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        relay: RelayConfig::Disabled,
        mdns: false,
        require_approval: true,
        ..Default::default()
    };
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
    let connected = Control::connect(&paths::socket(home), "approval-test").await.unwrap();
    silo_client::set_role(ClientRole::Cli);
    connected
}

fn pair() -> Method {
    Method::PairCreate { ttl_secs: 600, purpose: InvitePurpose::Device }
}

/// The next approval request's id and action.
async fn requested(events: &mut Events) -> (String, String, String) {
    timeout(T, async {
        loop {
            if let Some((_, Event::ApprovalRequested { id, action, summary, .. })) = events.recv().await {
                return (id, action, summary);
            }
        }
    })
    .await
    .expect("approval requested")
}

async fn resolved(events: &mut Events) -> (String, bool) {
    timeout(T, async {
        loop {
            if let Some((_, Event::ApprovalResolved { id, allowed })) = events.recv().await {
                return (id, allowed);
            }
        }
    })
    .await
    .expect("approval resolved")
}

fn code(error: anyhow::Error) -> RpcError {
    error.downcast::<RpcError>().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_actions_from_non_app_clients_wait_for_an_app() {
    let home = temp_home("flow");
    let task = start(&home).await;
    let (app, mut app_events) = connect(&home, ClientRole::App).await;
    app.subscribe().await.unwrap();
    let (cli, _) = connect(&home, ClientRole::Cli).await;
    let (other, _) = connect(&home, ClientRole::Cli).await;

    // Allowed in the app: the call goes through.
    let call = tokio::spawn({
        let cli = cli.clone();
        async move { cli.call::<PairInvite>(pair()).await }
    });
    let (id, action, summary) = requested(&mut app_events).await;
    assert_eq!(action, "pair.create");
    assert_eq!(summary, "Create a device invite valid for 10 min");
    assert_eq!(
        code(other.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap_err()).code,
        "not_found",
        "only approvers answer"
    );
    app.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap();
    assert!(timeout(T, call).await.unwrap().unwrap().unwrap().ticket.starts_with("silo1"));
    assert_eq!(resolved(&mut app_events).await, (id, true));

    // Denied: forbidden.
    let call = tokio::spawn({
        let cli = cli.clone();
        async move { cli.call_value(pair()).await }
    });
    let (id, _, _) = requested(&mut app_events).await;
    app.call_value(Method::ApprovalAnswer { id: id.clone(), allow: false }).await.unwrap();
    let denied = code(timeout(T, call).await.unwrap().unwrap().unwrap_err());
    assert_eq!((denied.code.as_str(), denied.message.as_str()), (RpcError::FORBIDDEN, "Denied in Silo"));
    assert_eq!(resolved(&mut app_events).await, (id, false));

    // The app's own calls need nobody.
    app.call::<PairInvite>(pair()).await.unwrap();

    // No app: a caller outside every pane may confirm on its terminal, if
    // it has one; else the call fails.
    drop(app_events);
    drop(app);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let error = code(cli.call_value(pair()).await.unwrap_err());
    if std::fs::File::open("/dev/tty").is_ok() {
        assert_eq!(error.code, RpcError::CONFIRM, "{error}");
        let id = error.data.as_ref().and_then(|d| d["approval"].as_str()).unwrap().to_owned();
        assert_eq!(code(other.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap_err()).code, "not_found");
        cli.call_value(Method::ApprovalAnswer { id, allow: true }).await.unwrap();
        cli.call::<PairInvite>(pair()).await.unwrap();
        assert_eq!(code(cli.call_value(Method::ServerShutdown {}).await.unwrap_err()).code, RpcError::CONFIRM, "a confirmation is single-use");
    } else {
        assert_eq!((error.code.as_str(), error.message.as_str()), (RpcError::FORBIDDEN, "Approve in Silo: no Silo app is connected"));
    }

    let (app, _) = connect(&home, ClientRole::App).await;
    app.call_value(Method::ServerShutdown {}).await.unwrap();
    drop((app, cli, other));
    timeout(T, task).await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
