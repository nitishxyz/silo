use std::{
    net::Ipv4Addr,
    time::{Duration, Instant},
};

use silo_latency::{Proxy, ProxyConfig, Stats};
use tokio::net::UdpSocket;

/// Raw UDP echo through the proxy: each round trip is `rtt` within ±2 ms (median).
#[tokio::test(flavor = "multi_thread")]
async fn proxy_adds_configured_rtt() {
    let echo = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let upstream = echo.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = echo.recv_from(&mut buf).await {
            let _ = echo.send_to(&buf[..n], from).await;
        }
    });
    for rtt in [0u64, 40] {
        let rtt = Duration::from_millis(rtt);
        let proxy = Proxy::spawn((Ipv4Addr::LOCALHOST, 0).into(), upstream, ProxyConfig::rtt(rtt)).await.unwrap();
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        client.connect(proxy.addr()).await.unwrap();
        let mut samples = Vec::new();
        for i in 0u32..20 {
            let sent = Instant::now();
            client.send(&i.to_be_bytes()).await.unwrap();
            let mut buf = [0u8; 4];
            let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf)).await.unwrap().unwrap();
            assert_eq!(&buf[..n], &i.to_be_bytes(), "order preserved");
            samples.push(sent.elapsed());
        }
        let stats = Stats::from_samples(&samples);
        let want = rtt.as_secs_f64() * 1e3;
        assert!(stats.p50 >= want && stats.p50 < want + 2.0, "rtt {want} ms: {stats:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_loss_drops_everything() {
    let sink = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let config = ProxyConfig { loss: 1.0, ..Default::default() };
    let proxy = Proxy::spawn((Ipv4Addr::LOCALHOST, 0).into(), sink.local_addr().unwrap(), config).await.unwrap();
    let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    client.send_to(b"x", proxy.addr()).await.unwrap();
    let mut buf = [0u8; 4];
    assert!(tokio::time::timeout(Duration::from_millis(100), sink.recv(&mut buf)).await.is_err());
}

/// Full path: in-process server, client paired over a 50 ms proxy.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "iroh QUIC NAT traversal routes around the proxy until silo-transport can pin the path"]
async fn bench_echo_at_50ms() {
    let report = silo_latency::bench(&[50], 15).await.unwrap();
    let row = report.rows.iter().find(|r| r.rtt_ms == Some(50)).unwrap();
    assert_eq!(row.echo.n, 15);
    assert!(row.echo.p50 >= 50.0 && row.echo.p50 < 50.0 + 15.0, "{}", report.table());
    let unix = &report.rows[0];
    assert!(unix.echo.p50 < 15.0, "{}", report.table());
}
