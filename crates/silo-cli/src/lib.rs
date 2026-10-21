//! Command logic of the `silo` CLI, callable from tests.

pub mod clipboard;
pub mod hosts;
pub mod link;
pub mod update;
pub mod upgrade;

use std::{path::Path, time::Duration};

use anyhow::{Context, bail};
use silo_client::{
    Control, Events, IncompatibleProtocol, Offline, Unauthorized,
    transport::{EndpointAddr, Net, PairTicket, RelayConfig, RemoteConn, Unreachable},
};
use silo_proto::{
    control::{Method, PairInvite, PairedDevice, PairedDevices, ServerInfo},
    model::{BlockKind, SessionSummary, SessionView},
};

use crate::hosts::{Host, Hosts, client_dir};

/// `90`, `90s`, `10m`, `2h` or `1d`.
///
/// # Errors
/// Not a positive duration.
pub fn parse_duration(text: &str) -> anyhow::Result<Duration> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().with_context(|| format!("invalid duration {text:?}"))?;
    let scale = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("invalid duration unit {unit:?} (use s, m, h or d)"),
    };
    if number == 0 {
        bail!("duration must be positive");
    }
    Ok(Duration::from_secs(number * scale))
}

/// `default`, `disabled` or a relay URL.
///
/// # Errors
/// An invalid URL.
pub fn parse_relay(text: &str) -> anyhow::Result<RelayConfig> {
    Ok(match text {
        "default" => RelayConfig::Default,
        "disabled" => RelayConfig::Disabled,
        url => RelayConfig::Custom(url.parse().with_context(|| format!("invalid relay url {url:?}"))?),
    })
}

/// A compact QR code of `text` drawn with half-block characters.
///
/// # Errors
/// `text` is too long for a QR code.
pub fn qr(text: &str) -> anyhow::Result<String> {
    use qrcode::render::unicode::Dense1x2;
    // Ticket decoding ignores case; uppercase fits the denser alphanumeric mode.
    let code = qrcode::QrCode::new(text.to_ascii_uppercase()).context("encode QR")?;
    Ok(code.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).quiet_zone(true).build())
}

/// How the client's endpoint reaches hosts.
#[derive(Clone, Debug)]
pub struct NetOptions {
    pub relay: RelayConfig,
    pub mdns: bool,
}

impl Default for NetOptions {
    fn default() -> Self {
        Self { relay: RelayConfig::Default, mdns: true }
    }
}

/// A connection to a remote host. Never spawns a server.
pub struct Remote {
    pub control: Control,
    pub events: Events,
    pub net: Net,
}

async fn bind(home: &Path, options: &NetOptions) -> anyhow::Result<Net> {
    let identity = silo_client::client_identity(&client_dir(home)).context("load client identity")?;
    Net::bind(&identity, options.relay.clone(), options.mdns).await
}

/// A clear message for the typed remote failures.
#[must_use]
pub fn explain(error: anyhow::Error) -> anyhow::Error {
    if let Some(e) = error.downcast_ref::<IncompatibleProtocol>() {
        return anyhow::anyhow!("the remote host runs a different silo build ({e}); install the same build on both sides");
    }
    if let Some(e) = error.downcast_ref::<Unauthorized>() {
        return anyhow::anyhow!("the remote host refused this device ({e}); pair again with `silo hosts add <ticket>`");
    }
    if error.downcast_ref::<Offline>().is_some() {
        return anyhow::anyhow!("the server has no reachable address yet (relay not connected); retry in a moment");
    }
    if let Some(e) = error.downcast_ref::<Unreachable>() {
        return anyhow::anyhow!("could not reach the host ({}); is its silo running?", e.reason);
    }
    error
}

/// Dial the stored `host`, as every command reaching a paired host does.
/// No limit of its own: when the stored addresses are outdated (each
/// restart of a host moves its ports) iroh needs the time it allows for
/// address lookup and the relay.
///
/// # Errors
/// Nothing answered (see [`explain`]) or another dial failure.
pub async fn reach(net: &Net, host: &Host) -> anyhow::Result<RemoteConn> {
    net.dial(host.addr.clone()).await.map_err(|error| {
        if let Some(e) = error.downcast_ref::<Unreachable>() {
            return anyhow::anyhow!("could not reach {} ({}); is its silo running?", host.name, e.reason);
        }
        error.context(format!("reach {}", host.name))
    })
}

