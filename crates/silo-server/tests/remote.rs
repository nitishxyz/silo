//! Remote access over iroh: pairing, authorization, permissions and data
//! ticket binding. Relays and mDNS are disabled; peers dial loopback.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;
use silo_client::{Control, Events};
use silo_proto::{
    Frame, PROTOCOL,
    control::{
        Capture, ClientHello, Created, DataTicket, Event, HostOffer, InvitePurpose, Method, OfferSource, PairInvite, PairedDevices, Request,
        RpcError, ServerHello, ServerMessage, SpawnSpec,
    },
    data::{ClientData, Clip, DataHello, ServerData},
    kind,
    model::TermSize,
    paths, read_frame, write_frame,
};
use silo_server::sysclip::Fake;
use silo_transport::{BiStream, EndpointAddr, Identity, Net, PairTicket, RelayConfig, RemoteConn};
use tokio::{io::AsyncWriteExt, task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(10);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-remote-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> JoinHandle<()> {
    start_with(home, None).await
}

async fn start_with(home: &Path, system_clipboard: Option<std::sync::Arc<dyn silo_server::sysclip::SystemClipboard>>) -> JoinHandle<()> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: RelayConfig::Disabled,
        mdns: false,
        system_clipboard,
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

async fn owner(home: &Path) -> Control {
    Control::connect(&paths::socket(home), "owner").await.unwrap().0
}

async fn shutdown(control: Control, task: JoinHandle<()>) {
    control.call_value(Method::ServerShutdown {}).await.unwrap();
    drop(control);
    timeout(T, task).await.unwrap().unwrap();
}

/// A fresh invite and the server's loopback address.
async fn invite(control: &Control, ttl_secs: u64) -> (String, EndpointAddr) {
    let (invite, addr) = invite_for(control, ttl_secs, InvitePurpose::Device).await;
    (invite.ticket, addr)
}

async fn invite_for(control: &Control, ttl_secs: u64, purpose: InvitePurpose) -> (PairInvite, EndpointAddr) {
    for _ in 0..200 {
        let invite: PairInvite = control.call(Method::PairCreate { ttl_secs, purpose }).await.unwrap();
        let ticket = PairTicket::decode(&invite.ticket).unwrap();
        if let Some(port) = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port) {
            let addr = EndpointAddr::new(ticket.addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into());
            return (invite, addr);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server endpoint has no direct address");
}

async fn device(home: &Path, name: &str) -> Net {
    let id = Identity::load_or_create(home.join(format!("device-{name}"))).unwrap();
    Net::bind(&id, RelayConfig::Disabled, false).await.unwrap()
}

async fn send(stream: &mut BiStream, frame: &Frame) {
    write_frame(stream, frame).await.unwrap();
    stream.flush().await.unwrap();
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
        send(&mut self.stream, &Frame::json(kind::CONTROL, &Request { id, method }).unwrap()).await;
        loop {
            let frame = timeout(T, read_frame(&mut self.stream)).await.unwrap().unwrap().expect("control stream open");
            if let ServerMessage::Response { id: got, result, error } = frame.parse().unwrap()
                && got == id
            {
                return error.map_or_else(|| Ok(result.unwrap_or(Value::Null)), Err);
            }
        }
    }

    /// Open a data stream with `ticket`: the server's first frame kind.
    async fn data(&self, ticket: &str) -> Option<u8> {
        data_on(&self.rc, ticket).await
    }
}

async fn data_on(rc: &RemoteConn, ticket: &str) -> Option<u8> {
    let mut stream = rc.open_stream().await.unwrap();
    let hello = DataHello::ours(ticket.into(), None);
    send(&mut stream, &Frame::json(kind::DATA_HELLO, &hello).unwrap()).await;
    timeout(T, read_frame(&mut stream)).await.unwrap().ok().flatten().map(|f| f.kind)
}

/// A hello speaking `protocol`, from a build other than the server's: only
/// the protocol decides admission.
async fn hello_on(rc: &RemoteConn, protocol: u32, invite: Option<&str>) -> (BiStream, Result<ServerHello, RpcError>) {
    let mut stream = rc.open_stream().await.unwrap();
    let hello = ClientHello { protocol, build: "another-build".into(), ..ClientHello::ours("remote-test", false, invite.map(Into::into)) };
    send(&mut stream, &Frame::json(kind::CONTROL_HELLO, &hello).unwrap()).await;
    let reply = match timeout(T, read_frame(&mut stream)).await.unwrap() {
        Ok(Some(frame)) if frame.kind == kind::CONTROL_HELLO => Ok(frame.parse().unwrap()),
        Ok(Some(frame)) => match frame.parse().unwrap() {
            ServerMessage::Response { error: Some(error), .. } => Err(error),
            other => panic!("unexpected {other:?}"),
        },
        _ => Err(RpcError::new("closed", "stream closed")),
    };
    (stream, reply)
}

async fn connect(net: &Net, addr: &EndpointAddr, protocol: u32, invite: Option<&str>) -> Result<Ctl, RpcError> {
    let rc = timeout(T, net.dial(addr.clone())).await.unwrap().unwrap();
    let (stream, reply) = hello_on(&rc, protocol, invite).await;
    reply.map(|_| Ctl { rc, stream, next: 0 })
}

fn shell() -> SpawnSpec {
    SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) }
}

