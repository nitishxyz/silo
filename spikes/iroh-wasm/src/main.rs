//! Native run of the same client, for comparison:
//! `cargo run --release -- <silo1…ticket> <vt>`.

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let ticket = args.next().expect("usage: spike <ticket> <vt>");
    let vt = args.next().expect("usage: spike <ticket> <vt>");
    let wait_online = std::env::var_os("SPIKE_NO_ONLINE").is_none();
    let relay_only = std::env::var_os("SPIKE_ALL_ADDRS").is_none();
    let report = silo_iroh_wasm_spike::run(&ticket, &vt, wait_online, relay_only, &|s| eprintln!("{s}")).await?;
    if let Some(path) = std::env::var_os("SPIKE_SNAPSHOT_OUT") {
        std::fs::write(path, &report.snapshot)?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn main() {}
