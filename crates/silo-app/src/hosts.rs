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

/// Whether a host with the same EndpointId is saved already.
#[must_use]
pub fn find<'a>(hosts: &'a [RemoteHost], id: &str) -> Option<&'a RemoteHost> {
    hosts.iter().find(|h| h.id == id)
}

/// Save a paired host: a known EndpointId takes the new route and keeps
/// its name unless the user `chose` one; a new host is named `chosen`,
/// else `suggested` (its hostname or offered name). The saved host.
pub fn save(hosts: &mut Vec<RemoteHost>, addr: EndpointAddr, chosen: Option<String>, suggested: &str) -> RemoteHost {
    let id = addr.id.to_string();
    match hosts.iter_mut().find(|h| h.id == id) {
        Some(host) => {
            host.addr = addr;
            if let Some(name) = chosen {
                host.name = name;
            }
            host.clone()
        }
        None => {
            let host = RemoteHost::new(addr, chosen.unwrap_or_else(|| suggested.to_owned()));
            hosts.push(host.clone());
            host
        }
    }
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
    try_load(home).unwrap_or_default()
}

/// The saved hosts; `None` if the file is unreadable or invalid (a missing
/// file is no hosts).
#[must_use]
pub fn try_load(home: &Path) -> Option<Vec<RemoteHost>> {
    match std::fs::read(hosts_path(home)) {
        Ok(bytes) => serde_json::from_slice::<HostsFile>(&bytes).ok().map(|f| f.hosts),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(Vec::new()),
        Err(_) => None,
    }
}

