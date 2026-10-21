//! Approvals: an owner action from a client that is not a Silo app waits
//! for an app to allow it; with no app, only a caller outside every pane
//! with a terminal may confirm it itself. A paired device's call is also
//! offered to that device's own app.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Control, DataStream, Events};
use silo_proto::{
    control::{Caller, Capture, ClientRole, Created, Event, InvitePurpose, Method, PairInvite, Pasted, RpcError, SpawnSpec},
    data::ClipKind,
    paths,
};
use silo_transport::{Conn, EndpointAddr, Identity, Net, PairTicket, RelayConfig};
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

/// The hello's role is process-wide: one connection at a time sets it.
static ROLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `connect` run saying `role`.
async fn as_role<T>(role: ClientRole, connect: impl std::future::Future<Output = T>) -> T {
    let _one = ROLE.lock().await;
    silo_client::set_role(role);
    let connected = connect.await;
    silo_client::set_role(ClientRole::Cli);
    connected
}

/// A connection saying `role`.
async fn connect(home: &Path, role: ClientRole) -> (Control, Events) {
    as_role(role, Control::connect(&paths::socket(home), "approval-test")).await.unwrap()
}

fn pair() -> Method {
    Method::PairCreate { ttl_secs: 600, purpose: InvitePurpose::Device }
}

/// The next approval request's id and action.
async fn requested(events: &mut Events) -> (String, String, String) {
    let (id, action, summary, _) = requested_by(events).await;
    (id, action, summary)
}

/// The next approval request's id, action, summary and caller.
async fn requested_by(events: &mut Events) -> (String, String, String, Caller) {
    timeout(T, async {
        loop {
            if let Some((_, Event::ApprovalRequested { id, action, summary, caller, .. })) = events.recv().await {
                return (id, action, summary, caller);
            }
        }
    })
    .await
    .expect("approval requested")
}

