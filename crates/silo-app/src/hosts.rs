//! Hosts the app can reach: this Mac's daemon plus saved remote hosts
//! (`hosts.json`). Each host connects through a [`Transport`]; the unix
//! socket is implemented, iroh plugs in as another variant later.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use silo_client::{Control, Events, transport::EndpointAddr};

/// A saved, paired remote host (`hosts.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHost {
    /// The server's canonical EndpointId.
    pub id: String,
    pub name: String,
    /// Last known route: relay and direct addresses.
    pub addr: EndpointAddr,
}

impl RemoteHost {
    #[must_use]
    pub fn new(addr: EndpointAddr, name: String) -> Self {
        Self { id: addr.id.to_string(), name, addr }
    }
}

/// Take a fresher route reported by the server: only for the same
/// EndpointId and only when it changed. Returns whether `hosts` changed.
pub fn merge_addr(hosts: &mut [RemoteHost], addr: &EndpointAddr) -> bool {
    let id = addr.id.to_string();
    match hosts.iter_mut().find(|h| h.id == id) {
        Some(host) if host.addr != *addr && !addr.is_empty() => {
            host.addr = addr.clone();
            true
        }
        _ => false,
    }
}

/// Whether a host with the same EndpointId is saved already.
#[must_use]
pub fn find<'a>(hosts: &'a [RemoteHost], id: &str) -> Option<&'a RemoteHost> {
    hosts.iter().find(|h| h.id == id)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct HostsFile {
    #[serde(default)]
    hosts: Vec<RemoteHost>,
}

fn hosts_path(home: &Path) -> PathBuf {
    home.join("hosts.json")
}

#[must_use]
pub fn load(home: &Path) -> Vec<RemoteHost> {
    std::fs::read(hosts_path(home)).ok().and_then(|b| serde_json::from_slice::<HostsFile>(&b).ok()).map(|f| f.hosts).unwrap_or_default()
}

/// # Errors
/// Write failure.
pub fn save(home: &Path, hosts: &[RemoteHost]) -> anyhow::Result<()> {
    std::fs::create_dir_all(home)?;
    let file = HostsFile { hosts: hosts.to_vec() };
    let tmp = hosts_path(home).with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
    std::fs::rename(tmp, hosts_path(home))?;
    Ok(())
}

/// How a host's control connection is made.
#[derive(Clone, Debug)]
pub enum Transport {
    /// This Mac: adopt or spawn the daemon on the unix socket.
    Local { home: PathBuf, server_exe: PathBuf },
    /// A paired remote server over iroh; never spawned or upgraded.
    Iroh { home: PathBuf, addr: EndpointAddr },
}

impl Transport {
    /// Open a control connection with events subscribed.
    ///
    /// # Errors
    /// Connection failures; remote transports are not available yet.
    pub async fn connect(&self, client_name: &str) -> anyhow::Result<(Control, Events)> {
        match self {
            Self::Local { home, server_exe } => {
                let (control, events) = silo_client::connect_or_spawn(home, server_exe, client_name).await?;
                control.subscribe().await?;
                Ok((control, events))
            }
            Self::Iroh { home, addr } => {
                let net = crate::remote::net(home).await?;
                let (control, events) = Control::connect_remote(net, addr.clone(), client_name, false, None).await?;
                control.subscribe().await?;
                Ok((control, events))
            }
        }
    }

    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Iroh { .. })
    }

    /// The remote server's EndpointId.
    #[must_use]
    pub fn remote_id(&self) -> Option<String> {
        match self {
            Self::Local { .. } => None,
            Self::Iroh { addr, .. } => Some(addr.id.to_string()),
        }
    }

    /// Key of this host's client cache: the socket path locally,
    /// `iroh:<EndpointId>` remotely (stable across address changes).
    #[must_use]
    pub fn cache_identity(&self) -> String {
        match self {
            Self::Local { home, .. } => silo_proto::paths::socket(home).to_string_lossy().into_owned(),
            Self::Iroh { addr, .. } => format!("iroh:{}", addr.id),
        }
    }
}

/// Connection state of a host, as shown in the switcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostState {
    Connecting,
    Connected,
    Unreachable(String),
    /// The remote server does not know this device (revoked, reset).
    NotPaired,
    /// The remote server is another build; it can't be upgraded from here.
    DifferentBuild,
}

#[derive(Clone, Debug)]
pub struct HostInfo {
    pub name: String,
    pub local: bool,
    pub transport: Transport,
}

impl HostInfo {
    #[must_use]
    pub fn local(home: PathBuf, server_exe: PathBuf) -> Self {
        Self { name: "This Mac".into(), local: true, transport: Transport::Local { home, server_exe } }
    }

