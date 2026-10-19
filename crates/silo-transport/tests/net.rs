mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use common::TempDir;
use silo_transport::{Conn, EndpointAddr, Identity, Net, RelayConfig};
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
    let port = net
        .bound_sockets()
        .into_iter()
        .find(SocketAddr::is_ipv4)
        .expect("ipv4 socket")
        .port();
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
    let mut incoming = server.serve_ctl(4).unwrap();

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
async fn serve_ctl_twice_fails() {
    let tmp = TempDir::new();
    let id = Identity::load_or_create(tmp.path()).unwrap();
    let mut net = Net::bind(&id, RelayConfig::Disabled, false).await.unwrap();
    let _incoming = net.serve_ctl(1).unwrap();
    assert!(net.serve_ctl(1).is_err());
    timeout(T, net.close()).await.unwrap().unwrap();
}