/// The approval requests `events` got so far.
async fn cards(events: &mut Events) -> Vec<String> {
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut ids = Vec::new();
    while let Ok((_, event)) = events.try_recv() {
        if let Event::ApprovalRequested { id, .. } = event {
            ids.push(id);
        }
    }
    ids
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

/// A device invite from `admin` (the owner's app: no approval) and the
/// server's loopback address.
async fn invite(admin: &Control) -> (PairTicket, EndpointAddr) {
    for _ in 0..200 {
        let invite: PairInvite = admin.call(pair()).await.unwrap();
        let ticket = PairTicket::decode(&invite.ticket).unwrap();
        let port = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port);
        if let Some(port) = port {
            let addr = EndpointAddr::new(ticket.addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into());
            return (ticket, addr);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server endpoint has no direct address");
}

/// A device's Silo app: bound with the device key `key`, paired with an
/// invite from `admin` as `name`, subscribed.
async fn device_app(admin: &Control, key: &Identity, name: &str) -> (Net, Control, Events, EndpointAddr) {
    let (ticket, addr) = invite(admin).await;
    let net = Net::bind(key, RelayConfig::Disabled, false).await.unwrap();
    let rc = timeout(T, net.dial(addr.clone())).await.unwrap().unwrap();
    let (control, events) = as_role(ClientRole::App, Control::over_remote(rc, name, false, Some(&ticket))).await.unwrap();
    control.subscribe().await.unwrap();
    (net, control, events, addr)
}

/// A process of its own on `key`'s device (a CLI run) speaking for it.
async fn speaking_for(key: &Identity, addr: &EndpointAddr, role: ClientRole) -> (Net, Control, Events) {
    let net = Net::bind(&Identity::ephemeral(), RelayConfig::Disabled, false).await.unwrap();
    let rc = timeout(T, net.dial(addr.clone())).await.unwrap().unwrap();
    let proof = silo_client::device_proof(key, addr.id, net.id());
    let (control, events) = as_role(role, Control::over_remote_as(rc, "silo cli", false, proof)).await.unwrap();
    (net, control, events)
}

/// The person at a paired device approves its calls there: the device's
/// own app gets the card, another device's app never does, and the owner's
/// apps on the host get every card (the first answer wins).
#[tokio::test(flavor = "multi_thread")]
async fn a_device_s_calls_are_approved_at_that_device() {
    let home = temp_home("device");
    let task = start(&home).await;
    // The owner's app, not subscribed yet: no approver.
    let (host_app, mut host_events) = connect(&home, ClientRole::App).await;
    let admin = &host_app;
    let (mac, other) = (Identity::load_or_create(home.join("device-mac")).unwrap(), Identity::load_or_create(home.join("device-other")).unwrap());
    let (mac_net, mac_app, mut mac_events, addr) = device_app(admin, &mac, "Ana's Mac").await;
    let (other_net, other_app, mut other_events, _) = device_app(admin, &other, "Other Mac").await;
    // A CLI run on the Mac, and one saying it is the app: only the app
    // bound with the device key approves.
    let (cli_net, cli, _) = speaking_for(&mac, &addr, ClientRole::Cli).await;
    let (fake_net, fake_app, mut fake_events) = speaking_for(&mac, &addr, ClientRole::App).await;
    fake_app.subscribe().await.unwrap();

    // A call the device holds too few rights for, approved at the device.
    let mut call = tokio::spawn({
        let cli = cli.clone();
        async move { cli.call::<PairInvite>(pair()).await }
    });
    let (id, action, summary, caller) = tokio::select! {
        got = requested_by(&mut mac_events) => got,
        done = &mut call => panic!("the call ended without asking: {done:?}"),
    };
    assert_eq!((action.as_str(), summary.as_str()), ("pair.create", "Create a device invite valid for 10 min"));
    assert_eq!(caller.label, "silo cli · Ana's Mac");
    assert_eq!(caller.from.as_deref(), Some("this device (CLI)"), "the device's own card names no other machine's panes");
    for answering in [&other_app, &fake_app] {
        let refused = code(answering.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap_err());
        assert_eq!(refused.code, "not_found", "only the device's own app answers");
    }
    mac_app.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap();
    assert!(timeout(T, call).await.unwrap().unwrap().unwrap().ticket.starts_with("silo1"));
    assert_eq!(resolved(&mut mac_events).await, (id, true));
    assert!(cards(&mut other_events).await.is_empty(), "another device never sees it");
    assert!(cards(&mut fake_events).await.is_empty(), "an endpoint speaking for the device is no approver");

    // The owner's app on the host gets it too; the first answer wins.
    host_app.subscribe().await.unwrap();
    let call = tokio::spawn({
        let cli = cli.clone();
        async move { cli.call_value(Method::ServerShutdown {}).await }
    });
    let (id, action, _, caller) = requested_by(&mut host_events).await;
    assert_eq!(action, "server.shutdown");
    assert_eq!(caller.from.as_deref(), Some("Ana's Mac (CLI)"), "the host's card names the device");
    assert_eq!(requested_by(&mut mac_events).await.0, id);
    host_app.call_value(Method::ApprovalAnswer { id: id.clone(), allow: false }).await.unwrap();
    let late = code(mac_app.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap_err());
    assert_eq!(late.code, "not_found", "answered already");
    let denied = code(timeout(T, call).await.unwrap().unwrap().unwrap_err());
    assert_eq!((denied.code.as_str(), denied.message.as_str()), (RpcError::FORBIDDEN, "Denied in Silo"));
    assert_eq!(resolved(&mut host_events).await, (id.clone(), false));
    assert_eq!(resolved(&mut mac_events).await, (id, false), "dismissed everywhere");

    // A local caller's card is the host's alone.
    let (local, _) = connect(&home, ClientRole::Cli).await;
    let call = tokio::spawn({
        let local = local.clone();
        async move { local.call::<PairInvite>(pair()).await }
    });
    let (id, _, _, caller) = requested_by(&mut host_events).await;
    assert_eq!(caller.from, None);
    let refused = code(mac_app.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap_err());
    assert_eq!(refused.code, "not_found", "a device never answers a local caller");
    host_app.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap();
    timeout(T, call).await.unwrap().unwrap().unwrap();
    assert!(!cards(&mut mac_events).await.contains(&id));

    // `silo remote upgrade` from the Mac, with the owner's app open on the
    // host: the Mac's app is asked as well (denied here: nothing installs).
    let up_net = Net::bind(&Identity::ephemeral(), RelayConfig::Disabled, false).await.unwrap();
    let rc = timeout(T, up_net.dial(addr.clone())).await.unwrap().unwrap();
    let proof = silo_client::device_proof(&mac, addr.id, up_net.id());
    let upgrade = timeout(T, silo_client::upgrade::open(&Conn::Iroh(rc.clone()), Some(proof))).await.unwrap().unwrap();
    let binary = home.join("not-a-build");
    std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
    let sent = tokio::spawn(async move { upgrade.send(&binary, false).await.map(|_| ()) });
    let (id, action, _, caller) = requested_by(&mut mac_events).await;
    assert_eq!((action.as_str(), caller.from.as_deref()), ("upgrade", Some("this device (CLI)")));
    assert_eq!(requested_by(&mut host_events).await.3.from.as_deref(), Some("Ana's Mac (CLI)"));
    assert_eq!(code(other_app.call_value(Method::ApprovalAnswer { id: id.clone(), allow: true }).await.unwrap_err()).code, "not_found");
    mac_app.call_value(Method::ApprovalAnswer { id, allow: false }).await.unwrap();
    let refused = timeout(T, sent).await.unwrap().unwrap().unwrap_err().downcast::<silo_client::upgrade::Refused>().unwrap();
    assert_eq!(refused.code, "denied");
    assert!(cards(&mut other_events).await.is_empty());
    rc.close(b"done");

    drop((cli, fake_app, other_app, mac_app, local));
    for net in [mac_net, other_net, cli_net, fake_net, up_net] {
        net.close().await.unwrap();
    }
    host_app.call_value(Method::ServerShutdown {}).await.unwrap();
    drop(host_app);
    timeout(T, task).await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