/// Connect to `host`: a pairing ticket (redeemed now, not stored) or a
/// stored host's name, id or id prefix.
///
/// # Errors
/// Unknown host, dial failure, or a typed refusal (see [`explain`]).
pub async fn connect_remote(home: &Path, host: &str, options: &NetOptions, client_name: &str) -> anyhow::Result<Remote> {
    let net = bind(home, options).await?;
    let result =
        if host.get(..silo_client::transport::TICKET_PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(silo_client::transport::TICKET_PREFIX)) {
            let ticket = PairTicket::decode(host)?;
            silo_client::pair(&net, &ticket, client_name).await
        } else {
            let host = Hosts::load(home)?.find(host)?.clone();
            match reach(&net, &host).await {
                Ok(rc) => Control::over_remote(rc, client_name, false, None).await,
                Err(error) => Err(error),
            }
        };
    match result {
        Ok((control, events)) => {
            refresh_host(home, &control).await?;
            Ok(Remote { control, events, net })
        }
        Err(error) => {
            let _ = net.close().await;
            Err(explain(error))
        }
    }
}

/// Redeem `ticket` and store the host (see [`hosts::Hosts::pair`]): named
/// `name` when the user chose one, else as before, else by the server's
/// hostname.
///
/// # Errors
/// Pairing failed or `hosts.json` could not be written.
pub async fn add_host(home: &Path, ticket: &str, name: Option<String>, options: &NetOptions, client_name: &str) -> anyhow::Result<hosts::Host> {
    let ticket = PairTicket::decode(ticket)?;
    let net = bind(home, options).await?;
    let paired = match silo_client::pair(&net, &ticket, client_name).await {
        // Prefer the server's own view of its address over the ticket's.
        Ok((control, _)) => {
            let hostname = control.hello().server.hostname.clone();
            current_addr(&control).await.map(|addr| (addr.filter(|a| a.id == ticket.addr.id), hostname))
        }
        Err(error) => Err(explain(error)),
    };
    let _ = net.close().await;
    let (addr, hostname) = paired?;
    let addr = addr.unwrap_or(ticket.addr);
    Hosts::update(home, |hosts| Ok(hosts.pair(addr, name, &hostname)?.clone()))
}

/// Fresh `server.info`.
///
/// # Errors
/// RPC failure.
pub async fn server_info(control: &Control) -> anyhow::Result<ServerInfo> {
    control.call(Method::ServerInfo {}).await
}

/// The server's current address from `server.info`, if it reports one.
///
/// # Errors
/// RPC failure or an undecodable address.
pub async fn current_addr(control: &Control) -> anyhow::Result<Option<EndpointAddr>> {
    let info = server_info(control).await?;
    info.endpoint_addr.map(serde_json::from_value).transpose().context("decode server endpoint_addr")
}

/// Update the stored address of the connected host when it moved. Hosts
/// not in `hosts.json` (an unsaved ticket) are left alone.
///
/// # Errors
/// RPC failure or `hosts.json` I/O.
pub async fn refresh_host(home: &Path, control: &Control) -> anyhow::Result<()> {
    match current_addr(control).await? {
        Some(addr) => store_addr(home, addr),
        None => Ok(()),
    }
}

