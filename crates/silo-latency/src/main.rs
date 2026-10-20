//! `silo-latency proxy <ticket> <rtt-ms>`: put the delay proxy in front of a
//! pairing ticket's server (its IPv4 address) and print the ticket to pair
//! through it; runs until killed. Pair the app with `SILO_PIN_PATH=1` so it
//! stays on the proxied path (docs/PHASE5.md §5).

use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, bail};
use silo_client::transport::{EndpointAddr, PairTicket};
use silo_latency::{Proxy, ProxyConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [command, ticket, rtt] = args.as_slice() else { bail!("usage: silo-latency proxy <ticket> <rtt-ms>") };
    if command != "proxy" {
        bail!("usage: silo-latency proxy <ticket> <rtt-ms>");
    }
    let ticket = PairTicket::decode(ticket)?;
    let rtt = Duration::from_millis(rtt.parse().context("rtt in milliseconds")?);
    let port = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port).context("ticket has no IPv4 address")?;
    let proxy = Proxy::spawn((Ipv4Addr::LOCALHOST, 0).into(), (Ipv4Addr::LOCALHOST, port).into(), ProxyConfig::rtt(rtt)).await?;
    let proxied = PairTicket { addr: EndpointAddr::new(ticket.addr.id).with_ip_addr(proxy.addr()), secret: ticket.secret };
    println!("{}", proxied.encode());
    tokio::signal::ctrl_c().await?;
    Ok(())
}
