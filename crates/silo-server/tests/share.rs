//! Silo-to-Silo sharing (`docs/PHASE7.md` §8), two in-process servers with
//! relays disabled: a share link's ticket, what view and interact links
//! may do over iroh, watchers, revocation and block links.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Control, DataStream};
use silo_proto::{
    access::{Granted, Rights, Scope},
    control::{Capture, Created, GrantCreated, GrantKind, InvitePurpose, Method, PairInvite, RpcError, SizeInfo, SpawnSpec},
    data::ClientData,
    model::{BlockKind, SessionView, TermSize, WatcherKind},
    paths,
};
use silo_transport::{EndpointAddr, Identity, Net, PairTicket, RelayConfig, SHARE_PREFIX, ShareTicket};
use tokio::{task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(10);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-share-{name}-{}", std::process::id()));
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

async fn owner(home: &Path) -> Control {
    Control::connect(&paths::socket(home), "share-test").await.unwrap().0
}

async fn shutdown(control: Control, task: JoinHandle<()>) {
    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    timeout(T, task).await.unwrap().unwrap();
}

fn shell() -> SpawnSpec {
    SpawnSpec { cwd: None, command: vec!["/bin/sh".into()], size: Some(TermSize::new(80, 24)) }
}

async fn link(owner: &Control, scope: Scope, rights: Rights) -> (GrantCreated, ShareTicket) {
    let created: GrantCreated =
        owner.call(Method::GrantCreate { scope, rights, kind: GrantKind::Link, ttl_secs: None, label: Some("share test".into()) }).await.unwrap();
    let text = created.ticket.clone().expect("a link has a share ticket");
    assert!(text.starts_with(SHARE_PREFIX), "{text}");
    let ticket = ShareTicket::parse(&text).unwrap();
    assert_eq!(ticket.addr.ip_addrs().count(), 0, "a share ticket carries no direct addresses");
    assert_eq!(ticket.vt, silo_proto::VT);
    (created, ticket)
}

/// The server's iroh address on loopback (from a pairing ticket): relays
/// are disabled, so the ticket's relay-only address can't be dialed here.
async fn loopback(owner: &Control) -> (EndpointAddr, String) {
    for _ in 0..200 {
        let invite: PairInvite = owner.call(Method::PairCreate { ttl_secs: 60, purpose: InvitePurpose::Device }).await.unwrap();
        let ticket = PairTicket::decode(&invite.ticket).unwrap();
        if let Some(port) = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port) {
            return (EndpointAddr::new(ticket.addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into()), invite.ticket);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server endpoint has no direct address");
}

async fn capture(control: &Control, block: &str) -> String {
    let capture: Capture = control.call(Method::TerminalCapture { block_id: block.into(), history: true }).await.unwrap();
    capture.text
}

async fn wait_for(what: &str, mut done: impl AsyncFnMut() -> bool) {
    for _ in 0..500 {
        if done().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn code(result: anyhow::Result<serde_json::Value>) -> String {
    let error = result.unwrap_err();
    if error.downcast_ref::<silo_client::Unauthorized>().is_some() {
        return RpcError::UNAUTHORIZED.to_owned();
    }
    error.downcast::<RpcError>().map(|e| e.code).unwrap_or_else(|e| format!("{e:#}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn share_links_between_two_silos() {
    let (host_home, other_home) = (temp_home("host"), temp_home("other"));
    let (host_task, other_task) = (start(&host_home).await, start(&other_home).await);
    let host = owner(&host_home).await;
    let other = owner(&other_home).await;
    let created: Created = host.call(Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let (session, block) = (created.session_id.clone(), created.block_id.clone());
    let (host_addr, pair_text) = loopback(&host).await;
    let (other_addr, _) = loopback(&other).await;
    // The viewing Silo dials with its own endpoint.
    let net = Net::bind(&Identity::load_or_create(other_home.join("viewer")).unwrap(), RelayConfig::Disabled, false).await.unwrap();

    // Tickets never parse as each other.
    let (view, view_ticket) = link(&host, Scope::Session { session_id: session.clone() }, Rights::View).await;
    let view_text = view.ticket.clone().unwrap();
    assert!(PairTicket::decode(&view_text).is_err(), "a share ticket is no pairing ticket");
    assert!(ShareTicket::parse(&pair_text).is_err(), "a pairing ticket is no share ticket");

    // A view link attaches, can't type or resize, and shows in watchers.
    let (viewer, _) = Control::connect_shared(&net, &view_ticket, Some(host_addr.clone()), "Ana's Silo").await.expect("view link admitted");
    let granted = Granted { scope: Scope::Session { session_id: session.clone() }, rights: Rights::View, expires_at: view.grant.expires_at };
    assert_eq!(viewer.hello().granted.as_ref(), Some(&granted));
    let shown: SessionView = viewer.call(Method::SessionView { session_id: session.clone() }).await.unwrap();
    assert_eq!(shown.session.id, session);
    let mut looking = DataStream::open(&viewer, &block, None).await.unwrap();
    timeout(T, looking.rx.recv()).await.expect("snapshot").expect("data stream open");
    assert!(looking.send(&ClientData::Input(b"echo VIEW_ONLY_INPUT\r".to_vec())));
    assert!(looking.send(&ClientData::Resize(TermSize::new(20, 5))));
    assert_eq!(code(viewer.call_value(Method::TerminalSend { block_id: block.clone(), text: "echo VIEW_SEND\r".into() }).await), RpcError::FORBIDDEN);
    assert_eq!(code(viewer.call_value(Method::TerminalResize { block_id: block.clone(), size: TermSize::new(20, 5) }).await), RpcError::FORBIDDEN);
    let own = Method::OwnBlock { block_ids: vec![block.clone()], mode: Default::default() };
    assert_eq!(code(viewer.call_value(own).await), RpcError::FORBIDDEN, "viewers never own");
    wait_for("the view link in watchers", async || {
        let view: SessionView = host.call(Method::SessionView { session_id: session.clone() }).await.unwrap();
        view.watchers.iter().any(|w| w.principal == WatcherKind::Link && w.rights == Rights::View && w.blocks.contains(&block))
    })
    .await;

    // An interact link types.
    let (interact, interact_ticket) = link(&host, Scope::Session { session_id: session.clone() }, Rights::Interact).await;
    let (typist, _) = Control::connect_shared(&net, &interact_ticket, Some(host_addr.clone()), "Ben's Silo").await.expect("interact link admitted");
    assert_eq!(typist.hello().granted.as_ref().map(|g| g.rights), Some(Rights::Interact));
    let typing = DataStream::open(&typist, &block, None).await.unwrap();
    assert!(typing.send(&ClientData::Input(b"echo TYPED_$((40+2))\r".to_vec())));
    wait_for("typed output", async || capture(&host, &block).await.contains("TYPED_42")).await;
    let text = capture(&host, &block).await;
    assert!(!text.contains("VIEW_ONLY") && !text.contains("VIEW_SEND"), "view input reached the pty:\n{text}");
    let size: SizeInfo = host.call(Method::TerminalSize { block_id: block.clone() }).await.unwrap();
    assert_ne!((size.size.columns, size.size.rows), (20, 5), "a viewer never resizes");
    // A link is reusable: a second connection is admitted.
    let (again, _) = Control::connect_shared(&net, &view_ticket, Some(host_addr.clone()), "Ana's Silo").await.expect("link reused");

    // A link of one Silo is nothing to another.
    let refused = Control::connect_shared(&net, &view_ticket, Some(other_addr.clone()), "Ana's Silo").await;
    assert!(refused.err().expect("refused").downcast_ref::<silo_client::Unauthorized>().is_some(), "another server refuses the link");

    // Revoking closes it at once: control and data.
    host.call_value(Method::GrantRevoke { id: view.grant.id.clone() }).await.unwrap();
    timeout(T, viewer.closed()).await.expect("a revoked link's connection closes");
    timeout(T, again.closed()).await.expect("every connection of it closes");
    timeout(T, async { while looking.rx.recv().await.is_some() {} }).await.expect("its data stream ends");
    let refused = Control::connect_shared(&net, &view_ticket, Some(host_addr.clone()), "Ana's Silo").await;
    assert!(refused.err().expect("refused").downcast_ref::<silo_client::Unauthorized>().is_some(), "a revoked link is refused");
    assert!(!typist.is_closed(), "other links stay");
    host.call_value(Method::GrantRevoke { id: interact.grant.id.clone() }).await.unwrap();
    timeout(T, typist.closed()).await.expect("the interact link closes");
    drop(typing);

    // A block link can't session.view; it sees its block.
    let (_, block_ticket) = link(&host, Scope::Block { block_id: block.clone() }, Rights::View).await;
    let (watcher, _) = Control::connect_shared(&net, &block_ticket, Some(host_addr.clone()), "Cy's Silo").await.expect("block link admitted");
    assert_eq!(watcher.hello().granted.as_ref().map(|g| g.scope.clone()), Some(Scope::Block { block_id: block.clone() }));
    assert_eq!(code(watcher.call_value(Method::SessionView { session_id: session.clone() }).await), "not_found");
    let sessions: Vec<silo_proto::model::SessionSummary> = watcher.call(Method::SessionList {}).await.unwrap();
    assert!(sessions.is_empty(), "a block link lists no sessions");
    watcher.call_value(Method::BlockView { block_id: block.clone() }).await.unwrap();
    let mut watching = DataStream::open(&watcher, &block, None).await.unwrap();
    timeout(T, watching.rx.recv()).await.expect("snapshot").expect("data stream open");

    drop((watching, watcher));
    net.close().await.unwrap();
    shutdown(host, host_task).await;
    shutdown(other, other_task).await;
    let _ = std::fs::remove_dir_all(&host_home);
    let _ = std::fs::remove_dir_all(&other_home);
}