async fn ticket(ctl: &mut Ctl, block: &str) -> String {
    let t: DataTicket = serde_json::from_value(ctl.call(Method::DataConnect { block_id: block.into() }).await.unwrap()).unwrap();
    t.ticket
}

async fn create(ctl: &mut Ctl) -> Created {
    serde_json::from_value(
        ctl.call(Method::SessionCreate { name: None, spawn: shell(), kind: silo_proto::model::BlockKind::Terminal, url: None }).await.unwrap(),
    )
    .unwrap()
}

fn code(result: Result<impl std::fmt::Debug, RpcError>) -> String {
    result.expect_err("expected an error").code
}

#[tokio::test(flavor = "multi_thread")]
async fn control_and_data_over_remote() {
    let home = temp_home("basic");
    let task = start(&home).await;
    let control = owner(&home).await;
    let (invite, addr) = invite(&control, 60).await;
    let net = device(&home, "a").await;
    let mut ctl = connect(&net, &addr, PROTOCOL, Some(&invite)).await.unwrap();

    let info: silo_proto::control::ServerInfo = serde_json::from_value(ctl.call(Method::ServerInfo {}).await.unwrap()).unwrap();
    assert_eq!(info.endpoint_id, addr.id.to_string());
    let created = create(&mut ctl).await;
    let t = ticket(&mut ctl, &created.block_id).await;
    assert_eq!(ctl.data(&t).await, Some(kind::WELCOME));
    // Single use.
    assert_eq!(ctl.data(&t).await, Some(kind::REJECT));

    // A second control stream on the same connection gets no reply.
    let (_, second) = hello_on(&ctl.rc, PROTOCOL, None).await;
    assert_eq!(second.unwrap_err().code, "closed");

    let devices: PairedDevices = control.call(Method::PairList {}).await.unwrap();
    assert_eq!(devices.devices.len(), 1);
    assert_eq!(devices.devices[0].id, net.id().to_string());
    assert_eq!(devices.devices[0].name, "remote-test");
    drop(ctl);
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_device_denied() {
    let home = temp_home("unknown");
    let task = start(&home).await;
    let control = owner(&home).await;
    let (_, addr) = invite(&control, 60).await;
    let net = device(&home, "x").await;
    assert_eq!(code(connect(&net, &addr, PROTOCOL, None).await.map(|_| ())), RpcError::UNAUTHORIZED);
    assert_eq!(code(connect(&net, &addr, PROTOCOL, Some("silo1garbage")).await.map(|_| ())), RpcError::UNAUTHORIZED);
    let forged = PairTicket { addr: addr.clone(), secret: [7; 32] }.encode();
    assert_eq!(code(connect(&net, &addr, PROTOCOL, Some(&forged)).await.map(|_| ())), RpcError::UNAUTHORIZED);
    let devices: PairedDevices = control.call(Method::PairList {}).await.unwrap();
    assert!(devices.devices.is_empty());
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_expiry_and_replay_rejected() {
    let home = temp_home("invite");
    let task = start(&home).await;
    let control = owner(&home).await;
    let (short, addr) = invite(&control, 1).await;
    let (once, _) = invite(&control, 60).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let a = device(&home, "a").await;
    assert_eq!(code(connect(&a, &addr, PROTOCOL, Some(&short)).await.map(|_| ())), RpcError::UNAUTHORIZED);
    connect(&a, &addr, PROTOCOL, Some(&once)).await.unwrap();
    let b = device(&home, "b").await;
    assert_eq!(code(connect(&b, &addr, PROTOCOL, Some(&once)).await.map(|_| ())), RpcError::UNAUTHORIZED);
    a.close().await.unwrap();
    b.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn paired_device_is_not_owner() {
    let home = temp_home("perm");
    let task = start(&home).await;
    let control = owner(&home).await;
    let (invite, addr) = invite(&control, 60).await;
    let net = device(&home, "a").await;
    let mut ctl = connect(&net, &addr, PROTOCOL, Some(&invite)).await.unwrap();
    assert_eq!(code(ctl.call(Method::PairCreate { ttl_secs: 60, purpose: silo_proto::control::InvitePurpose::Device }).await), RpcError::FORBIDDEN);
    assert_eq!(code(ctl.call(Method::PairList {}).await), RpcError::FORBIDDEN);
    assert_eq!(code(ctl.call(Method::PairRevoke { id: net.id().to_string() }).await), RpcError::FORBIDDEN);
    assert_eq!(code(ctl.call(Method::ServerShutdown {}).await), RpcError::FORBIDDEN);
    // Still serving.
    ctl.call(Method::SessionList {}).await.unwrap();
    control.call_value(Method::SessionList {}).await.unwrap();
    drop(ctl);
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn data_ticket_bound_to_its_connection() {
    let home = temp_home("ticket");
    let task = start(&home).await;
    let control = owner(&home).await;
    let (ia, addr) = invite(&control, 60).await;
    let (ib, _) = invite(&control, 60).await;
    let (a, b) = (device(&home, "a").await, device(&home, "b").await);
    let mut ca = connect(&a, &addr, PROTOCOL, Some(&ia)).await.unwrap();
    let cb = connect(&b, &addr, PROTOCOL, Some(&ib)).await.unwrap();
    let created = create(&mut ca).await;
    let t = ticket(&mut ca, &created.block_id).await;
    assert_eq!(cb.data(&t).await, Some(kind::REJECT));
    // Not consumed: the issuing connection still redeems it.
    assert_eq!(ca.data(&t).await, Some(kind::WELCOME));
    drop((ca, cb));
    a.close().await.unwrap();
    b.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn revocation_closes_live_access() {
    let home = temp_home("revoke");
    let task = start(&home).await;
    let control = owner(&home).await;
    let (invite, addr) = invite(&control, 60).await;
    let net = device(&home, "a").await;
    let mut ctl = connect(&net, &addr, PROTOCOL, Some(&invite)).await.unwrap();
    let created = create(&mut ctl).await;
    let t = ticket(&mut ctl, &created.block_id).await;
    let mut data = ctl.rc.open_stream().await.unwrap();
    send(&mut data, &Frame::json(kind::DATA_HELLO, &DataHello::ours(t, None)).unwrap()).await;
    assert_eq!(timeout(T, read_frame(&mut data)).await.unwrap().unwrap().unwrap().kind, kind::WELCOME);

    control.call_value(Method::PairRevoke { id: net.id().to_string() }).await.unwrap();
    timeout(T, ctl.rc.closed()).await.unwrap();
    // Drain whatever was in flight; the stream must end.
    timeout(T, async { while let Ok(Some(_)) = read_frame(&mut data).await {} }).await.unwrap();
    assert_eq!(code(connect(&net, &addr, PROTOCOL, None).await.map(|_| ())), RpcError::UNAUTHORIZED);
    let devices: PairedDevices = control.call(Method::PairList {}).await.unwrap();
    assert!(devices.devices.is_empty());
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incompatible_protocol_does_not_enroll() {
    let home = temp_home("build");
    let task = start(&home).await;
    let control = owner(&home).await;
    let (invite, addr) = invite(&control, 60).await;
    let net = device(&home, "a").await;
    assert_eq!(code(connect(&net, &addr, PROTOCOL + 1, Some(&invite)).await.map(|_| ())), RpcError::INCOMPATIBLE);
    // Before protocol numbers (no `protocol` field): incompatible too.
    assert_eq!(code(connect(&net, &addr, 0, Some(&invite)).await.map(|_| ())), RpcError::INCOMPATIBLE);
    let devices: PairedDevices = control.call(Method::PairList {}).await.unwrap();
    assert!(devices.devices.is_empty());
    // The invite was not consumed.
    connect(&net, &addr, PROTOCOL, Some(&invite)).await.unwrap();
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grants_survive_restart() {
    let home = temp_home("restart");
    let task = start(&home).await;
    let control = owner(&home).await;
    let (invite, _) = invite(&control, 60).await;
    let net = device(&home, "a").await;
    let first_addr = {
        let (_, addr) = self::invite(&control, 60).await;
        addr
    };
    drop(connect(&net, &first_addr, PROTOCOL, Some(&invite)).await.unwrap());
    shutdown(control, task).await;

    let task = start(&home).await;
    let control = owner(&home).await;
    let (_, addr) = self::invite(&control, 60).await;
    assert_eq!(addr.id, first_addr.id, "identity persists");
    let mut ctl = connect(&net, &addr, PROTOCOL, None).await.unwrap();
    ctl.call(Method::SessionList {}).await.unwrap();
    drop(ctl);
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grants_and_identity_survive_takeover() {
    let home = temp_home("takeover");
    let old = start(&home).await;
    let control = owner(&home).await;
    let (invite, addr) = invite(&control, 60).await;
    let net = device(&home, "a").await;
    let ctl = connect(&net, &addr, PROTOCOL, Some(&invite)).await.unwrap();

    let config = silo_server::Config {
        home: home.clone(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: RelayConfig::Disabled,
        mdns: false,
        ..Default::default()
    };
    let successor = tokio::spawn(silo_server::takeover(config, false));
    timeout(T, old).await.unwrap().unwrap();
    // The old endpoint closed its remote connections at commit.
    timeout(T, ctl.rc.closed()).await.unwrap();
    drop(control);

    let control = owner(&home).await;
    let (_, new_addr) = self::invite(&control, 60).await;
    assert_eq!(new_addr.id, addr.id);
    let mut ctl = connect(&net, &new_addr, PROTOCOL, None).await.unwrap();
    ctl.call(Method::SessionList {}).await.unwrap();
    drop(ctl);
    net.close().await.unwrap();
    control.call_value(Method::ServerShutdown {}).await.unwrap();
    timeout(T, successor).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_ttl_is_bounded_and_overflow_is_harmless() {
    let home = temp_home("ttl");
    let task = start(&home).await;
    let control = owner(&home).await;
    for ttl_secs in [0, 7 * 24 * 3600 + 1, u64::MAX] {
        let error = control.call_value(Method::PairCreate { ttl_secs, purpose: silo_proto::control::InvitePurpose::Device }).await.unwrap_err();
        assert!(error.to_string().contains("bad_request"), "{ttl_secs}: {error}");
    }
    // Remote access still works afterwards.
    let (invite, addr) = invite(&control, 7 * 24 * 3600).await;
    let net = device(&home, "a").await;
    let mut ctl = connect(&net, &addr, PROTOCOL, Some(&invite)).await.unwrap();
    let info: silo_proto::control::ServerInfo = serde_json::from_value(ctl.call(Method::ServerInfo {}).await.unwrap()).unwrap();
    assert_eq!(info.remote_error, None);
    drop(ctl);
    net.close().await.unwrap();
    shutdown(control, task).await;
}

/// Dial and make the connection visible to the server with a partial hello
/// (a length prefix only), leaving it unauthenticated.
async fn pending(net: &Net, addr: &EndpointAddr) -> (RemoteConn, BiStream) {
    let rc = timeout(T, net.dial(addr.clone())).await.unwrap().unwrap();
    let mut stream = rc.open_stream().await.unwrap();
    stream.write_all(&100u32.to_le_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    (rc, stream)
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_connections_are_limited() {
    let home = temp_home("preauth");
    let task = start(&home).await;
    let control = owner(&home).await;
    let (_, addr) = invite(&control, 60).await;
    let net = device(&home, "x").await;

    // No data streams before the hello is admitted: a data hello gets no
    // reply at all (not even a ticket rejection).
    let (rc, _first) = pending(&net, &addr).await;
    let mut data = rc.open_stream().await.unwrap();
    send(&mut data, &Frame::json(kind::DATA_HELLO, &DataHello::ours("00".into(), None)).unwrap()).await;
    assert!(timeout(Duration::from_millis(500), read_frame(&mut data)).await.is_err());

    // An oversized hello is refused without being read.
    let rc2 = timeout(T, net.dial(addr.clone())).await.unwrap().unwrap();
    let mut big = rc2.open_stream().await.unwrap();
    big.write_all(&(1u32 << 20).to_le_bytes()).await.unwrap();
    big.flush().await.unwrap();
    assert!(matches!(timeout(T, read_frame(&mut big)).await.unwrap(), Ok(None) | Err(_)));
    timeout(T, rc2.closed()).await.unwrap();

    // Per-peer cap: with `rc` and three more pending, the next is closed.
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(pending(&net, &addr).await);
    }
    let (extra, _s) = pending(&net, &addr).await;
    timeout(T, extra.closed()).await.unwrap();
    drop((rc, held));
    net.close().await.unwrap();
    shutdown(control, task).await;
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap().local_addr().unwrap().port()
}

fn port_config(home: &Path, port: u16) -> silo_server::Config {
    silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: RelayConfig::Disabled,
        mdns: false,
        port: Some(port),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fixed_port_survives_takeover_and_bind_failure_is_reported() {
    let home = temp_home("port");
    let port = free_udp_port();
    let old = tokio::spawn(silo_server::run(port_config(&home, port)));
    for _ in 0..500 {
        if silo_client::server_running(&home).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let control = owner(&home).await;
    let (invite, addr) = invite(&control, 60).await;
    assert_eq!(addr.ip_addrs().next().unwrap().port(), port);
    // server.info carries the live address, decodable as an EndpointAddr.
    let info: silo_proto::control::ServerInfo = control.call(Method::ServerInfo {}).await.unwrap();
    let live: EndpointAddr = serde_json::from_value(info.endpoint_addr.expect("endpoint_addr")).unwrap();
    assert_eq!(live.id, addr.id);
    assert!(live.ip_addrs().all(|a| a.port() == port) && live.ip_addrs().next().is_some(), "{live:?}");
    drop(control);

    // The successor rebinds the same port once the old endpoint closed.
    let successor = tokio::spawn(silo_server::takeover(port_config(&home, port), false));
    timeout(T, old).await.unwrap().unwrap().unwrap();
    let control = owner(&home).await;
    let (fresh, again) = self::invite(&control, 60).await;
    assert_eq!(again, addr);
    let net = device(&home, "a").await;
    // Invites live in memory only: the old server's is gone.
    assert_eq!(code(connect(&net, &addr, PROTOCOL, Some(&invite)).await.map(|_| ())), RpcError::UNAUTHORIZED);
    drop(connect(&net, &addr, PROTOCOL, Some(&fresh)).await.unwrap());
    drop(control);

    // A successor that cannot bind keeps serving unix and says why.
    let busy = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    let busy_port = busy.local_addr().unwrap().port();
    let third = tokio::spawn(silo_server::takeover(port_config(&home, busy_port), false));
    timeout(T, successor).await.unwrap().unwrap().unwrap();
    let control = owner(&home).await;
    let info: silo_proto::control::ServerInfo = control.call(Method::ServerInfo {}).await.unwrap();
    assert!(info.remote_error.is_some(), "{info:?}");
    assert!(info.endpoint_addr.is_none(), "{info:?}");
    let error = control.call_value(Method::PairCreate { ttl_secs: 60, purpose: silo_proto::control::InvitePurpose::Device }).await.unwrap_err();
    assert!(error.to_string().contains("remote access is not running"), "{error}");
    net.close().await.unwrap();
    control.call_value(Method::ServerShutdown {}).await.unwrap();
    timeout(T, third).await.unwrap().unwrap().unwrap();
}

/// A valid pairing ticket of some other machine.
fn other_host(home: &Path) -> String {
    let id = Identity::load_or_create(home.join("other-host")).unwrap().id();
    PairTicket { addr: EndpointAddr::new(id), secret: [1; 32] }.encode()
}

async fn next_offer(events: &mut Events) -> HostOffer {
    loop {
        let (_, event) = timeout(T, events.recv()).await.unwrap().expect("events");
        if let Event::HostOffered { offer } = event {
            return offer;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn join_invite_only_offers_a_host_once() {
    let home = temp_home("join");
    let task = start(&home).await;
    let (control, mut events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    control.subscribe().await.unwrap();
    // A paired device subscribed to events must not see offers (they
    // carry pairing tickets).
    let (paired_invite, addr) = invite(&control, 60).await;
    let paired_net = device(&home, "paired").await;
    let paired_ticket = PairTicket { addr, secret: PairTicket::decode(&paired_invite).unwrap().secret };
    let (paired, mut paired_events) =
        Control::connect_remote(&paired_net, paired_ticket.addr.clone(), "paired", false, Some(&paired_ticket)).await.unwrap();
    paired.subscribe().await.unwrap();
    let (join, addr) = invite_for(&control, 60, InvitePurpose::Join).await;
    let offered = other_host(&home);
    let net = device(&home, "a").await;
    let mut ctl = connect(&net, &addr, PROTOCOL, Some(&join.ticket)).await.unwrap();
    assert_eq!(code(ctl.call(Method::SessionList {}).await), RpcError::UNAUTHORIZED);
    assert_eq!(code(ctl.call(Method::PairList {}).await), RpcError::UNAUTHORIZED);
    assert_eq!(code(ctl.call(Method::HostOffer { ticket: "silo1garbage".into(), name: "far".into() }).await), "bad_request");
    ctl.call(Method::HostOffer { ticket: offered.clone(), name: " far\n".into() }).await.unwrap();
    let offer = next_offer(&mut events).await;
    let endpoint_id = PairTicket::decode(&offered).unwrap().addr.id.to_string();
    assert_eq!(
        offer,
        HostOffer { ticket: offered.clone(), name: "far".into(), endpoint_id, source: OfferSource::Join { invite_id: join.invite_id } }
    );
    // The connection ends, the invite is spent and nothing was paired.
    assert!(timeout(T, read_frame(&mut ctl.stream)).await.unwrap().map_or(true, |f| f.is_none()));
    assert_eq!(code(connect(&net, &addr, PROTOCOL, Some(&join.ticket)).await.map(|_| ())), RpcError::UNAUTHORIZED);
    let devices: PairedDevices = control.call(Method::PairList {}).await.unwrap();
    assert_eq!(devices.devices.len(), 1, "only the paired device");
    let leaked = timeout(Duration::from_millis(300), next_offer(&mut paired_events)).await;
    assert!(leaked.is_err(), "{leaked:?}");
    drop(paired);
    paired_net.close().await.unwrap();

    // The client helper, on a fresh join invite.
    let (join, addr) = invite_for(&control, 60, InvitePurpose::Join).await;
    let ticket = PairTicket { addr, secret: PairTicket::decode(&join.ticket).unwrap().secret };
    silo_client::offer_host(&net, &ticket, &offered, "far").await.unwrap();
    assert_eq!(next_offer(&mut events).await.source, OfferSource::Join { invite_id: join.invite_id });
    // A device invite does not admit host.offer.
    let (device_invite, addr) = invite(&control, 60).await;
    let ticket = PairTicket { addr, secret: PairTicket::decode(&device_invite).unwrap().secret };
    assert!(silo_client::offer_host(&net, &ticket, &offered, "far").await.is_err());
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn printed_offer_is_raised_once_and_stripped() {
    let home = temp_home("osc");
    let task = start(&home).await;
    let (control, mut events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    control.subscribe().await.unwrap();
    let offered = other_host(&home);
    let osc = silo_proto::offer::osc(&offered, "far").replace('\x1b', "\\033").replace('\x07', "\\007");
    let script = format!("printf 'before{osc}'; printf '{osc}after\\n'; exec sleep 30");
    let spawn = SpawnSpec { command: vec!["/bin/sh".into(), "-c".into(), script], cwd: Some("/tmp".into()), size: Some(TermSize::new(200, 24)) };
    let created: Created =
        control.call(Method::SessionCreate { name: None, spawn, kind: silo_proto::model::BlockKind::Terminal, url: None }).await.unwrap();
    let offer = next_offer(&mut events).await;
    assert_eq!(offer.source, OfferSource::Terminal { block_id: created.block_id.clone() });
    assert_eq!((offer.ticket.as_str(), offer.name.as_str()), (offered.as_str(), "far"));
    let mut text = String::new();
    for _ in 0..200 {
        let capture: Capture = control.call(Method::TerminalCapture { block_id: created.block_id.clone(), history: true }).await.unwrap();
        text = capture.text;
        if text.contains("after") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(text.contains("beforeafter") && !text.contains("7717") && !text.contains("silo1"), "{text:?}");
    // The same ticket again is not raised twice.
    let again = timeout(Duration::from_millis(500), next_offer(&mut events)).await;
    assert!(again.is_err(), "{again:?}");
    shutdown(control, task).await;
}

/// A remote data stream's next clipboard write, or `None` within `wait`.
async fn remote_clip_write(stream: &mut BiStream, wait: Duration) -> Option<String> {
    timeout(wait, async {
        loop {
            let frame = read_frame(&mut *stream).await.unwrap().expect("data stream open");
            if let Ok(ServerData::ClipboardWrite { text, .. }) = ServerData::from_frame(frame) {
                return text;
            }
        }
    })
    .await
    .ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_pastes_reach_the_host_clipboard_and_host_copies_come_back() {
    let home = temp_home("sysclip");
    let fake = std::sync::Arc::new(Fake::with_delay(Duration::from_millis(300)));
    let task = start_with(&home, Some(fake.clone())).await;
    let control = owner(&home).await;
    let (invite, addr) = invite(&control, 60).await;
    let net = device(&home, "a").await;
    let mut ctl = connect(&net, &addr, PROTOCOL, Some(&invite)).await.unwrap();
    let created = create(&mut ctl).await;
    let block = created.block_id;

    // A push lands before the input after it is processed.
    let mut data = ctl.rc.open_stream().await.unwrap();
    let hello = DataHello::ours(ticket(&mut ctl, &block).await, None);
    send(&mut data, &Frame::json(kind::DATA_HELLO, &hello).unwrap()).await;
    let pasted = Clip::Text("pasted".into());
    send(&mut data, &ClientData::ClipboardPush { content: pasted.clone() }.to_frame()).await;
    send(&mut data, &ClientData::Input(b"echo mark$((40+2))\n".to_vec()).to_frame()).await;
    let mut seen = String::new();
    timeout(T, async {
        while !seen.contains("mark42") {
            let frame = read_frame(&mut data).await.unwrap().expect("data stream open");
            if let Ok(ServerData::Output { bytes, .. }) = ServerData::from_frame(frame) {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                assert!(seen.contains("mark42") <= (fake.content() == Some(pasted.clone())), "input ran before the push landed");
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(fake.content(), Some(pasted));
    // Our own push is not copied back.
    assert_eq!(remote_clip_write(&mut data, Duration::from_millis(1200)).await, None);

    // Local clients share the host's clipboard: their pushes are ignored,
    // and host changes go to remote streams only.
    let mut local = silo_client::DataStream::open(&control, &block, None).await.unwrap();
    local.send(&ClientData::ClipboardPush { content: Clip::Text("local".into()) });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(fake.content(), Some(Clip::Text("pasted".into())));
    fake.set_by_host("copied on host");
    assert_eq!(remote_clip_write(&mut data, T).await.as_deref(), Some("copied on host"));
    let local_write = timeout(Duration::from_millis(800), async {
        loop {
            if let ServerData::ClipboardWrite { text, .. } = local.rx.recv().await.expect("stream open") {
                return text;
            }
        }
    })
    .await;
    assert!(local_write.is_err(), "{local_write:?}");
    drop(local);
    drop(data);
    drop(ctl);
    net.close().await.unwrap();
    shutdown(control, task).await;
}
