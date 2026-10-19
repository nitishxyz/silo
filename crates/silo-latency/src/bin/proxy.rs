//! `proxy --upstream <addr> [--listen 127.0.0.1:0] [--rtt-ms N] [--jitter-ms N] [--loss P]`

use std::time::Duration;

use anyhow::{Context, bail};
use silo_latency::{Proxy, ProxyConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut listen, mut upstream) = ("127.0.0.1:0".to_owned(), None);
    let mut config = ProxyConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().with_context(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--listen" => listen = value,
            "--upstream" => upstream = Some(value.parse().context("--upstream")?),
            "--rtt-ms" => config.delay = Duration::from_millis(value.parse::<u64>().context("--rtt-ms")?) / 2,
            "--jitter-ms" => config.jitter = Duration::from_millis(value.parse().context("--jitter-ms")?),
            "--loss" => config.loss = value.parse().context("--loss")?,
            other => bail!("unknown argument {other}"),
        }
    }
    let upstream = upstream.context("--upstream is required")?;
    let proxy = Proxy::spawn(listen.parse().context("--listen")?, upstream, config).await?;
    println!("{}", proxy.addr());
    tokio::signal::ctrl_c().await?;
    Ok(())
}
