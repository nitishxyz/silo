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
    control::{InvitePurpose, Method, SpawnSpec},
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
    loopback(control, InvitePurpose::Device).await.1
}

/// An invite of `purpose` (id, ticket at the server's loopback address).
async fn loopback(control: &Control, purpose: InvitePurpose) -> (String, String) {
    for _ in 0..200 {
        let invite = silo_cli::link::invite(control, Duration::from_secs(60), purpose).await.unwrap();
        let ticket = PairTicket::decode(&invite.ticket).unwrap();
        if let Some(port) = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port) {
            let addr = EndpointAddr::new(ticket.addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into());
            return (invite.invite_id, PairTicket { addr, secret: ticket.secret }.encode());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server endpoint has no direct address");
}

/// `silo join` from B with A's join ticket: A sees B's offer and adds it.
#[tokio::test(flavor = "multi_thread")]
async fn join_offers_and_adds_host() {
    let a_home = temp_home("join-a");
    let b_home = temp_home("join-b");
    let a_task = start(&a_home).await;
    let b_task = start(&b_home).await;
    let (a, mut a_events) = Control::connect(&paths::socket(&a_home), "a").await.unwrap();
    let b = Control::connect(&paths::socket(&b_home), "b").await.unwrap().0;
    let options = NetOptions { relay: RelayConfig::Disabled, mdns: false };
    a.subscribe().await.unwrap();
    let (invite_id, join) = loopback(&a, InvitePurpose::Join).await;
    let (_, offer_ticket) = loopback(&b, InvitePurpose::Device).await;

    let error = silo_cli::link::offer(&b_home, "nonsense", &offer_ticket, "box-b", &options).await.unwrap_err();
    assert!(format!("{error:#}").contains("invalid join ticket"), "{error:#}");
    timeout(T, silo_cli::link::offer(&b_home, &join, &offer_ticket, "box-b", &options)).await.unwrap().unwrap();
    let offer = silo_cli::link::wait_offer(&mut a_events, &invite_id, T).await.unwrap();
    assert_eq!((offer.name.as_str(), offer.endpoint_id.as_str()), ("box-b", b.hello().server.endpoint_id.as_str()));
    let host = timeout(T, silo_cli::add_host(&a_home, &offer.ticket, Some(offer.name), &options, "a")).await.unwrap().unwrap();
    assert_eq!(host.name, "box-b");
    assert_eq!(Hosts::load(&a_home).unwrap().hosts, [host]);
    assert_eq!(silo_cli::devices(&b).await.unwrap().len(), 1);

    let error = timeout(T, silo_cli::link::offer(&b_home, &join, &offer_ticket, "box-b", &options)).await.unwrap().unwrap_err();
    assert!(format!("{error:#}").contains("expired or already used"), "{error:#}");

    for (control, task) in [(a, a_task), (b, b_task)] {
        control.call_value(Method::ServerShutdown {}).await.unwrap();
        drop(control);
        timeout(T, task).await.unwrap().unwrap();
    }
    let _ = std::fs::remove_dir_all(a_home);
    let _ = std::fs::remove_dir_all(b_home);
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
    let invite = ticket(&owner).await;
    let invite_addr = PairTicket::decode(&invite).unwrap().addr;
    Hosts::update(&client_home, |hosts| hosts.upsert(invite_addr.clone(), Some("box".into())).map(drop)).unwrap();
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
    Hosts::update(&client_home, |stale| {
        stale.hosts[0].addr = invite_addr.clone();
        Ok(())
    })
    .unwrap();
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