    #[must_use]
    pub fn remote(home: PathBuf, host: &RemoteHost) -> Self {
        Self { name: host.name.clone(), local: false, transport: Transport::Iroh { home, addr: host.addr.clone() } }
    }
}

/// Pane layout density.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    /// Flush panes meeting at thin dividers.
    Compact,
    /// Inset rounded pane cards with small gaps.
    #[default]
    Comfortable,
}

fn yes() -> bool {
    true
}

/// App-side state and settings remembered across launches
/// (`$SILO_HOME/app.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub last_session: Option<String>,
    #[serde(default)]
    pub density: Density,
    #[serde(default = "yes")]
    pub show_pane_headers: bool,
    /// UI (chrome) font: an exact installed family, or empty for Automatic
    /// (the macOS system font). Separate from the terminal font in
    /// `terminal.json`. Absent in older files, which keeps the default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ui_font_family: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self { last_session: None, density: Density::default(), show_pane_headers: true, ui_font_family: String::new() }
    }
}

#[must_use]
pub fn load_state(home: &Path) -> AppState {
    std::fs::read(home.join("app.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_state(home: &Path, state: &AppState) {
    if let Ok(bytes) = serde_json::to_vec_pretty(state) {
        let _ = std::fs::write(home.join("app.json"), bytes);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("silo-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A fresh endpoint address with a relay and a direct address.
    pub(crate) fn addr(dir: &Path) -> EndpointAddr {
        let id = silo_client::client_identity(dir).unwrap().id();
        EndpointAddr::new(id).with_relay_url("https://relay.example.org".parse().unwrap()).with_ip_addr("192.168.1.7:4433".parse().unwrap())
    }

    #[test]
    fn hosts_round_trip_by_endpoint_id() {
        let dir = temp("hosts");
        let host = RemoteHost::new(addr(&dir.join("key")), "mini".into());
        assert_eq!(host.id, host.addr.id.to_string());
        save(&dir, std::slice::from_ref(&host)).unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded, vec![host.clone()]);
        assert!(find(&loaded, &host.id).is_some(), "same EndpointId is a duplicate");
        assert!(find(&loaded, "other").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn server_reported_addr_merges() {
        let dir = temp("merge");
        let mut hosts = vec![RemoteHost::new(addr(&dir.join("a")), "a".into())];
        let old = hosts[0].addr.clone();
        assert!(!merge_addr(&mut hosts, &old), "unchanged");
        let moved = EndpointAddr::new(old.id).with_ip_addr("10.0.0.2:5000".parse().unwrap());
        assert!(merge_addr(&mut hosts, &moved));
        assert_eq!(hosts[0].addr, moved);
        assert_eq!(hosts[0].id, old.id.to_string(), "id is kept");
        // An address with no routes, or of another server, is ignored.
        assert!(!merge_addr(&mut hosts, &EndpointAddr::new(old.id)));
        assert!(!merge_addr(&mut hosts, &addr(&dir.join("b"))));
        assert_eq!(hosts[0].addr, moved);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_identity_per_transport() {
        let dir = temp("identity");
        let local = Transport::Local { home: dir.clone(), server_exe: "silo".into() };
        assert_eq!(local.cache_identity(), dir.join("server.sock").to_string_lossy());
        let a = addr(&dir.join("key"));
        let remote = Transport::Iroh { home: dir.clone(), addr: a.clone() };
        assert_eq!(remote.cache_identity(), format!("iroh:{}", a.id));
        // Another route to the same server keeps the same cache.
        let moved = Transport::Iroh { home: dir.clone(), addr: EndpointAddr::new(a.id) };
        assert_eq!(moved.cache_identity(), remote.cache_identity());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_default_and_round_trip() {
        // Older files without settings get the reference design.
        let old: AppState = serde_json::from_str(r#"{"last_session":"s1"}"#).unwrap();
        assert_eq!(old.density, Density::Comfortable);
        assert!(old.show_pane_headers);
        assert_eq!(old.ui_font_family, "", "Automatic UI font");
        let dir = std::env::temp_dir().join(format!("silo-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState { last_session: None, density: Density::Compact, show_pane_headers: false, ui_font_family: String::new() };
        save_state(&dir, &state);
        let text = std::fs::read_to_string(dir.join("app.json")).unwrap();
        assert!(!text.contains("ui_font_family"), "Automatic is not written: {text}");
        let back = load_state(&dir);
        assert_eq!((back.density, back.show_pane_headers), (Density::Compact, false));
        // The exact family persists, independent of the terminal font.
        let state = AppState { ui_font_family: "Helvetica Neue".into(), ..state };
        save_state(&dir, &state);
        assert_eq!(load_state(&dir).ui_font_family, "Helvetica Neue");
        assert!(!dir.join("terminal.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
