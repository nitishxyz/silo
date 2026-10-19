mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use common::TempDir;
use silo_transport::{Conn, EndpointAddr, Identity, Net, NetOptions, RelayConfig, RemoteConn, TransportAddr};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

const T: Duration = Duration::from_secs(10);

async fn write_frame(w: &mut (impl AsyncWrite + Unpin), kind: u8, payload: &[u8]) {
    w.write_u32(payload.len() as u32 + 1).await.unwrap();
    w.write_u8(kind).await.unwrap();
    w.write_all(payload).await.unwrap();
    w.flush().await.unwrap();
}

async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> (u8, Vec<u8>) {
    let len = r.read_u32().await.unwrap() as usize;
    let kind = r.read_u8().await.unwrap();
    let mut buf = vec![0; len - 1];
    r.read_exact(&mut buf).await.unwrap();
    (kind, buf)
}

fn loopback_addr(net: &Net) -> EndpointAddr {
    let port = net.bound_sockets().into_iter().find(SocketAddr::is_ipv4).expect("ipv4 socket").port();
    EndpointAddr::new(net.id()).with_ip_addr((Ipv4Addr::LOCALHOST, port).into())
}

#[tokio::test(flavor = "multi_thread")]
async fn ctl_connect_streams_and_close() {
    let tmp = TempDir::new();
    let sid = Identity::load_or_create(tmp.path().join("s")).unwrap();
    let cid = Identity::load_or_create(tmp.path().join("c")).unwrap();
    let mut server = Net::bind(&sid, RelayConfig::Disabled, false).await.unwrap();
    let client = Net::bind(&cid, RelayConfig::Disabled, false).await.unwrap();
    assert_eq!(server.id(), sid.id());
    let (mut incoming, _tunnels) = server.serve(4).unwrap();

    let rc = timeout(T, client.dial(loopback_addr(&server))).await.unwrap().unwrap();
    assert_eq!(rc.remote_id(), sid.id());
    let conn = Conn::Iroh(rc.clone());

    let mut s1 = conn.open_stream().await.unwrap();
    write_frame(&mut s1, 1, b"hello").await;

    let (src, mut first) = timeout(T, incoming.next()).await.unwrap().unwrap();
    assert_eq!(src.remote_id(), cid.id());
    assert_eq!(read_frame(&mut first).await, (1, b"hello".to_vec()));
    write_frame(&mut first, 2, b"welcome").await;
    assert_eq!(timeout(T, read_frame(&mut s1)).await.unwrap(), (2, b"welcome".to_vec()));

    let mut s2 = conn.open_stream().await.unwrap();
    write_frame(&mut s2, 3, b"data").await;
    let mut second = timeout(T, src.accept_stream()).await.unwrap().unwrap();
    assert_eq!(read_frame(&mut second).await, (3, b"data".to_vec()));
    write_frame(&mut second, 4, b"ack").await;
    assert_eq!(timeout(T, read_frame(&mut s2)).await.unwrap(), (4, b"ack".to_vec()));

    // Stream shutdown is seen as EOF by the peer.
    s2.shutdown().await.unwrap();
    let mut rest = Vec::new();
    timeout(T, second.read_to_end(&mut rest)).await.unwrap().unwrap();
    assert!(rest.is_empty());

    timeout(T, client.close()).await.unwrap().unwrap();
    timeout(T, src.closed()).await.unwrap();
    timeout(T, rc.closed()).await.unwrap();
    timeout(T, server.close()).await.unwrap().unwrap();
    assert!(timeout(T, incoming.next()).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_twice_fails() {
    let tmp = TempDir::new();
    let id = Identity::load_or_create(tmp.path()).unwrap();
    let mut net = Net::bind(&id, RelayConfig::Disabled, false).await.unwrap();
    let _incoming = net.serve(1).unwrap();
    assert!(net.serve(1).is_err());
    timeout(T, net.close()).await.unwrap().unwrap();
}

/// UDP proxy on 127.0.0.1 adding `delay` each way; returns its address.
async fn delay_proxy(target: SocketAddr, delay: Duration) -> SocketAddr {
    use std::sync::{Arc, Mutex};
    use tokio::net::UdpSocket;
    let front = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
    let back = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
    let addr = front.local_addr().unwrap();
    let peer: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    let (f, b, p) = (front.clone(), back.clone(), peer.clone());
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((n, from)) = f.recv_from(&mut buf).await {
            *p.lock().unwrap() = Some(from);
            let (b, pkt) = (b.clone(), buf[..n].to_vec());
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = b.send_to(&pkt, target).await;
            });
        }
    });
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((n, _)) = back.recv_from(&mut buf).await {
            let Some(to) = *peer.lock().unwrap() else {
                continue;
            };
            let (f, pkt) = (front.clone(), buf[..n].to_vec());
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = f.send_to(&pkt, to).await;
            });
        }
    });
    addr
}

