//! Approvals: an owner action from a client that is not a Silo app waits
//! for an app to allow it; with no app, only a caller outside every pane
//! with a terminal may confirm it itself.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Control, DataStream, Events};
use silo_proto::{
    control::{Capture, ClientRole, Created, Event, InvitePurpose, Method, PairInvite, Pasted, RpcError, SpawnSpec},
    data::ClipKind,
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

/// Run as a block's program (`SILO_APPROVAL_HELPER` = the socket): a
/// process in a pane that says it is the Silo app asks for an invite and
/// prints the outcome. Does nothing in a normal test run.
#[tokio::test(flavor = "multi_thread")]
async fn pane_helper() {
    let Ok(socket) = std::env::var("SILO_APPROVAL_HELPER") else { return };
    silo_client::set_role(ClientRole::App);
    let (control, _) = Control::connect(Path::new(&socket), "pane-app").await.unwrap();
    let outcome = match control.call_value(pair()).await {
        Ok(_) => "allowed".to_owned(),
        Err(error) => code(error).code,
    };
    println!("helper:{outcome}:");
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_actions_from_non_app_clients_wait_for_an_app() {
    let home = temp_home("flow");
    let task = start(&home).await;
    let (cli, _) = connect(&home, ClientRole::Cli).await;
    let (other, _) = connect(&home, ClientRole::Cli).await;

    // No app: a caller outside every pane may confirm on its terminal, if
    // it has one; else the call fails.
    let error = code(cli.call_value(pair()).await.unwrap_err());
    if std::fs::File::open("/dev/tty").is_ok() {
        assert_eq!(error.code, RpcError::CONFIRM, "{error}");
        let id = error.data.as_ref().and_then(|d| d["approval"].as_str()).unwrap().to_owned();
        assert_eq!(code(other.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap_err()).code, "not_found");
        cli.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap();
        assert_eq!(code(cli.call_value(Method::ServerShutdown {}).await.unwrap_err()).code, RpcError::CONFIRM, "confirmed for pair.create only");
        cli.call::<PairInvite>(pair()).await.unwrap();
        assert_eq!(code(cli.call_value(Method::ApprovalAnswer { id, allow: true }).await.unwrap_err()).code, "not_found", "single-use");
        assert_eq!(code(cli.call_value(pair()).await.unwrap_err()).code, RpcError::CONFIRM, "single-use");
    } else {
        assert_eq!((error.code.as_str(), error.message.as_str()), (RpcError::FORBIDDEN, "Approve in Silo: no Silo app is connected"));
    }

    let (app, mut app_events) = connect(&home, ClientRole::App).await;
    app.subscribe().await.unwrap();

    // A clipboard read asks nobody, even with an app on the block: the
    // app answers only right after a paste, and that paste is the consent.
    let created: Created =
        app.call(Method::SessionCreate { name: None, spawn: Default::default(), kind: Default::default(), url: None }).await.unwrap();
    let _stream = DataStream::open(&app, &created.block_id, None).await.unwrap();
    let read = cli.call::<Pasted>(Method::ClipboardRead { block_id: created.block_id.clone(), kind: ClipKind::Image });
    assert_eq!(timeout(T, read).await.expect("no approval wait").unwrap().content, None);

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

    // A program in a pane saying it is the app is asked like any other
    // (and is no approver of its own call).
    let exe = std::env::current_exe().unwrap().to_string_lossy().into_owned();
    let socket = paths::socket(&home).to_string_lossy().into_owned();
    let command =
        ["/usr/bin/env".to_owned(), format!("SILO_APPROVAL_HELPER={socket}"), exe, "--exact".into(), "pane_helper".into(), "--nocapture".into()];
    let spawn = SpawnSpec { command: command.into(), ..Default::default() };
    let pane: Created = app.call(Method::SessionCreate { name: None, spawn, kind: Default::default(), url: None }).await.unwrap();
    let (id, action, _) = requested(&mut app_events).await;
    assert_eq!(action, "pair.create");
    app.call_value(Method::ApprovalAnswer { id: id.clone(), allow: false }).await.unwrap();
    assert_eq!(resolved(&mut app_events).await, (id, false));
    let printed = timeout(T, async {
        loop {
            let capture: Capture = app.call(Method::TerminalCapture { block_id: pane.block_id.clone(), history: true }).await.unwrap();
            if let Some(rest) = capture.text.split("helper:").nth(1) {
                return rest.split(':').next().unwrap_or_default().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the helper's outcome");
    assert_eq!(printed, RpcError::FORBIDDEN);

    // The app's own calls need nobody.
    app.call::<PairInvite>(pair()).await.unwrap();
    app.call_value(Method::ServerShutdown {}).await.unwrap();
    drop((app, cli, other));
    timeout(T, task).await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
