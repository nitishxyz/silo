//! Regenerate the bundled models.dev snapshot, optionally from a local feed.

#[cfg(feature = "client")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use otto_llm::catalog::{PROVIDERS, SOURCE, trim_feed};
    let args: Vec<_> = std::env::args().skip(1).collect();
    let feed = match args.as_slice() {
        [] => reqwest::Client::new().get(SOURCE).timeout(std::time::Duration::from_secs(60)).send().await?.error_for_status()?.json().await?,
        [flag, path] if flag == "--from" => serde_json::from_slice(&std::fs::read(path)?)?,
        _ => return Err("usage: update_catalog [--from /path/api.json]".into()),
    };
    let providers = trim_feed(feed)?;
    if providers.len() != PROVIDERS.len() || providers.values().any(|p| p.models.is_empty()) {
        return Err("models.dev feed is missing supported providers or chat models".into());
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("catalog/snapshot.json");
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&providers)?))?;
    println!("Wrote {} ({} models)", path.display(), providers.values().map(|p| p.models.len()).sum::<usize>());
    Ok(())
}

#[cfg(not(feature = "client"))]
fn main() {
    eprintln!("update_catalog requires the client feature");
    std::process::exit(1);
}
