//! `silo/tunnel/1` end to end: a paired device reaches a test HTTP server
//! next to the silo server, unpaired devices and public destinations are
//! refused, and the client proxy handles CONNECT, absolute-form GET and a
//! websocket upgrade (public hosts going direct is a silo-client unit test).
//! Relays and mDNS are disabled.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use data_encoding::BASE64;
use silo_client::{
    Control,
    transport::{
        EndpointAddr, Net, PairTicket, RelayConfig,
        tunnel::{self, TunnelErr, TunnelErrCode},
    },
    tunnel::TunnelProxy,
};
use silo_proto::{
    control::{Method, PairInvite},
    paths,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};

const T: Duration = Duration::from_secs(10);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-tunnel-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> (JoinHandle<()>, Control) {
    let config = silo_server::Config { home: home.to_owned(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    for _ in 0..500 {
        if silo_client::server_running(home).await {
            let control = Control::connect(&paths::socket(home), "owner").await.unwrap().0;
            return (task, control);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn shutdown(control: Control, task: JoinHandle<()>) {
    control.call_value(Method::ServerShutdown {}).await.unwrap();
    drop(control);
    timeout(T, task).await.unwrap().unwrap();
}

/// A fresh invite, rewritten to the server's loopback address.
async fn invite(control: &Control) -> PairTicket {
    for _ in 0..200 {
        let invite: PairInvite = control.call(Method::PairCreate { ttl_secs: 60 }).await.unwrap();
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

async fn paired_device(home: &Path, control: &Control) -> (Net, EndpointAddr) {
    let ticket = invite(control).await;
    let net = device(home, "paired").await;
    let (ctl, _) = timeout(T, silo_client::pair(&net, &ticket, "laptop")).await.unwrap().unwrap();
    drop(ctl);
    (net, ticket.addr)
}

/// Answers plain requests with their request line as the body; upgrade
/// requests with 101 and then echoes bytes.
async fn http_server() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0u8; 1];
                    if tcp.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let line = head.lines().next().unwrap().to_owned();
                if head.to_ascii_lowercase().contains("upgrade: websocket") {
                    tcp.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").await.unwrap();
                    let (mut r, mut w) = tcp.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                } else {
                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{line}", line.len());
                    let _ = tcp.write_all(reply.as_bytes()).await;
                }
            });
        }
    });
    port
}

fn refusal(e: &std::io::Error) -> Option<TunnelErrCode> {
    e.get_ref().and_then(|e| e.downcast_ref::<TunnelErr>()).map(|e| e.code)
}