/// hosts.json's (mtime, length), to notice changes; `None` if missing.
#[must_use]
pub fn stamp(home: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(hosts_path(home)).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Change the saved hosts under an exclusive `flock` of `hosts.json.lock`
/// (shared with the CLI); written (temp file + rename) only if changed.
///
/// # Errors
/// An unreadable or invalid hosts.json (never overwritten), lock or write
/// failure.
pub fn update<R>(home: &Path, change: impl FnOnce(&mut Vec<RemoteHost>) -> R) -> anyhow::Result<R> {
    std::fs::create_dir_all(home)?;
    let lock = std::fs::File::options().create(true).truncate(false).write(true).open(home.join("hosts.json.lock"))?;
    lock.lock()?;
    let mut hosts = try_load(home).ok_or_else(|| anyhow::anyhow!("hosts.json is unreadable"))?;
    let before = hosts.clone();
    let out = change(&mut hosts);
    if hosts != before {
        let tmp = home.join(format!("hosts.json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(&HostsFile { hosts })?)?;
        std::fs::rename(tmp, hosts_path(home))?;
    }
    Ok(out)
}

/// How a host's control connection is made.
#[derive(Clone, Debug)]
pub enum Transport {
    /// This Mac on the unix socket; `service` starts its server (see
    /// `conn::ensure`), or says why it can't.
    Local { home: PathBuf, service: Result<silo_client::service::Service, String> },
    /// A paired remote server over iroh; never spawned or upgraded.
    Iroh { home: PathBuf, addr: EndpointAddr },
    /// A session or block someone shared with a link (`silos1…`): only
    /// what the link's grant covers (see `share.rs`).
    Shared { home: PathBuf, ticket: silo_client::transport::ShareTicket },
}

impl Transport {
    /// Open a control connection with events subscribed.
    ///
    /// # Errors
    /// Connection failures; remote transports are not available yet.
    pub async fn connect(&self, client_name: &str) -> anyhow::Result<(Control, Events)> {
        match self {
            Self::Local { home, .. } => {
                // A server of the other channel (a dev build and the
                // installed Silo sharing a custom SILO_HOME) is never used.
                let (control, events) = match Control::connect(&silo_proto::paths::socket(home), client_name).await {
                    Ok((control, _)) if control.hello().channel != silo_proto::CHANNEL => {
                        return Err(silo_proto::OtherChannel { server: control.hello().channel }.into());
                    }
                    Ok(connected) => connected,
                    Err(error) => match silo_client::mismatch::incompatible_server(home, &error).await {
                        Some(server) => {
                            return Err(match silo_proto::paths::check_runtime_channel(home) {
                                Err(other) => other.into(),
                                Ok(()) => server.into(),
                            });
                        }
                        None => return Err(error),
                    },
                };
                control.subscribe().await?;
                Ok((control, events))
            }
            Self::Iroh { home, addr } => {
                let net = crate::remote::net(home).await?;
                let (control, events) = Control::connect_remote(net, addr.clone(), client_name, false, None).await?;
                control.subscribe().await?;
                Ok((control, events))
            }
            Self::Shared { home, ticket } => {
                let net = crate::remote::net(home).await?;
                let (control, events) = Control::connect_shared(net, ticket, None, client_name).await?;
                control.subscribe().await?;
                Ok((control, events))
            }
        }
    }

    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Iroh { .. } | Self::Shared { .. })
    }

    #[must_use]
    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Shared { .. })
    }

    /// The paired remote server's EndpointId (not a shared link's: those
    /// are no paired hosts, and are kept outside hosts.json).
    #[must_use]
    pub fn remote_id(&self) -> Option<String> {
        match self {
            Self::Local { .. } | Self::Shared { .. } => None,
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
            Self::Shared { ticket, .. } => crate::share::key(ticket),
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
    /// The remote server speaks another protocol: upgrade it (`silo remote
    /// upgrade`). Same-protocol builds connect normally.
    Incompatible,
}

#[derive(Clone, Debug)]
pub struct HostInfo {
    pub name: String,
    pub local: bool,
    pub transport: Transport,
}

impl HostInfo {
    #[must_use]
    pub fn local(home: PathBuf, service: Result<silo_client::service::Service, String>) -> Self {
        Self { name: "This Mac".into(), local: true, transport: Transport::Local { home, service } }
    }

    #[must_use]
    pub fn remote(home: PathBuf, host: &RemoteHost) -> Self {
        Self { name: host.name.clone(), local: false, transport: Transport::Iroh { home, addr: host.addr.clone() } }
    }

    #[must_use]
    pub fn shared(home: PathBuf, name: String, ticket: silo_client::transport::ShareTicket) -> Self {
        Self { name, local: false, transport: Transport::Shared { home, ticket } }
    }

    /// Stable across launches and routes: `local`, the remote server's
    /// EndpointId, or `share:<link>`. Names the host in notification
    /// settings and payloads.
    #[must_use]
    pub fn key(&self) -> String {
        match &self.transport {
            Transport::Shared { ticket, .. } => crate::share::key(ticket),
            transport => transport.remote_id().unwrap_or_else(|| LOCAL_KEY.to_owned()),
        }
    }
}

/// [`HostInfo::key`] of this Mac.
pub const LOCAL_KEY: &str = "local";

/// Which notifications this device shows (Settings › Notifications).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyPrefs {
    #[serde(default = "yes")]
    pub on: bool,
    /// A program needs you (blocked).
    #[serde(default = "yes")]
    pub blocked: bool,
    #[serde(default = "yes")]
    pub done: bool,
    #[serde(default = "yes")]
    pub error: bool,
    /// Programs' own notifications (OSC 9 / 777).
    #[serde(default = "yes")]
    pub program: bool,
    /// Only for blocks not on screen.
    #[serde(default = "yes")]
    pub only_hidden: bool,
}

impl Default for NotifyPrefs {
    fn default() -> Self {
        Self { on: true, blocked: true, done: true, error: true, program: true, only_hidden: true }
    }
}

/// One switch of [`NotifyPrefs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyField {
    On,
    Blocked,
    Done,
    Error,
    Program,
    OnlyHidden,
}

impl NotifyPrefs {
    #[must_use]
    pub fn get(&self, field: NotifyField) -> bool {
        match field {
            NotifyField::On => self.on,
            NotifyField::Blocked => self.blocked,
            NotifyField::Done => self.done,
            NotifyField::Error => self.error,
            NotifyField::Program => self.program,
            NotifyField::OnlyHidden => self.only_hidden,
        }
    }

