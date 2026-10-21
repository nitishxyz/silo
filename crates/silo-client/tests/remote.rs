//! Remote control and data over iroh: pairing, reconnect, data streams,
//! build mismatch, unauthorized devices and disconnect. Relays and mDNS
//! are disabled; the client dials the server's loopback address.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{
    Control, DataStream, Mirror, Unauthorized,
    transport::{Conn, EndpointAddr, Net, PairTicket, RelayConfig},
};
use silo_proto::{
    control::{Created, Method, PairInvite, PairedDevices, SpawnSpec},
    data::ClientData,
    model::TermSize,
    paths,
};
use tokio::{task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(10);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-client-remote-{name}-{}", std::process::id()));
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
    Control::connect(&paths::socket(home), "owner").await.unwrap().0
}

async fn shutdown(control: Control, task: JoinHandle<()>) {
    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    timeout(T, task).await.unwrap().unwrap();
}

/// A fresh invite, rewritten to the server's loopback address.
async fn invite(control: &Control) -> PairTicket {
    for _ in 0..200 {
        let invite: PairInvite =
            control.call(Method::PairCreate { ttl_secs: 60, purpose: silo_proto::control::InvitePurpose::Device }).await.unwrap();
        let ticket = PairTicket::decode(&invite.ticket).unwrap();
        if let Some(port) = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port) {
            let addr = EndpointAddr::new(ticket.addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into());
            return PairTicket { addr, secret: ticket.secret };
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server endpoint has no direct address");
}

async fn device(home: &Path, name: &str) -> Net {
    let id = silo_client::client_identity(&home.join(format!("client-{name}"))).unwrap();
    Net::bind(&id, RelayConfig::Disabled, false).await.unwrap()
}

fn shell() -> SpawnSpec {
    SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) }
}

async fn attach(control: &Control, block: &str, mirror: &mut Mirror) -> DataStream {
    let stream = DataStream::open(control, block, mirror.resume()).await.unwrap();
    mirror.begin(&stream.welcome).unwrap();
    stream
}

async fn pump_until(stream: &mut DataStream, mirror: &mut Mirror, text: &str) {
    timeout(T, async {
        while !mirror.screen_text().contains(text) {
            let data = stream.rx.recv().await.expect("stream closed");
            mirror.apply(data).unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; screen:\n{}", mirror.screen_text()));
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_reconnect_and_attach() {
    let home = temp_home("pair");
    let task = start(&home).await;
    let control = owner(&home).await;
    let ticket = invite(&control).await;
    let net = device(&home, "a").await;

    let (first, _) = timeout(T, silo_client::pair(&net, &ticket, "laptop")).await.unwrap().unwrap();
    let devices: PairedDevices = control.call(Method::PairList {}).await.unwrap();
    assert_eq!(devices.devices.len(), 1);
    assert_eq!(devices.devices[0].name, "laptop");
    drop(first);

    let (remote, _) = Control::connect_remote(&net, ticket.addr.clone(), "laptop", false, None).await.unwrap();
    assert!(matches!(remote.conn(), Conn::Iroh(_)));
    let created: Created =
        remote.call(Method::SessionCreate { name: None, spawn: shell(), kind: silo_proto::model::BlockKind::Terminal, url: None }).await.unwrap();

    // Two data streams on the one connection.
    let (mut a, mut b) = (Mirror::new(), Mirror::new());
    let mut stream_a = attach(&remote, &created.block_id, &mut a).await;
    let mut stream_b = attach(&remote, &created.block_id, &mut b).await;
    stream_a.send(&ClientData::Input(b"echo remote-$((40+2))\n".to_vec()));
    pump_until(&mut stream_a, &mut a, "remote-42").await;
    pump_until(&mut stream_b, &mut b, "remote-42").await;

    drop((stream_a, stream_b, remote));
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unpaired_device_is_unauthorized() {
    let home = temp_home("unauth");
    let task = start(&home).await;
    let control = owner(&home).await;
    let ticket = invite(&control).await;
    let net = device(&home, "x").await;
    let error = Control::connect_remote(&net, ticket.addr.clone(), "x", false, None).await.err().unwrap();
    assert!(error.downcast_ref::<Unauthorized>().is_some(), "{error:#}");
    let forged = PairTicket { addr: ticket.addr.clone(), secret: [7; 32] };
    let error = silo_client::pair(&net, &forged, "x").await.err().unwrap();
    assert!(error.downcast_ref::<Unauthorized>().is_some(), "{error:#}");
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_exit_resolves_closed() {
    let home = temp_home("closed");
    let task = start(&home).await;
    let control = owner(&home).await;
    let ticket = invite(&control).await;
    let net = device(&home, "a").await;
    let (remote, _) = silo_client::pair(&net, &ticket, "laptop").await.unwrap();
    assert!(!remote.is_closed());
    shutdown(control, task).await;
    timeout(T, remote.closed()).await.unwrap();
    assert!(remote.is_closed());
    assert!(remote.call_value(Method::SessionList {}).await.is_err());
    drop(remote);
    net.close().await.unwrap();
}

/// The upgrade stream works across builds; only refusals are exercised so
/// the test binary (the in-process server's executable) is never replaced.
#[tokio::test(flavor = "multi_thread")]
async fn upgrade_stream_checks_and_refuses() {
    use silo_client::upgrade::{self, Refused};
    let home = temp_home("upgrade");
    let task = start(&home).await;
    let control = owner(&home).await;
    let ticket = invite(&control).await;

    let stranger = device(&home, "x").await;
    let rc = stranger.dial(ticket.addr.clone()).await.unwrap();
    let error = upgrade::open(&Conn::Iroh(rc), None).await.err().unwrap();
    assert_eq!(error.downcast_ref::<Refused>().unwrap().code, "unauthorized", "{error:#}");
    stranger.close().await.unwrap();

    let net = device(&home, "a").await;
    let (paired, _) = silo_client::pair(&net, &ticket, "laptop").await.unwrap();
    paired.call_value(Method::SessionCreate { name: None, spawn: shell(), kind: silo_proto::model::BlockKind::Terminal, url: None }).await.unwrap();
    drop(paired);
    let script = home.join("fake-silo");
    let cases = [
        (format!("silo {} ({})", env!("CARGO_PKG_VERSION"), silo_proto::BUILD), silo_proto::upgrade::UP_TO_DATE),
        ("silo 0.0.1 (other)".to_owned(), silo_proto::upgrade::NEWER),
        ("not silo".to_owned(), "invalid"),
    ];
    for (version, code) in cases {
        std::fs::write(&script, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
        // Held: the connection must outlive the stream.
        let conn = Conn::Iroh(net.dial(ticket.addr.clone()).await.unwrap());
        let opened = timeout(T, upgrade::open(&conn, None)).await.unwrap().unwrap();
        assert_eq!(opened.host.build, silo_proto::BUILD);
        assert_eq!(opened.host.platform, silo_proto::upgrade::platform());
        assert_eq!(opened.host.sessions.len(), 1);
        let error = timeout(T, opened.send(&script, false)).await.unwrap().err().unwrap();
        assert_eq!(error.downcast_ref::<Refused>().unwrap().code, code, "{error:#}");
    }
    let exe_dir = std::env::current_exe().unwrap().canonicalize().unwrap().parent().unwrap().to_owned();
    let leftovers =
        std::fs::read_dir(exe_dir).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().starts_with(".silo.upgrade."));
    assert_eq!(leftovers.count(), 0);
    net.close().await.unwrap();
    shutdown(control, task).await;
}
