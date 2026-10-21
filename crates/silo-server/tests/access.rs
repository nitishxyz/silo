//! Access rules end to end: scoped tokens and links, what view-only
//! streams may do, and revocation and expiry closing connections.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;
use silo_client::{Control, DataStream};
use silo_proto::{
    Frame,
    access::{Rights, Scope},
    control::{
        Capture, ClientHello, Created, DataTicket, GrantCreated, GrantKind, InvitePurpose, Method, PairInvite, Request, RpcError, ServerHello,
        ServerMessage, SpawnSpec,
    },
    data::{ClientData, DataHello},
    kind,
    model::{BlockKind, SessionSummary},
    paths, read_frame, write_frame,
};
use silo_transport::{BiStream, EndpointAddr, Identity, Net, PairTicket, RelayConfig, RemoteConn};
use tokio::{io::AsyncWriteExt, task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(10);

/// Hellos read the process-wide token: one connection set up at a time.
static HELLO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-access-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> JoinHandle<()> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        relay: RelayConfig::Disabled,
        mdns: false,
        grant_sweep: Duration::from_millis(200),
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

async fn connect(home: &Path, token: Option<&str>) -> Result<Control, anyhow::Error> {
    let _one = HELLO.lock().await;
    silo_client::set_token(token.map(str::to_owned));
    let connected = Control::connect(&paths::socket(home), "access-test").await.map(|(c, _)| c);
    silo_client::set_token(None);
    connected
}

async fn shutdown(control: Control, task: JoinHandle<()>) {
    control.call_value(Method::ServerShutdown {}).await.unwrap();
    drop(control);
    timeout(T, task).await.unwrap().unwrap();
}

fn shell() -> SpawnSpec {
    SpawnSpec { cwd: None, command: vec!["/bin/sh".into()], size: Some(silo_proto::model::TermSize::new(80, 24)) }
}

async fn token(owner: &Control, scope: Scope, rights: Rights, ttl_secs: Option<u64>, kind: GrantKind) -> GrantCreated {
    owner.call(Method::GrantCreate { scope, rights, kind, ttl_secs, label: Some("test".into()) }).await.unwrap()
}

fn code(result: anyhow::Result<Value>) -> String {
    let error = result.unwrap_err();
    if error.downcast_ref::<silo_client::Unauthorized>().is_some() {
        return RpcError::UNAUTHORIZED.to_owned();
    }
    error.downcast::<RpcError>().map(|e| e.code).unwrap_or_else(|e| format!("{e:#}"))
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

/// The stream's frames until it closes.
async fn ends(stream: &mut DataStream) {
    timeout(T, async { while stream.rx.recv().await.is_some() {} }).await.expect("data stream closes");
}

#[tokio::test(flavor = "multi_thread")]
async fn scoped_tokens_see_and_do_only_what_they_cover() {
    let home = temp_home("scope");
    let task = start(&home).await;
    let owner = connect(&home, None).await.unwrap();
    let new = || Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None };
    let mine: Created = owner.call(new()).await.unwrap();
    let other: Created = owner.call(new()).await.unwrap();

    assert_eq!(code(connect(&home, Some("silot1aaaa")).await.map(|_| Value::Null)), RpcError::UNAUTHORIZED, "an unknown token is refused");

    let session = Scope::Session { session_id: mine.session_id.clone() };
    let view = token(&owner, session.clone(), Rights::View, None, GrantKind::Token).await;
    let interact = token(&owner, session.clone(), Rights::Interact, None, GrantKind::Token).await;
    let block = token(&owner, Scope::Block { block_id: mine.block_id.clone() }, Rights::View, None, GrantKind::Token).await;
    assert!(view.secret.starts_with("silot1") && view.ticket.is_none());
    let viewer = connect(&home, Some(&view.secret)).await.unwrap();
    let typist = connect(&home, Some(&interact.secret)).await.unwrap();
    let watcher = connect(&home, Some(&block.secret)).await.unwrap();

    let listed: Vec<SessionSummary> = viewer.call(Method::SessionList {}).await.unwrap();
    assert_eq!(listed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), [mine.session_id.as_str()]);
    let listed: Vec<SessionSummary> = watcher.call(Method::SessionList {}).await.unwrap();
    assert!(listed.is_empty(), "a block share sees no sessions");
    assert_eq!(code(watcher.call_value(Method::SessionView { session_id: mine.session_id.clone() }).await), "not_found");
    watcher.call_value(Method::BlockView { block_id: mine.block_id.clone() }).await.unwrap();
    assert_eq!(code(viewer.call_value(Method::SessionView { session_id: other.session_id.clone() }).await), "not_found");
    assert_eq!(code(viewer.call_value(Method::TerminalSend { block_id: mine.block_id.clone(), text: "x".into() }).await), RpcError::FORBIDDEN);
    assert_eq!(code(typist.call_value(Method::BlockClose { block_id: mine.block_id.clone() }).await), RpcError::FORBIDDEN);
    assert_eq!(code(typist.call_value(new()).await), RpcError::FORBIDDEN);
    assert_eq!(code(typist.call_value(Method::GrantList {}).await), RpcError::FORBIDDEN);
    let own = Method::OwnBlock { block_ids: vec![mine.block_id.clone()], mode: Default::default() };
    assert_eq!(code(viewer.call_value(own.clone()).await), RpcError::FORBIDDEN, "viewers never own");
    typist.call_value(own).await.unwrap();

    // A view stream's input never reaches the program; an interact one's does.
    let looking = DataStream::open(&viewer, &mine.block_id, None).await.unwrap();
    assert!(looking.send(&ClientData::Input(b"echo VIEW_ONLY_INPUT\r".to_vec())));
    assert!(looking.send(&ClientData::Resize(silo_proto::model::TermSize::new(20, 5))));
    let typing = DataStream::open(&typist, &mine.block_id, None).await.unwrap();
    assert!(typing.send(&ClientData::Input(b"echo TYPED_$((40+2))\r".to_vec())));
    wait_for("typed output", async || capture(&owner, &mine.block_id).await.contains("TYPED_42")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!capture(&owner, &mine.block_id).await.contains("VIEW_ONLY"), "view input reached the pty");
    let size: silo_proto::control::SizeInfo = owner.call(Method::TerminalSize { block_id: mine.block_id.clone() }).await.unwrap();
    assert_ne!(size.size.columns, 20, "a viewer never resizes");

    // Watchers are listed with their principal and rights.
    let view_of: silo_proto::model::SessionView = owner.call(Method::SessionView { session_id: mine.session_id.clone() }).await.unwrap();
    let mut rights: Vec<Rights> = view_of.watchers.iter().map(|w| w.rights).collect();
    rights.sort();
    assert_eq!(rights, [Rights::View, Rights::Interact]);
    assert!(view_of.watchers.iter().all(|w| w.principal == silo_proto::model::WatcherKind::Token));

    // Closing the session removes the grants scoped to it.
    owner.call_value(Method::SessionClose { session_id: mine.session_id.clone() }).await.unwrap();
    timeout(T, viewer.closed()).await.expect("a closed session's grant closes its connections");
    let grants: silo_proto::control::Grants = owner.call(Method::GrantList {}).await.unwrap();
    assert!(grants.grants.is_empty(), "{:?}", grants.grants);
    drop((looking, typing));
    shutdown(owner, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_and_expired_grants_close_control_and_data_connections() {
    let home = temp_home("revoke");
    let task = start(&home).await;
    let owner = connect(&home, None).await.unwrap();
    let created: Created = owner.call(Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await.unwrap();

    // Revocation: at once.
    let revoked = token(&owner, Scope::Server, Rights::Interact, None, GrantKind::Token).await;
    let control = connect(&home, Some(&revoked.secret)).await.unwrap();
    let mut stream = DataStream::open(&control, &created.block_id, None).await.unwrap();
    owner.call_value(Method::GrantRevoke { id: revoked.grant.id.clone() }).await.unwrap();
    timeout(T, control.closed()).await.expect("control closes");
    ends(&mut stream).await;
    assert_eq!(code(connect(&home, Some(&revoked.secret)).await.map(|_| Value::Null)), RpcError::UNAUTHORIZED);

    // Expiry: within one sweep (60 s in production, 200 ms here).
    let expiring = token(&owner, Scope::Server, Rights::View, Some(1), GrantKind::Token).await;
    let control = connect(&home, Some(&expiring.secret)).await.unwrap();
    let mut stream = DataStream::open(&control, &created.block_id, None).await.unwrap();
    timeout(T, control.closed()).await.expect("an expired grant's control closes");
    ends(&mut stream).await;

    // A share link over iroh: admitted by its secret, closed on revoke.
    let link = token(&owner, Scope::Session { session_id: created.session_id.clone() }, Rights::View, None, GrantKind::Link).await;
    assert_eq!(link.grant.expires_at.map(|at| at - link.grant.created_at), Some(silo_proto::access::DEFAULT_LINK_TTL_SECS));
    let addr = loopback(&owner).await;
    let net = Net::bind(&Identity::load_or_create(home.join("viewer")).unwrap(), RelayConfig::Disabled, false).await.unwrap();
    let rc = net.dial(addr.clone()).await.unwrap();
    let mut remote =
        hello(&rc, &ClientHello { grant: Some(link.secret.clone()), ..ClientHello::ours("viewer", false, None) }).await.expect("link admitted");
    remote.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    assert_eq!(
        remote.call(Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await.unwrap_err().code,
        RpcError::FORBIDDEN
    );
    let ticket: DataTicket = serde_json::from_value(remote.call(Method::DataConnect { block_id: created.block_id.clone() }).await.unwrap()).unwrap();
    let mut data = remote.rc.open_stream().await.unwrap();
    write_frame(&mut data, &Frame::json(kind::DATA_HELLO, &DataHello::ours(ticket.ticket, None)).unwrap()).await.unwrap();
    data.flush().await.unwrap();
    assert_eq!(timeout(T, read_frame(&mut data)).await.unwrap().unwrap().map(|f| f.kind), Some(kind::WELCOME));
    // A link is a reusable bearer secret: a second connection is admitted.
    let rc2 = net.dial(addr.clone()).await.unwrap();
    hello(&rc2, &ClientHello { grant: Some(link.secret.clone()), ..ClientHello::ours("viewer", false, None) }).await.expect("link reused");
    owner.call_value(Method::GrantRevoke { id: link.grant.id.clone() }).await.unwrap();
    timeout(T, rc.closed()).await.expect("a revoked link's connection closes");
    timeout(T, rc2.closed()).await.expect("every connection of it closes");
    let rc = net.dial(addr).await.unwrap();
    let refused = hello(&rc, &ClientHello { grant: Some(link.secret.clone()), ..ClientHello::ours("viewer", false, None) }).await;
    assert_eq!(refused.err().map(|e| e.code).as_deref(), Some(RpcError::UNAUTHORIZED));
    net.close().await.unwrap();
    shutdown(owner, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

/// The server's iroh address on loopback (from a pairing ticket).
async fn loopback(owner: &Control) -> EndpointAddr {
    for _ in 0..200 {
        let invite: PairInvite = owner.call(Method::PairCreate { ttl_secs: 60, purpose: InvitePurpose::Device }).await.unwrap();
        let ticket = PairTicket::decode(&invite.ticket).unwrap();
        if let Some(port) = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port) {
            return EndpointAddr::new(ticket.addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server endpoint has no direct address");
}

struct Ctl {
    rc: RemoteConn,
    stream: BiStream,
    next: u64,
}

impl Ctl {
    async fn call(&mut self, method: Method) -> Result<Value, RpcError> {
        self.next += 1;
        let id = self.next;
        write_frame(&mut self.stream, &Frame::json(kind::CONTROL, &Request { id, method }).unwrap()).await.unwrap();
        self.stream.flush().await.unwrap();
        loop {
            let frame = timeout(T, read_frame(&mut self.stream)).await.unwrap().unwrap().expect("control stream open");
            if let ServerMessage::Response { id: got, result, error } = frame.parse().unwrap()
                && got == id
            {
                return error.map_or_else(|| Ok(result.unwrap_or(Value::Null)), Err);
            }
        }
    }
}

async fn hello(rc: &RemoteConn, hello: &ClientHello) -> Result<Ctl, RpcError> {
    let mut stream = rc.open_stream().await.unwrap();
    write_frame(&mut stream, &Frame::json(kind::CONTROL_HELLO, hello).unwrap()).await.unwrap();
    stream.flush().await.unwrap();
    let frame = timeout(T, read_frame(&mut stream)).await.unwrap().unwrap().expect("hello reply");
    if frame.kind == kind::CONTROL_HELLO {
        let _: ServerHello = frame.parse().unwrap();
        return Ok(Ctl { rc: rc.clone(), stream, next: 0 });
    }
    match frame.parse::<ServerMessage>().unwrap() {
        ServerMessage::Response { error: Some(error), .. } => Err(error),
        other => panic!("unexpected {other:?}"),
    }
}