#[tokio::test(flavor = "multi_thread")]
async fn paired_reaches_private_and_is_refused_public() {
    let home = temp_home("paired");
    let (task, control) = start(&home).await;
    let web = http_server().await;
    let (net, addr) = paired_device(&home, &control).await;
    let rc = timeout(T, net.dial_tunnel(addr)).await.unwrap().unwrap();

    let mut s = rc.open_stream().await.unwrap();
    timeout(T, tunnel::open(&mut s, "localhost", web)).await.unwrap().unwrap();
    s.write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let mut body = String::new();
    timeout(T, s.read_to_string(&mut body)).await.unwrap().unwrap();
    assert!(body.ends_with("GET /hello HTTP/1.1"), "{body}");

    for host in ["example.com", "8.8.8.8", "[2001:db8::1]", "169.254.169.254", "fe80::1", "[::ffff:169.254.169.254]"] {
        let mut s = rc.open_stream().await.unwrap();
        let err = timeout(T, tunnel::open(&mut s, host, 80)).await.unwrap().unwrap_err();
        assert_eq!(refusal(&err), Some(TunnelErrCode::Forbidden), "{host}");
    }

    rc.close(b"done");
    net.close().await.unwrap();
    shutdown(control, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unpaired_peer_is_closed() {
    let home = temp_home("unpaired");
    let (task, control) = start(&home).await;
    let web = http_server().await;
    let addr = invite(&control).await.addr;
    let net = device(&home, "stranger").await;
    let rc = timeout(T, net.dial_tunnel(addr)).await.unwrap().unwrap();

    let mut s = rc.open_stream().await.unwrap();
    let err = timeout(T, tunnel::open(&mut s, "localhost", web)).await.unwrap().unwrap_err();
    assert_eq!(refusal(&err), None, "closed, not answered: {err}");
    let reason = timeout(T, rc.closed()).await.unwrap();
    assert!(reason.to_string().contains("unauthorized") || format!("{reason:?}").contains("unauthorized"), "{reason:?}");

    net.close().await.unwrap();
    shutdown(control, task).await;
}

async fn read_response_head(tcp: &mut TcpStream) -> String {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        assert_eq!(tcp.read(&mut byte).await.unwrap(), 1, "eof in head: {}", String::from_utf8_lossy(&head));
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_connect_get_and_websocket() {
    let home = temp_home("proxy");
    let (task, control) = start(&home).await;
    let web = http_server().await;
    let (net, addr) = paired_device(&home, &control).await;
    let proxy = TunnelProxy::start(Arc::new(net), addr).await.unwrap();
    let p = proxy.port();
    let (user, pass) = proxy.credentials();
    let userpass = format!("{user}:{pass}");
    let auth = format!("Proxy-Authorization: Basic {}\r\n", BASE64.encode(userpass.as_bytes()));

    // Absolute-form GET: rewritten to origin-form upstream.
    let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, p)).await.unwrap();
    tcp.write_all(
        format!("GET http://localhost:{web}/abs?q=1 HTTP/1.1\r\nHost: localhost:{web}\r\n{auth}Proxy-Connection: keep-alive\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    timeout(T, tcp.read_to_string(&mut out)).await.unwrap().unwrap();
    assert!(out.starts_with("HTTP/1.1 200") && out.ends_with("GET /abs?q=1 HTTP/1.1"), "{out}");

    // CONNECT, then a request over the raw tunnel.
    let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, p)).await.unwrap();
    tcp.write_all(format!("CONNECT 127.0.0.1:{web} HTTP/1.1\r\nHost: 127.0.0.1:{web}\r\n{auth}\r\n").as_bytes()).await.unwrap();
    let head = timeout(T, read_response_head(&mut tcp)).await.unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    tcp.write_all(b"GET /tunneled HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut out = String::new();
    timeout(T, tcp.read_to_string(&mut out)).await.unwrap().unwrap();
    assert!(out.ends_with("GET /tunneled HTTP/1.1"), "{out}");

    // Websocket upgrade via absolute-form, then bytes both ways.
    let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, p)).await.unwrap();
    tcp.write_all(
        format!("GET http://localhost:{web}/ws HTTP/1.1\r\nHost: localhost:{web}\r\n{auth}Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let head = timeout(T, read_response_head(&mut tcp)).await.unwrap();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    for msg in [&b"ping"[..], b"second frame"] {
        tcp.write_all(msg).await.unwrap();
        let mut back = vec![0u8; msg.len()];
        timeout(T, tcp.read_exact(&mut back)).await.unwrap().unwrap();
        assert_eq!(back, msg);
    }

    // The same through curl, when present.
    if std::process::Command::new("curl").arg("--version").output().is_ok() {
        for args in [vec![], vec!["-p"]] {
            let out = tokio::process::Command::new("curl")
                .args(["-sS", "--max-time", "10", "-x", &format!("http://127.0.0.1:{p}"), "--proxy-user", &userpass])
                .args(&args)
                .arg(format!("http://localhost:{web}/curl"))
                .env_remove("NO_PROXY")
                .env_remove("no_proxy")
                .output()
                .await
                .unwrap();
            let body = String::from_utf8_lossy(&out.stdout);
            assert_eq!(body, "GET /curl HTTP/1.1", "curl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        }
    }

    drop(proxy);
    shutdown(control, task).await;
}
