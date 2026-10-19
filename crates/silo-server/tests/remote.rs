//! Remote access over iroh: pairing, authorization, permissions and data
//! ticket binding. Relays and mDNS are disabled; peers dial loopback.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;
use silo_client::Control;
use silo_proto::{
    BUILD, Frame,
    control::{ClientHello, Created, DataTicket, Method, PairInvite, PairedDevices, Request, RpcError, ServerHello, ServerMessage, SpawnSpec},
    data::DataHello,
    kind,
    model::TermSize,
    paths, read_frame, write_frame,
};
use silo_transport::{BiStream, EndpointAddr, Identity, Net, PairTicket, RelayConfig, RemoteConn};
use tokio::{io::AsyncWriteExt, task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(10);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-remote-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> JoinHandle<()> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: RelayConfig::Disabled,
        mdns: false,
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
    for _ in 0..200 {
        let invite: PairInvite = control.call(Method::PairCreate { ttl_secs }).await.unwrap();
        let ticket = PairTicket::decode(&invite.ticket).unwrap();
        if let Some(port) = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port) {
            let addr = EndpointAddr::new(ticket.addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into());
            return (invite.ticket, addr);
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
    let hello = DataHello { build: BUILD.into(), ticket: ticket.into(), resume: None };
    send(&mut stream, &Frame::json(kind::DATA_HELLO, &hello).unwrap()).await;
    timeout(T, read_frame(&mut stream)).await.unwrap().ok().flatten().map(|f| f.kind)
}

async fn hello_on(rc: &RemoteConn, build: &str, invite: Option<&str>) -> (BiStream, Result<ServerHello, RpcError>) {
    let mut stream = rc.open_stream().await.unwrap();
    let hello = ClientHello { build: build.into(), client_name: "remote-test".into(), token: None, passive: false, invite: invite.map(Into::into) };
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

async fn connect(net: &Net, addr: &EndpointAddr, build: &str, invite: Option<&str>) -> Result<Ctl, RpcError> {
    let rc = timeout(T, net.dial(addr.clone())).await.unwrap().unwrap();
    let (stream, reply) = hello_on(&rc, build, invite).await;
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
    let mut ctl = connect(&net, &addr, BUILD, Some(&invite)).await.unwrap();

    let info: silo_proto::control::ServerInfo = serde_json::from_value(ctl.call(Method::ServerInfo {}).await.unwrap()).unwrap();
    assert_eq!(info.endpoint_id, addr.id.to_string());
    let created = create(&mut ctl).await;
    let t = ticket(&mut ctl, &created.block_id).await;
    assert_eq!(ctl.data(&t).await, Some(kind::WELCOME));
    // Single use.
    assert_eq!(ctl.data(&t).await, Some(kind::REJECT));

    // A second control stream on the same connection gets no reply.
    let (_, second) = hello_on(&ctl.rc, BUILD, None).await;
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
    assert_eq!(code(connect(&net, &addr, BUILD, None).await.map(|_| ())), RpcError::UNAUTHORIZED);
    assert_eq!(code(connect(&net, &addr, BUILD, Some("silo1garbage")).await.map(|_| ())), RpcError::UNAUTHORIZED);
    let forged = PairTicket { addr: addr.clone(), secret: [7; 32] }.encode();
    assert_eq!(code(connect(&net, &addr, BUILD, Some(&forged)).await.map(|_| ())), RpcError::UNAUTHORIZED);
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
    assert_eq!(code(connect(&a, &addr, BUILD, Some(&short)).await.map(|_| ())), RpcError::UNAUTHORIZED);
    connect(&a, &addr, BUILD, Some(&once)).await.unwrap();
    let b = device(&home, "b").await;
    assert_eq!(code(connect(&b, &addr, BUILD, Some(&once)).await.map(|_| ())), RpcError::UNAUTHORIZED);
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
    let mut ctl = connect(&net, &addr, BUILD, Some(&invite)).await.unwrap();
    assert_eq!(code(ctl.call(Method::PairCreate { ttl_secs: 60 }).await), RpcError::FORBIDDEN);
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
    let mut ca = connect(&a, &addr, BUILD, Some(&ia)).await.unwrap();
    let cb = connect(&b, &addr, BUILD, Some(&ib)).await.unwrap();
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
    let mut ctl = connect(&net, &addr, BUILD, Some(&invite)).await.unwrap();
    let created = create(&mut ctl).await;
    let t = ticket(&mut ctl, &created.block_id).await;
    let mut data = ctl.rc.open_stream().await.unwrap();
    send(&mut data, &Frame::json(kind::DATA_HELLO, &DataHello { build: BUILD.into(), ticket: t, resume: None }).unwrap()).await;
    assert_eq!(timeout(T, read_frame(&mut data)).await.unwrap().unwrap().unwrap().kind, kind::WELCOME);

    control.call_value(Method::PairRevoke { id: net.id().to_string() }).await.unwrap();
    timeout(T, ctl.rc.closed()).await.unwrap();
    // Drain whatever was in flight; the stream must end.
    timeout(T, async { while let Ok(Some(_)) = read_frame(&mut data).await {} }).await.unwrap();
    assert_eq!(code(connect(&net, &addr, BUILD, None).await.map(|_| ())), RpcError::UNAUTHORIZED);
    let devices: PairedDevices = control.call(Method::PairList {}).await.unwrap();
    assert!(devices.devices.is_empty());
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn build_mismatch_does_not_enroll() {
    let home = temp_home("build");
    let task = start(&home).await;
    let control = owner(&home).await;
    let (invite, addr) = invite(&control, 60).await;
    let net = device(&home, "a").await;
    assert_eq!(code(connect(&net, &addr, "other-build", Some(&invite)).await.map(|_| ())), RpcError::DIFFERENT_BUILD);
    let devices: PairedDevices = control.call(Method::PairList {}).await.unwrap();
    assert!(devices.devices.is_empty());
    // The invite was not consumed.
    connect(&net, &addr, BUILD, Some(&invite)).await.unwrap();
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
    drop(connect(&net, &first_addr, BUILD, Some(&invite)).await.unwrap());
    shutdown(control, task).await;

    let task = start(&home).await;
    let control = owner(&home).await;
    let (_, addr) = self::invite(&control, 60).await;
    assert_eq!(addr.id, first_addr.id, "identity persists");
    let mut ctl = connect(&net, &addr, BUILD, None).await.unwrap();
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
    let ctl = connect(&net, &addr, BUILD, Some(&invite)).await.unwrap();

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
    let mut ctl = connect(&net, &new_addr, BUILD, None).await.unwrap();
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
        let error = control.call_value(Method::PairCreate { ttl_secs }).await.unwrap_err();
        assert!(error.to_string().contains("bad_request"), "{ttl_secs}: {error}");
    }
    // Remote access still works afterwards.
    let (invite, addr) = invite(&control, 7 * 24 * 3600).await;
    let net = device(&home, "a").await;
    let mut ctl = connect(&net, &addr, BUILD, Some(&invite)).await.unwrap();
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
    send(&mut data, &Frame::json(kind::DATA_HELLO, &DataHello { build: BUILD.into(), ticket: "00".into(), resume: None }).unwrap()).await;
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
    assert_eq!(code(connect(&net, &addr, BUILD, Some(&invite)).await.map(|_| ())), RpcError::UNAUTHORIZED);
    drop(connect(&net, &addr, BUILD, Some(&fresh)).await.unwrap());
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
    let error = control.call_value(Method::PairCreate { ttl_secs: 60 }).await.unwrap_err();
    assert!(error.to_string().contains("remote access is not running"), "{error}");
    net.close().await.unwrap();
    control.call_value(Method::ServerShutdown {}).await.unwrap();
    timeout(T, third).await.unwrap().unwrap().unwrap();
}
