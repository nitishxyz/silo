//! CLI-level remote use: a server on a temp home (no relays, no mDNS),
//! `pair.create` over the local socket, then `hosts add` and `ls` from a
//! separate client home through the library functions.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use silo_cli::{NetOptions, hosts::Hosts};
use silo_client::{
    Control,
    transport::{EndpointAddr, PairTicket, RelayConfig},
};
use silo_proto::{
    control::{Method, SpawnSpec},
    model::TermSize,
    paths,
};
use tokio::time::timeout;

const T: Duration = Duration::from_secs(15);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-cli-remote-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> tokio::task::JoinHandle<()> {
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

/// `silo pair`'s ticket, rewritten to the server's loopback address.
async fn ticket(control: &Control) -> String {
    for _ in 0..200 {
        let invite = silo_cli::create_invite(control, Duration::from_secs(60)).await.unwrap();
        let ticket = PairTicket::decode(&invite.ticket).unwrap();
        if let Some(port) = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port) {
            let addr = EndpointAddr::new(ticket.addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into());
            return PairTicket { addr, secret: ticket.secret }.encode();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server endpoint has no direct address");
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_then_remote_ls() {
    let server_home = temp_home("server");
    let client_home = temp_home("client");
    let task = start(&server_home).await;
    let owner = Control::connect(&paths::socket(&server_home), "owner").await.unwrap().0;
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) };
    owner
        .call_value(Method::SessionCreate { name: Some("remote-one".into()), spawn, kind: silo_proto::model::BlockKind::Terminal, url: None })
        .await
        .unwrap();
    let options = NetOptions { relay: RelayConfig::Disabled, mdns: false };

    // Unpaired: a clear refusal, nothing stored.
    let server_id = owner.hello().server.endpoint_id.clone();
    let mut hosts = Hosts::default();
    let invite = ticket(&owner).await;
    let invite_addr = PairTicket::decode(&invite).unwrap().addr;
    hosts.upsert(invite_addr.clone(), Some("box".into())).unwrap();
    hosts.save(&client_home).unwrap();
    let error = silo_cli::connect_remote(&client_home, "box", &options, "laptop").await.err().unwrap();
    assert!(format!("{error:#}").contains("refused this device"), "{error:#}");

    let host = timeout(T, silo_cli::add_host(&client_home, &invite, None, &options, "laptop")).await.unwrap().unwrap();
    assert_eq!(host.id, server_id);
    assert_eq!(host.name, "box", "re-adding keeps the stored name");
    assert_eq!(Hosts::load(&client_home).unwrap().hosts.len(), 1);
    let devices = silo_cli::devices(&owner).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "laptop");

    let remote = timeout(T, silo_cli::connect_remote(&client_home, &server_id[..8], &options, "laptop")).await.unwrap().unwrap();
    let sessions = silo_cli::sessions(&remote.control).await.unwrap();
    assert!(silo_cli::format_sessions(&sessions).contains("remote-one:"));
    // A stale stored address is refreshed from `server.info` on connect.
    let current = silo_cli::current_addr(&remote.control).await.unwrap().unwrap();
    let mut stale = Hosts::load(&client_home).unwrap();
    stale.hosts[0].addr = invite_addr.clone();
    stale.save(&client_home).unwrap();
    silo_cli::refresh_host(&client_home, &remote.control).await.unwrap();
    assert_eq!(Hosts::load(&client_home).unwrap().hosts[0].addr, current);
    // Remote never spawns a server for the client.
    assert!(!paths::socket(&client_home).exists());

    let revoked = silo_cli::revoke(&owner, &devices[0].id[..8]).await.unwrap();
    assert_eq!(revoked.id, devices[0].id);
    drop(remote.control);
    remote.net.close().await.unwrap();

    owner.call_value(Method::ServerShutdown {}).await.unwrap();
    drop(owner);
    timeout(T, task).await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(server_home);
    let _ = std::fs::remove_dir_all(client_home);
}