    pub fn set(&mut self, field: NotifyField, on: bool) {
        *match field {
            NotifyField::On => &mut self.on,
            NotifyField::Blocked => &mut self.blocked,
            NotifyField::Done => &mut self.done,
            NotifyField::Error => &mut self.error,
            NotifyField::Program => &mut self.program,
            NotifyField::OnlyHidden => &mut self.only_hidden,
        } = on;
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
    /// Which block types show their header.
    #[serde(default)]
    pub pane_headers: crate::pane_header::PaneHeaders,
    /// UI (chrome) font: an exact installed family, or empty for Automatic
    /// (the macOS system font). Separate from the terminal font in
    /// `terminal.json`. Absent in older files, which keeps the default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ui_font_family: String,
    /// Card padding and window chrome take on the running app's background.
    #[serde(default = "yes")]
    pub match_running_app: bool,
    /// Window material; Frosted renders only where blur is available.
    #[serde(default)]
    pub material: crate::material::Material,
    /// Frosted: the running app's colour tints the veils.
    #[serde(default = "yes")]
    pub tint_with_app: bool,
    /// Frosted: an app's own backdrop is veiled too, not just the default.
    #[serde(default)]
    pub translucent_apps: bool,
    /// Look for a newer release at launch and every few hours.
    #[serde(default = "yes")]
    pub check_updates: bool,
    /// Ask before a close that would end running programs.
    #[serde(default = "yes")]
    pub confirm_close: bool,
    /// What the address bar searches with when the text is not an address.
    #[serde(default)]
    pub search_engine: crate::search::SearchEngine,
    /// Pages visited in browser blocks, newest first, per host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_pages: Vec<crate::browser_home::RecentPage>,
    #[serde(default)]
    pub notifications: NotifyPrefs,
    /// Hosts ([`HostInfo::key`]) whose notifications are off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quiet_hosts: Vec<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            last_session: None,
            density: Density::default(),
            pane_headers: crate::pane_header::PaneHeaders::default(),
            ui_font_family: String::new(),
            match_running_app: true,
            material: crate::material::Material::Solid,
            tint_with_app: true,
            translucent_apps: false,
            check_updates: true,
            confirm_close: true,
            search_engine: crate::search::SearchEngine::default(),
            recent_pages: Vec::new(),
            notifications: NotifyPrefs::default(),
            quiet_hosts: Vec::new(),
        }
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

