//! Whether a remote host's connection is direct or relayed, and what to do
//! about a relay. iroh starts each connection through the host's relay and
//! moves it to a direct UDP path once hole punching works. A firewall that
//! drops unsolicited UDP to the host (a cloud security group, a router)
//! keeps it on the relay, whose round trip every keystroke then pays.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use silo_proto::control::ServerInfo;
use silo_transport::{EndpointAddr, PathInfo, RemoteConn};

/// How long a fresh connection gets to leave the relay before it counts as
/// relayed: hole punching takes a few round trips, and a relay far away
/// makes each one slow.
pub const SETTLE: Duration = Duration::from_secs(10);

/// Wait up to `wait` for `conn` to go direct; its selected path then
/// (`None` while it has none).
pub async fn settle(conn: &RemoteConn, wait: Duration) -> Option<PathInfo> {
    let deadline = Instant::now() + wait;
    loop {
        let path = conn.path();
        if path.as_ref().is_some_and(|p| !p.relay) || Instant::now() >= deadline {
            return path;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The host's public addresses among those it reported (`endpoint_addr`),
/// IPv4 first: what a firewall in front of it is asked to let in.
#[must_use]
pub fn public_addrs(info: &ServerInfo) -> Vec<SocketAddr> {
    let Some(addr) = info.endpoint_addr.clone().and_then(|a| serde_json::from_value::<EndpointAddr>(a).ok()) else { return Vec::new() };
    let mut public: Vec<SocketAddr> = addr.ip_addrs().copied().filter(|a| is_public(a.ip())).collect();
    public.sort_by_key(|a| (a.is_ipv6(), *a));
    public.dedup();
    public
}

/// Routable on the internet: not loopback, private, link-local, CGNAT,
/// unique-local or unspecified.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            !(v6.is_loopback() || v6.is_unspecified() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// What to do so the host's connections go direct: which UDP port to let
/// in, or how to give it one that stays the same.
#[must_use]
pub fn relay_advice(info: &ServerInfo) -> String {
    let Some(port) = info.udp_port else {
        return "Its silo picks a new UDP port at each start: update it (it then keeps one port) or run it with \
                `silo server --port <PORT>`, then let that UDP port in through its firewall."
            .to_owned();
    };
    let at = public_addrs(info).into_iter().find(|a| a.port() == port).map(|a| format!(" ({})", a.ip())).unwrap_or_default();
    format!(
        "If it is behind a firewall (a cloud security group such as AWS's, or a router), allow inbound UDP port {port}{at} \
         from anywhere; only paired devices can connect."
    )
}

/// One line for a connection to `host` still on a relay after [`SETTLE`]:
/// the round trip, then `advice` ([`relay_advice`]).
#[must_use]
pub fn relay_hint(host: &str, path: &PathInfo, advice: &str) -> String {
    format!("{host} is connected through a relay ({} ms), not directly. {advice}", path.rtt.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(udp_port: Option<u16>, addrs: &[&str]) -> ServerInfo {
        let id = silo_transport::Identity::ephemeral().id();
        let addr = EndpointAddr::from_parts(id, addrs.iter().map(|a| silo_transport::TransportAddr::Ip(a.parse().unwrap())));
        ServerInfo {
            version: String::new(),
            pid: 1,
            server_id: String::new(),
            started_at: 0,
            endpoint_id: String::new(),
            hostname: String::new(),
            remote_error: None,
            endpoint_addr: Some(serde_json::to_value(addr).unwrap()),
            udp_port,
            idle_exit_secs: 0,
        }
    }

    #[test]
    fn public_addresses_skip_private_ones_and_come_ipv4_first() {
        let info = info(
            Some(7842),
            &["172.31.10.179:7842", "[2406:da1a::1]:7842", "13.233.176.180:7842", "100.70.0.1:7842", "[fe80::1]:7842", "127.0.0.1:7842"],
        );
        let public: Vec<String> = public_addrs(&info).iter().map(ToString::to_string).collect();
        assert_eq!(public, ["13.233.176.180:7842", "[2406:da1a::1]:7842"]);
    }

    #[test]
    fn the_advice_names_the_port_to_let_in_or_how_to_keep_one() {
        let advice = relay_advice(&info(Some(7842), &["172.31.10.179:7842", "13.233.176.180:7842"]));
        assert!(advice.contains("inbound UDP port 7842 (13.233.176.180)"), "{advice}");
        let advice = relay_advice(&info(Some(7842), &["172.31.10.179:7842"]));
        assert!(advice.contains("inbound UDP port 7842 from"), "{advice}");
        let advice = relay_advice(&info(None, &["13.233.176.180:40000"]));
        assert!(advice.contains("new UDP port at each start") && advice.contains("--port"), "{advice}");
        let path = PathInfo { relay: true, addr: "Relay(https://euc1-1.relay.n0.iroh.link./)".into(), rtt: Duration::from_millis(395) };
        let hint = relay_hint("aws", &path, &relay_advice(&info(Some(7842), &[])));
        assert!(hint.starts_with("aws is connected through a relay (395 ms), not directly. If it is behind"), "{hint}");
    }
}
