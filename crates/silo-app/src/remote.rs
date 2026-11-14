//! The process-wide iroh endpoint for remote hosts, bound on first use
//! with this app's device identity (`<state>/client`) and the relay set in
//! Settings › Hosts (`relay` in `app.json`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use silo_client::transport::{Net, NetOptions, RelayConfig, RelayUrl};
use tokio::sync::OnceCell;

static NET: OnceCell<Net> = OnceCell::const_new();
/// The relay setting the endpoint was bound with.
static BOUND: OnceLock<Relay> = OnceLock::new();

/// Where this app's device key lives.
#[must_use]
pub fn identity_dir(home: &Path) -> PathBuf {
    home.join("client")
}

/// How this app's endpoint reaches hosts it can't reach directly: in
/// `app.json` as `"default"`, `"disabled"` or a relay URL (`silo --relay`'s
/// forms).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Relay {
    /// n0's public relays.
    #[default]
    Default,
    /// No relay: direct addresses and the local network only.
    Off,
    /// One self-hosted relay.
    Custom(String),
}

impl Relay {
    /// Typed text as a setting: `default`, `disabled` (or `off`), or an
    /// http(s) relay URL.
    ///
    /// # Errors
    /// Not one of those, said for the user.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        match text.to_ascii_lowercase().as_str() {
            "" | "default" => return Ok(Self::Default),
            "disabled" | "off" | "none" => return Ok(Self::Off),
            _ => {}
        }
        let web =
            text.split_once("://").is_some_and(|(scheme, rest)| matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") && !rest.is_empty());
        match text.parse::<RelayUrl>() {
            Ok(url) if web => Ok(Self::Custom(url.to_string())),
            _ => Err("A relay is an address like https://relay.example.com.".into()),
        }
    }

    /// The endpoint's relay configuration.
    #[must_use]
    pub fn config(&self) -> RelayConfig {
        match self {
            Self::Default => RelayConfig::Default,
            Self::Off => RelayConfig::Disabled,
            Self::Custom(url) => url.parse().map_or(RelayConfig::Default, RelayConfig::Custom),
        }
    }

    /// As shown in Settings.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Default => "Default",
            Self::Off => "Off",
            Self::Custom(url) => url,
        }
    }
}

/// Lenient: anything unreadable is the default, so a bad value never
/// discards the rest of `app.json`.
impl From<String> for Relay {
    fn from(text: String) -> Self {
        Self::parse(&text).unwrap_or_default()
    }
}

impl From<Relay> for String {
    fn from(relay: Relay) -> Self {
        match relay {
            Relay::Default => "default".into(),
            Relay::Off => "disabled".into(),
            Relay::Custom(url) => url,
        }
    }
}

/// Whether a change of the relay setting to `relay` waits for a restart:
/// the endpoint is bound with another one.
#[must_use]
pub fn needs_restart(relay: &Relay) -> bool {
    BOUND.get().is_some_and(|bound| bound != relay)
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
        let setting = crate::hosts::load_state(home).relay;
        let relay = if pinned { RelayConfig::Disabled } else { setting.config() };
        let net = Net::bind_with(&identity, NetOptions { relay, mdns: !pinned, pin_initial_path: pinned, port: None }).await?;
        let _ = BOUND.set(setting);
        Ok(net)
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

    #[test]
    fn relay_settings_parse_like_the_cli() {
        assert_eq!(Relay::parse("default"), Ok(Relay::Default));
        assert_eq!(Relay::parse(" "), Ok(Relay::Default));
        assert_eq!(Relay::parse("disabled"), Ok(Relay::Off));
        assert_eq!(Relay::parse("Off"), Ok(Relay::Off));
        let Ok(Relay::Custom(url)) = Relay::parse(" https://relay.example.com ") else { panic!("a relay URL") };
        assert!(url.starts_with("https://relay.example.com"), "{url}");
        assert!(matches!(Relay::parse(&url).unwrap().config(), RelayConfig::Custom(_)));
        assert!(Relay::parse("relay.example.com").is_err(), "an address needs its scheme");
        assert!(Relay::parse("ftp://relay.example.com").is_err());
        assert!(Relay::parse("https://").is_err());
        assert!(matches!(Relay::Default.config(), RelayConfig::Default));
        assert!(matches!(Relay::Off.config(), RelayConfig::Disabled));
    }

    #[test]
    fn relay_settings_serialize_as_the_cli_forms() {
        assert_eq!(serde_json::to_string(&Relay::Default).unwrap(), r#""default""#);
        assert_eq!(serde_json::to_string(&Relay::Off).unwrap(), r#""disabled""#);
        let custom = Relay::Custom("https://relay.example.com/".into());
        assert_eq!(serde_json::to_string(&custom).unwrap(), r#""https://relay.example.com/""#);
        assert_eq!(serde_json::from_str::<Relay>(r#""https://relay.example.com/""#).unwrap(), custom);
        assert_eq!(serde_json::from_str::<Relay>(r#""disabled""#).unwrap(), Relay::Off);
        assert_eq!(serde_json::from_str::<Relay>(r#""nonsense""#).unwrap(), Relay::Default, "a bad value is the default");
    }
}