    /// This Mac's transport for `home`, never managed by launchd.
    pub(crate) fn local_transport(home: &std::path::Path) -> Transport {
        let service = silo_client::service::Service::new(
            silo_client::service::Manager::None,
            "/nonexistent/silo".into(),
            home.to_owned(),
            home.to_owned(),
            501,
            "test".into(),
        )
        .map_err(|e| e.to_string());
        Transport::Local { home: home.to_owned(), service }
    }

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
        update(&dir, |hosts| hosts.push(host.clone())).unwrap();
        assert!(dir.join("hosts.json.lock").exists());
        assert!(!dir.join(format!("hosts.json.{}.tmp", std::process::id())).exists());
        // An unreadable file is never overwritten.
        let bad = temp("hosts-bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("hosts.json"), b"{").unwrap();
        assert!(update(&bad, |hosts| hosts.push(host.clone())).is_err());
        assert_eq!(std::fs::read(bad.join("hosts.json")).unwrap(), b"{");
        let _ = std::fs::remove_dir_all(bad);
        let loaded = load(&dir);
        assert_eq!(loaded, vec![host.clone()]);
        assert!(find(&loaded, &host.id).is_some(), "same EndpointId is a duplicate");
        assert!(find(&loaded, "other").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_known_host_keeps_its_name_when_paired_again() {
        let dir = temp("save");
        let mut hosts = Vec::new();
        let first = addr(&dir.join("a"));
        assert_eq!(save(&mut hosts, first.clone(), None, "batmini.local").name, "batmini.local", "a new host takes the suggestion");
        hosts[0].name = "mini".into();
        // Reinstalled or re-offered: same EndpointId, new route.
        let moved = EndpointAddr::new(first.id).with_ip_addr("10.0.0.2:5000".parse().unwrap());
        let saved = save(&mut hosts, moved.clone(), None, "batmini.local");
        assert_eq!((saved.name.as_str(), &saved.addr), ("mini", &moved));
        assert_eq!(hosts, vec![saved]);
        // A name the user types wins.
        assert_eq!(save(&mut hosts, moved, Some("studio".into()), "batmini.local").name, "studio");
        // A fresh key is another host, named afresh.
        let other = save(&mut hosts, addr(&dir.join("b")), None, "batmini.local");
        assert_eq!((other.name.as_str(), hosts.len()), ("batmini.local", 2));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn server_reported_addr_is_stored_as_the_cli_does() {
        let dir = temp("merge");
        let old = addr(&dir.join("a"));
        update(&dir, |hosts| hosts.push(RemoteHost::new(old.clone(), "a".into()))).unwrap();
        let moved = EndpointAddr::new(old.id).with_ip_addr("10.0.0.2:5000".parse().unwrap());
        silo_client::hosts::store_addr(&dir, moved.clone()).unwrap();
        assert_eq!(load(&dir)[0].addr, moved);
        assert_eq!((load(&dir)[0].id.as_str(), load(&dir)[0].name.as_str()), (old.id.to_string().as_str(), "a"), "id and name are kept");
        // An address with no routes, or of another server, is ignored.
        silo_client::hosts::store_addr(&dir, EndpointAddr::new(old.id)).unwrap();
        silo_client::hosts::store_addr(&dir, addr(&dir.join("b"))).unwrap();
        assert_eq!(load(&dir).iter().map(|h| &h.addr).collect::<Vec<_>>(), [&moved]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_identity_per_transport() {
        let dir = temp("identity");
        let local = local_transport(&dir);
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
        assert_eq!(old.pane_headers, crate::pane_header::PaneHeaders::all(true));
        assert_eq!(old.ui_font_family, "", "Automatic UI font");
        assert!(old.match_running_app);
        assert_eq!((old.material, old.tint_with_app, old.translucent_apps), (crate::material::Material::Solid, true, false));
        assert!(old.check_updates, "updates are checked unless turned off");
        assert!(old.confirm_close, "closing running programs asks unless turned off");
        assert_eq!(old.search_engine, crate::search::SearchEngine::Google, "pages search with Google unless set");
        let dir = std::env::temp_dir().join(format!("silo-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState {
            last_session: None,
            density: Density::Compact,
            pane_headers: crate::pane_header::PaneHeaders { terminal: false, agent: true },
            ui_font_family: String::new(),
            match_running_app: false,
            material: crate::material::Material::Frosted,
            tint_with_app: false,
            translucent_apps: true,
            check_updates: false,
            confirm_close: false,
            search_engine: crate::search::SearchEngine::Kagi,
            recent_pages: Vec::new(),
            notifications: NotifyPrefs { done: false, ..NotifyPrefs::default() },
            quiet_hosts: vec!["local".into()],
        };
        save_state(&dir, &state);
        let text = std::fs::read_to_string(dir.join("app.json")).unwrap();
        assert!(!text.contains("ui_font_family"), "Automatic is not written: {text}");
        let back = load_state(&dir);
        assert_eq!((back.density, back.pane_headers, back.match_running_app), (Density::Compact, state.pane_headers, false));
        assert_eq!((back.material, back.tint_with_app, back.translucent_apps), (crate::material::Material::Frosted, false, true));
        assert!(!back.check_updates);
        assert!(!back.confirm_close);
        assert_eq!((back.notifications.done, back.notifications.blocked), (false, true));
        assert_eq!(back.quiet_hosts, ["local"]);
        assert_eq!(old.notifications, NotifyPrefs::default(), "notifications are on unless turned off");
        let partial: AppState = serde_json::from_str(r#"{"notifications":{"done":false}}"#).unwrap();
        assert!(partial.notifications.on && partial.notifications.only_hidden && !partial.notifications.done);
        // The exact family persists, independent of the terminal font.
        let state = AppState { ui_font_family: "Helvetica Neue".into(), ..state };
        save_state(&dir, &state);
        assert_eq!(load_state(&dir).ui_font_family, "Helvetica Neue");
        assert!(!dir.join("terminal.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
