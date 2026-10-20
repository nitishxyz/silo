//! The process-wide iroh endpoint for remote hosts, bound on first use
//! with this app's device identity (`<state>/client`).

use std::path::{Path, PathBuf};

use silo_client::transport::{Net, NetOptions, RelayConfig};
use tokio::sync::OnceCell;

static NET: OnceCell<Net> = OnceCell::const_new();

/// Where this app's device key lives.
#[must_use]
pub fn identity_dir(home: &Path) -> PathBuf {
    home.join("client")
}

/// The shared endpoint, bound the first time it is needed. A failed bind
/// is retried on the next call.
///
/// # Errors
/// Unreadable identity or bind failure.
pub async fn net(home: &Path) -> anyhow::Result<&'static Net> {
    NET.get_or_try_init(|| async {
        let identity = silo_client::client_identity(&identity_dir(home))?;
        // Dev/test knob (docs/PHASE5.md §5): only the dialed path, e.g. a
        // delay proxy or loopback: kept, with no relay and no mDNS.
        let pinned = std::env::var_os("SILO_PIN_PATH").is_some_and(|v| v == "1");
        let relay = if pinned { RelayConfig::Disabled } else { RelayConfig::Default };
        Net::bind_with(&identity, NetOptions { relay, mdns: !pinned, pin_initial_path: pinned, port: None }).await
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_lives_in_state_dir() {
        assert_eq!(identity_dir(Path::new("/tmp/silo")), PathBuf::from("/tmp/silo/client"));
    }
}
