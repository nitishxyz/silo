//! `bench [--rtt 0,50,150] [--keys 50] [--json <file>]`

use anyhow::{Context, bail};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut rtts, mut keys, mut json) = (vec![0, 50, 150], 50usize, None);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--rtt" => rtts = value()?.split(',').map(str::parse).collect::<Result<_, _>>().context("--rtt")?,
            "--keys" => keys = value()?.parse().context("--keys")?,
            "--json" => json = Some(value()?),
            other => bail!("unknown argument {other}"),
        }
    }
    let report = silo_latency::bench(&rtts, keys).await?;
    print!("{}", report.table());
    let encoded = serde_json::to_string_pretty(&report)?;
    match json {
        Some(path) => std::fs::write(path, encoded)?,
        None => println!("{encoded}"),
    }
    let bypassed = report.bypassed();
    if !bypassed.is_empty() {
        bail!("iroh routed around the proxy for {}", bypassed.join(", "));
    }
    Ok(())
}