/// Store `addr` (a host's own report of its address) for the stored host
/// with its id when it moved; an address without any route is ignored.
///
/// # Errors
/// `hosts.json` I/O.
pub fn store_addr(home: &Path, addr: EndpointAddr) -> anyhow::Result<()> {
    if addr.is_empty() {
        return Ok(());
    }
    let id = addr.id.to_string();
    if Hosts::load(home)?.hosts.iter().any(|h| h.id == id && h.addr != addr) {
        Hosts::update(home, |hosts| {
            if hosts.hosts.iter().any(|h| h.id == id) {
                hosts.upsert(addr, None)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// `pair.create` with `ttl`.
///
/// # Errors
/// RPC failure; [`Offline`] is explained.
pub async fn create_invite(control: &Control, ttl: Duration) -> anyhow::Result<PairInvite> {
    control.call(Method::PairCreate { ttl_secs: ttl.as_secs(), purpose: silo_proto::control::InvitePurpose::Device }).await.map_err(explain)
}

/// # Errors
/// RPC failure.
pub async fn devices(control: &Control) -> anyhow::Result<Vec<PairedDevice>> {
    let devices: PairedDevices = control.call(Method::PairList {}).await?;
    Ok(devices.devices)
}

/// Revoke the device whose id is or starts with `key`.
///
/// # Errors
/// No match, an ambiguous prefix or RPC failure.
pub async fn revoke(control: &Control, key: &str) -> anyhow::Result<PairedDevice> {
    let devices = devices(control).await?;
    let mut matches = devices.iter().filter(|d| d.id.starts_with(key));
    let device = match (matches.next(), matches.next()) {
        (Some(device), None) => device.clone(),
        (Some(_), Some(_)) => bail!("{key:?} matches several devices"),
        _ => bail!("no paired device {key:?}"),
    };
    control.call_value(Method::PairRevoke { id: device.id.clone() }).await?;
    Ok(device)
}

/// # Errors
/// RPC failure.
pub async fn sessions(control: &Control) -> anyhow::Result<Vec<SessionSummary>> {
    control.call(Method::SessionList {}).await
}

/// The sessions stopping the server would lose something in: all of them,
/// unless the only one is an idle default (one window holding one running
/// shell, at its prompt, nobody attached), which a service server starts
/// by itself. `view` is the only session's view; `shell` the login shell.
/// A shell that set its own title reads as busy.
#[must_use]
pub fn sessions_to_end(sessions: &[SessionSummary], view: Option<&SessionView>, shell: Option<&str>) -> usize {
    let idle = |summary: &SessionSummary, view: &SessionView| {
        let [block] = view.blocks.as_slice() else { return false };
        let shell = shell.and_then(|s| Path::new(s).file_name()?.to_str());
        let title = block.title.trim_start_matches('-');
        summary.windows == 1
            && summary.attached_clients == 0
            && block.kind == BlockKind::Terminal
            && block.lifecycle.is_running()
            && (title == "shell" || shell == Some(title))
    };
    match (sessions, view) {
        ([summary], Some(view)) if idle(summary, view) => 0,
        _ => sessions.len(),
    }
}

/// One line per session, as `silo ls` prints them.
#[must_use]
pub fn format_sessions(sessions: &[SessionSummary]) -> String {
    let mut out = String::new();
    for s in sessions {
        let attached = if s.attached_clients > 0 { format!(" ({} attached)", s.attached_clients) } else { String::new() };
        out.push_str(&format!("{}: {} windows, {} blocks{attached}  [{}]\n", s.name, s.windows, s.blocks, s.id));
    }
    out
}

/// `3m ago`-style age of a unix-seconds timestamp.
#[must_use]
pub fn ago(unix_secs: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let secs = now.saturating_sub(unix_secs);
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use silo_client::transport::{EndpointAddr, Identity};

    use super::*;

    #[test]
    fn idle_default_session_is_not_worth_asking() {
        let summary = |windows: usize, attached_clients: usize| SessionSummary {
            id: "s1".into(),
            name: "halka-pebble".into(),
            windows,
            blocks: 1,
            attached_clients,
            created_at: 0,
        };
        let view = |blocks: &[(&str, &str)]| -> SessionView {
            let blocks: Vec<_> = blocks
                .iter()
                .enumerate()
                .map(|(i, (title, lifecycle))| {
                    serde_json::json!({"id": format!("b{i}"), "session_id": "s1", "kind": "terminal", "title": title,
                        "lifecycle": lifecycle, "size": {"columns": 80, "rows": 24}, "owner_name": null})
                })
                .collect();
            serde_json::from_value(serde_json::json!({
                "session": {"id": "s1", "name": "halka-pebble", "windows": [], "active_window": "w1", "revision": 0, "created_at": 0},
                "blocks": blocks,
            }))
            .unwrap()
        };
        let zsh = Some("/bin/zsh");
        assert_eq!(sessions_to_end(&[summary(1, 0)], Some(&view(&[("zsh", "running")])), zsh), 0);
        assert_eq!(sessions_to_end(&[summary(1, 0)], Some(&view(&[("-zsh", "running")])), zsh), 0);
        assert_eq!(sessions_to_end(&[summary(1, 0)], Some(&view(&[("shell", "running")])), None), 0);
        // A program running, a title of its own, a client, more blocks or windows.
        assert_eq!(sessions_to_end(&[summary(1, 0)], Some(&view(&[("vim", "running")])), zsh), 1);
        assert_eq!(sessions_to_end(&[summary(1, 0)], Some(&view(&[("me@box: ~", "running")])), zsh), 1);
        assert_eq!(sessions_to_end(&[summary(1, 0)], Some(&view(&[("zsh", "exited")])), zsh), 1);
        assert_eq!(sessions_to_end(&[summary(1, 1)], Some(&view(&[("zsh", "running")])), zsh), 1);
        assert_eq!(sessions_to_end(&[summary(2, 0)], Some(&view(&[("zsh", "running")])), zsh), 1);
        assert_eq!(sessions_to_end(&[summary(1, 0)], Some(&view(&[("zsh", "running"), ("zsh", "running")])), zsh), 1);
        assert_eq!(sessions_to_end(&[summary(1, 0), summary(1, 0)], None, zsh), 2);
        assert_eq!(sessions_to_end(&[], None, zsh), 0);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
        for bad in ["", "m", "0", "10x", "-5", "1.5m"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn relays() {
        assert_eq!(parse_relay("default").unwrap(), RelayConfig::Default);
        assert_eq!(parse_relay("disabled").unwrap(), RelayConfig::Disabled);
        assert!(matches!(parse_relay("https://relay.example.com").unwrap(), RelayConfig::Custom(_)));
        assert!(parse_relay("not a url").is_err());
    }

    fn addr(dir: &Path) -> EndpointAddr {
        EndpointAddr::new(Identity::load_or_create(dir).unwrap().id())
    }

    #[test]
    fn qr_renders_a_ticket() {
        let dir = std::env::temp_dir().join(format!("silo-cli-qr-{}", std::process::id()));
        let ticket = PairTicket { addr: addr(&dir).with_ip_addr(([192, 168, 1, 20], 4433).into()), secret: [9; 32] };
        let art = qr(&ticket.encode()).unwrap();
        assert!(art.lines().count() > 10);
        assert!(art.contains('\u{2580}') || art.contains('\u{2584}') || art.contains('\u{2588}'));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hosts_round_trip_and_dedupe() {
        let home = std::env::temp_dir().join(format!("silo-cli-hosts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (a, b) = (addr(&home.join("a")), addr(&home.join("b")));
        assert!(Hosts::load(&home).unwrap().hosts.is_empty());
        let moved = a.clone().with_ip_addr(([10, 0, 0, 1], 1).into());
        let hosts = Hosts::update(&home, |hosts| {
            hosts.upsert(a.clone(), Some("work".into())).unwrap();
            hosts.upsert(b.clone(), None).unwrap();
            assert!(hosts.upsert(b.clone(), Some("work".into())).is_err(), "names are unique");
            // Same id again: updated in place, name kept.
            hosts.upsert(moved.clone(), None).unwrap();
            assert_eq!(hosts.hosts.len(), 2);
            Ok(hosts.clone())
        })
        .unwrap();

        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(home.join("hosts.json")).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        let loaded = Hosts::load(&home).unwrap();
        assert_eq!(loaded, hosts);
        assert_eq!(loaded.find("work").unwrap().addr, moved);
        assert_eq!(loaded.find(&a.id.to_string()[..6]).unwrap().name, "work");
        assert_eq!(loaded.find(&b.id.to_string()).unwrap().name, b.id.to_string()[..10]);
        assert!(loaded.find("nope").is_err());

        Hosts::update(&home, |hosts| hosts.remove("work").map(drop)).unwrap();
        assert_eq!(Hosts::load(&home).unwrap().hosts.len(), 1);
        // A failed change writes nothing; an unreadable file is never overwritten.
        assert!(Hosts::update(&home, |hosts| hosts.remove("nope").map(drop)).is_err());
        assert_eq!(Hosts::load(&home).unwrap().hosts.len(), 1);
        std::fs::write(home.join("hosts.json"), "{ not json").unwrap();
        assert!(Hosts::update(&home, |_| Ok(())).is_err());
        assert_eq!(std::fs::read_to_string(home.join("hosts.json")).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_known_host_keeps_its_name_when_paired_again() {
        let home = std::env::temp_dir().join(format!("silo-cli-repair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (a, b) = (addr(&home.join("a")), addr(&home.join("b")));
        let mut hosts = Hosts::default();
        assert_eq!(hosts.pair(a.clone(), None, "batmini.local").unwrap().name, "batmini.local", "a new host takes its hostname");
        hosts.hosts[0].name = "mini".into();
        // `silo remote install` again: same key, new route, name kept.
        let moved = a.clone().with_ip_addr(([10, 0, 0, 1], 1).into());
        let host = hosts.pair(moved.clone(), None, "batmini.local").unwrap().clone();
        assert_eq!((host.name.as_str(), &host.addr, hosts.hosts.len()), ("mini", &moved, 1));
        // `silo hosts add --name` is the user's choice.
        assert_eq!(hosts.pair(moved, Some("studio".into()), "batmini.local").unwrap().name, "studio");
        // A fresh key is another host, named afresh.
        assert_eq!(hosts.pair(b, None, "studio").unwrap().name, "studio 2");
        let _ = std::fs::remove_dir_all(home);
    }
}
