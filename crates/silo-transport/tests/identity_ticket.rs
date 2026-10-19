mod common;

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;

use common::TempDir;
use silo_transport::{EndpointAddr, Identity, PairTicket, TICKET_PREFIX};

#[test]
fn identity_persists_and_is_private() {
    let tmp = TempDir::new();
    let dir = tmp.path().join("state/iroh");
    let a = Identity::load_or_create(&dir).unwrap();
    let b = Identity::load_or_create(&dir).unwrap();
    assert_eq!(a.node_id(), b.node_id());
    assert_eq!(a.id(), b.id());
    let mode = fs::metadata(dir.join("key")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let other = Identity::load_or_create(tmp.path().join("other")).unwrap();
    assert_ne!(a.node_id(), other.node_id());
}

#[test]
fn identity_rejects_symlinked_key() {
    let tmp = TempDir::new();
    let real = tmp.path().join("real");
    Identity::load_or_create(&real).unwrap();
    let dir = tmp.path().join("linked");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(real.join("key"), dir.join("key")).unwrap();
    let err = Identity::load_or_create(&dir).unwrap_err();
    assert!(format!("{err:#}").contains("symlink"), "{err:#}");
}

fn sample_ticket() -> PairTicket {
    let tmp = TempDir::new();
    let id = Identity::load_or_create(tmp.path()).unwrap();
    let addr = EndpointAddr::new(id.id())
        .with_relay_url("https://relay.example.com./".parse().unwrap())
        .with_ip_addr("192.168.1.7:4433".parse::<SocketAddr>().unwrap())
        .with_ip_addr("[fe80::1]:4433".parse::<SocketAddr>().unwrap());
    PairTicket { addr, secret: [7u8; 32] }
}

#[test]
fn ticket_round_trip() {
    let t = sample_ticket();
    let s = t.encode();
    assert!(s.starts_with(TICKET_PREFIX), "{s}");
    assert_eq!(s, s.to_ascii_lowercase());
    assert_eq!(PairTicket::decode(&s).unwrap(), t);
    assert_eq!(PairTicket::decode(&s.to_ascii_uppercase()).unwrap(), t);
    assert_eq!(format!(" {s}\n").parse::<PairTicket>().unwrap(), t);
    assert!(!format!("{t:?}").contains("7, 7"));
}

#[test]
fn ticket_rejects_malformed() {
    let s = sample_ticket().encode();
    let body = &s[TICKET_PREFIX.len()..];
    for bad in [
        String::new(),
        "silo".to_string(),
        format!("silo2{body}"),
        format!("{TICKET_PREFIX}!!!{body}"),
        format!("{TICKET_PREFIX}{}", &body[..body.len() / 2]),
        format!("{TICKET_PREFIX}{body}aaaa"),
        TICKET_PREFIX.to_string(),
    ] {
        assert!(PairTicket::decode(&bad).is_err(), "accepted {bad:?}");
    }
    let err = PairTicket::decode("nope").unwrap_err();
    assert!(err.to_string().contains("invalid pairing ticket"), "{err}");
}