async fn bind_pinned(id: &Identity, pin: bool) -> Net {
    let opts = NetOptions { relay: RelayConfig::Disabled, mdns: false, pin_initial_path: pin, port: None };
    Net::bind_with(id, opts).await.unwrap()
}

type Paths = Vec<(TransportAddr, bool, Duration)>;

fn ip_paths(rc: &RemoteConn) -> Paths {
    rc.paths().iter().filter(|p| p.is_ip()).map(|p| (p.remote_addr().clone(), p.is_selected(), p.rtt())).collect()
}

/// Connects through a 50 ms RTT proxy, pings ~3 s, returns (dialed, client, server) paths.
async fn settle(pin: bool) -> (SocketAddr, Paths, Paths) {
    let tmp = TempDir::new();
    let sid = Identity::load_or_create(tmp.path().join("s")).unwrap();
    let cid = Identity::load_or_create(tmp.path().join("c")).unwrap();
    let mut server = bind_pinned(&sid, pin).await;
    let client = bind_pinned(&cid, pin).await;
    let (mut incoming, _tunnels) = server.serve(1).unwrap();
    let direct = *loopback_addr(&server).ip_addrs().next().unwrap();
    let dialed_ip = delay_proxy(direct, Duration::from_millis(25)).await;
    let dialed = EndpointAddr::new(server.id()).with_ip_addr(dialed_ip);
    let rc = timeout(T, client.dial(dialed)).await.unwrap().unwrap();
    let mut s = rc.open_stream().await.unwrap();
    write_frame(&mut s, 1, b"x").await;
    let (src, mut first) = timeout(T, incoming.next()).await.unwrap().unwrap();
    for _ in 0..30 {
        assert_eq!(read_frame(&mut first).await.0, 1);
        write_frame(&mut first, 2, b"y").await;
        assert_eq!(timeout(T, read_frame(&mut s)).await.unwrap().0, 2);
        tokio::time::sleep(Duration::from_millis(100)).await;
        write_frame(&mut s, 1, b"x").await;
    }
    let out = (dialed_ip, ip_paths(&rc), ip_paths(&src));
    client.close().await.unwrap();
    server.close().await.unwrap();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_connection_stays_on_dialed_path() {
    // Without pinning iroh may holepunch around the proxy (machine-dependent).
    let (_, unpinned, _) = settle(false).await;
    eprintln!("unpinned client paths: {unpinned:?}");

    let (dialed, client, server) = settle(true).await;
    let [(addr, selected, rtt)] = client.as_slice() else {
        panic!("client paths {client:?}");
    };
    assert_eq!(*addr, TransportAddr::Ip(dialed));
    assert!(*selected);
    assert!(*rtt >= Duration::from_millis(45), "rtt {rtt:?}");
    let [(_, selected, rtt)] = server.as_slice() else {
        panic!("server paths {server:?}");
    };
    assert!(*selected);
    assert!(*rtt >= Duration::from_millis(45), "rtt {rtt:?}");
}
