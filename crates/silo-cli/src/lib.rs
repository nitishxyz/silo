//! Command logic of the `silo` CLI, callable from tests.

pub mod clipboard;
pub mod hooks;
pub mod link;
pub mod mcp;
pub mod update;
pub mod upgrade;

use std::{path::Path, time::Duration};

use anyhow::{Context, bail};
use silo_client::{
    Control, Events, IncompatibleProtocol, Offline, Unauthorized,
    transport::{EndpointId, Identity, Net, PairTicket, RelayConfig, RemoteConn, Unreachable},
};
use silo_proto::{
    access::{Grant, Rights, Scope, Who},
    control::{BusyBlock, BusyBlocks, GrantCreated, GrantKind, Grants, Method, PairInvite, PairedDevice, PairedDevices, RpcError, ServerInfo},
    device::DeviceProof,
    model::{BlockKind, SessionSummary, SessionView},
};

pub use silo_client::hosts;
use silo_client::hosts::{Host, Hosts, client_dir};

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

/// This process's way to hosts. Hosts pair the device key this CLI shares
/// with the app (`<home>/client`), but a relay keeps one connection per
/// endpoint id: binding that key would knock the app (or another CLI run)
/// off it. So each run has an endpoint of its own, with a fresh key, that
/// speaks for the device key with a [`DeviceProof`].
pub struct Endpoint {
    pub net: Net,
    /// The device key this endpoint speaks for; `None` when bound with it.
    device: Option<Identity>,
}

fn device_identity(home: &Path) -> anyhow::Result<Identity> {
    silo_client::client_identity(&client_dir(home)).context("load client identity")
}

impl Endpoint {
    /// An endpoint of this process's own, speaking for the device key.
    ///
    /// # Errors
    /// Unreadable device key or bind failure.
    pub async fn own(home: &Path, options: &NetOptions) -> anyhow::Result<Self> {
        let device = device_identity(home)?;
        let net = Net::bind(&Identity::ephemeral(), options.relay.clone(), options.mdns).await?;
        Ok(Self { net, device: Some(device) })
    }

    /// Bound with the device key itself: to pair it, and for hosts from
    /// before device proofs (see [`Endpoint::refused`]).
    ///
    /// # Errors
    /// Unreadable device key or bind failure.
    pub async fn device(home: &Path, options: &NetOptions) -> anyhow::Result<Self> {
        let net = Net::bind(&device_identity(home)?, options.relay.clone(), options.mdns).await?;
        Ok(Self { net, device: None })
    }

    /// What a hello to `server` carries to speak for the device key.
    #[must_use]
    pub fn proof(&self, server: EndpointId) -> Option<DeviceProof> {
        self.device.as_ref().map(|device| silo_client::device_proof(device, server, self.net.id()))
    }

    pub async fn close(self) {
        let _ = self.net.close().await;
    }

    /// A host refused this endpoint of the process's own as an unknown
    /// device: it predates device proofs (silo 0.1.3 and before) and knows
    /// only the device key itself. The endpoint to repeat the attempt
    /// with, bound with that key.
    ///
    /// # Errors
    /// Binding failed.
    pub async fn refused(self, home: &Path, options: &NetOptions) -> anyhow::Result<Self> {
        self.close().await;
        Self::device(home, options).await
    }
}

/// How hosts before device proofs refuse an endpoint they hold no grant
/// for: the control hello's message, and the upgrade stream's reason.
const UNKNOWN_DEVICE: &str = "unknown device; pair it with an invite";
const NOT_PAIRED: &str = "this device is not paired with the host";

/// Whether a hello `error` is that refusal (see [`Endpoint::refused`]).
fn unknown_device(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Unauthorized>().is_some_and(|e| e.message == UNKNOWN_DEVICE)
        || error.downcast_ref::<silo_client::upgrade::Refused>().is_some_and(|r| r.code == "unauthorized" && r.reason == NOT_PAIRED)
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
    let (endpoint, result) =
        if host.get(..silo_client::transport::TICKET_PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(silo_client::transport::TICKET_PREFIX)) {
            let ticket = PairTicket::decode(host)?;
            // Pairing grants the endpoint's key: the device key itself.
            let endpoint = Endpoint::device(home, options).await?;
            let result = silo_client::pair(&endpoint.net, &ticket, client_name).await;
            (endpoint, result)
        } else {
            let host = Hosts::load(home)?.find(host)?.clone();
            let hello = async |endpoint: &Endpoint| {
                let rc = reach(&endpoint.net, &host).await?;
                match endpoint.proof(host.addr.id) {
                    Some(proof) => Control::over_remote_as(rc, client_name, false, proof).await,
                    None => Control::over_remote(rc, client_name, false, None).await,
                }
            };
            let own = Endpoint::own(home, options).await?;
            match hello(&own).await {
                Err(error) if unknown_device(&error) => {
                    let device = own.refused(home, options).await?;
                    let result = hello(&device).await;
                    (device, result)
                }
                result => (own, result),
            }
        };
    match result {
        Ok((control, events)) => {
            hosts::refresh_host(home, &control).await?;
            Ok(Remote { control, events, net: endpoint.net })
        }
        Err(error) => {
            endpoint.close().await;
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
    // Pairing grants the endpoint's key: the device key itself.
    let net = Endpoint::device(home, options).await?.net;
    let paired = match silo_client::pair(&net, &ticket, client_name).await {
        // Prefer the server's own view of its address over the ticket's.
        Ok((control, _)) => {
            let hostname = control.hello().server.hostname.clone();
            hosts::current_addr(&control).await.map(|addr| (addr.filter(|a| a.id == ticket.addr.id), hostname))
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

/// `pair.create` with `ttl`.
///
/// # Errors
/// RPC failure; [`Offline`] is explained.
pub async fn create_invite(control: &Control, ttl: Duration) -> anyhow::Result<PairInvite> {
    let method = || Method::PairCreate { ttl_secs: ttl.as_secs(), purpose: silo_proto::control::InvitePurpose::Device };
    confirmed(control, || control.call(method())).await.map_err(explain)
}

/// How long an approval-gated call runs before the CLI says it is waiting.
const APPROVAL_NOTICE: Duration = Duration::from_secs(1);

/// Run `call`, a method that needs an approval. While a Silo app is asked
/// (up to two minutes), say so. When no Silo app can approve it and the
/// server lets this terminal confirm instead (`confirm`), ask on the
/// terminal, answer, and run it once more.
///
/// # Errors
/// The call's error, or a declined confirmation.
pub async fn confirmed<T, F, Fut>(control: &Control, call: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let first = call();
    tokio::pin!(first);
    let result = tokio::select! {
        result = &mut first => result,
        () = tokio::time::sleep(APPROVAL_NOTICE) => {
            eprintln!("silo: waiting for approval in Silo…");
            first.await
        }
    };
    let error = match result {
        Err(error) => error,
        ok => return ok,
    };
    let Some(confirm) = error.downcast_ref::<RpcError>().filter(|e| e.code == RpcError::CONFIRM) else { return Err(error) };
    let data = confirm.data.clone().unwrap_or_default();
    let id = data["approval"].as_str().unwrap_or_default().to_owned();
    let summary = data["summary"].as_str().unwrap_or(&confirm.message).to_owned();
    let allow = ask_terminal(&format!("{summary}. Allow? [y/N] "));
    control.call_value(Method::ApprovalAnswer { id, allow }).await?;
    if !allow {
        bail!("cancelled");
    }
    call().await
}

/// Ask `prompt` on the controlling terminal; yes only for `y`/`yes`.
fn ask_terminal(prompt: &str) -> bool {
    use std::io::{BufRead, Write};
    let Ok(tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") else { return false };
    let mut writer = &tty;
    if writer.write_all(format!("silo: {prompt}").as_bytes()).and_then(|()| writer.flush()).is_err() {
        return false;
    }
    let mut answer = String::new();
    if std::io::BufReader::new(&tty).read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes")
}

/// The methods that refuse to end running programs without `force`
/// (Phase 7 §7.5).
pub const CLOSES: [&str; 5] = ["block.close", "window.close", "session.close", "server.shutdown", "server.restart"];

/// Run a close, `call(force)`. When it is refused as `busy`: with stdin a
/// terminal, list what runs and ask on the controlling terminal (never
/// into redirected output) whether to close anyway (default no), then run
/// `call(true)`; otherwise fail naming `--force`.
///
/// # Errors
/// The call's error, a `busy` refusal without a terminal, or a declined
/// confirmation.
pub async fn guarded<T, F, Fut>(force: bool, call: F) -> anyhow::Result<T>
where
    F: Fn(bool) -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    use std::io::IsTerminal;
    let error = match call(force).await {
        Err(error) => error,
        ok => return ok,
    };
    let Some(refusal) = error.downcast_ref::<RpcError>().filter(|e| e.code == RpcError::BUSY) else { return Err(error) };
    let busy: Vec<BusyBlock> = refusal.data.clone().and_then(|d| serde_json::from_value::<BusyBlocks>(d).ok()).map(|b| b.busy).unwrap_or_default();
    let them = if busy.len() > 1 { "them" } else { "it" };
    if !std::io::stdin().is_terminal() {
        bail!("{}; pass --force to close {them} anyway", refusal.message);
    }
    if !ask_terminal(&format!("{}Close anyway? [y/N] ", busy_list(&refusal.message, &busy))) {
        bail!("not closed");
    }
    call(true).await
}

/// The lines a `busy` refusal is shown as before asking.
fn busy_list(message: &str, busy: &[BusyBlock]) -> String {
    let mut out = format!("{message}\n");
    let sessions: std::collections::HashSet<&str> = busy.iter().map(|b| b.session_id.as_str()).collect();
    // One block without a command line: the message says it all.
    if let [only] = busy
        && only.busy.command.is_none()
    {
        return out;
    }
    for b in busy {
        out.push_str(&format!("  {} in '{}'", b.busy.process, b.block_title));
        if sessions.len() > 1 {
            out.push_str(&format!(" ({})", b.session_name));
        }
        if let Some(command) = &b.busy.command {
            out.push_str(&format!(": {command}"));
        }
        out.push('\n');
    }
    out
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
    confirmed(control, || control.call_value(Method::PairRevoke { id: device.id.clone() })).await?;
    Ok(device)
}

/// `grant.create` of an agent token.
///
/// # Errors
/// RPC failure or a declined confirmation.
pub async fn create_token(
    control: &Control,
    scope: Scope,
    rights: Rights,
    ttl: Option<Duration>,
    label: Option<String>,
) -> anyhow::Result<GrantCreated> {
    let method =
        || Method::GrantCreate { scope: scope.clone(), rights, kind: GrantKind::Token, ttl_secs: ttl.map(|t| t.as_secs()), label: label.clone() };
    confirmed(control, || control.call(method())).await
}

/// # Errors
/// RPC failure.
pub async fn grants(control: &Control) -> anyhow::Result<Vec<Grant>> {
    let grants: Grants = control.call(Method::GrantList {}).await?;
    Ok(grants.grants)
}

/// Revoke the grant whose id is or starts with `key`.
///
/// # Errors
/// No match, an ambiguous prefix, RPC failure or a declined confirmation.
pub async fn revoke_grant(control: &Control, key: &str) -> anyhow::Result<Grant> {
    let grants = grants(control).await?;
    let mut matches = grants.iter().filter(|g| g.id.starts_with(key));
    let grant = match (matches.next(), matches.next()) {
        (Some(grant), None) => grant.clone(),
        (Some(_), Some(_)) => bail!("{key:?} matches several grants"),
        _ => bail!("no grant {key:?}"),
    };
    confirmed(control, || control.call_value(Method::GrantRevoke { id: grant.id.clone() })).await?;
    Ok(grant)
}

/// One line per grant, as `silo grants` prints them.
#[must_use]
pub fn format_grants(grants: &[Grant]) -> String {
    let mut out = String::new();
    for g in grants {
        let who = match &g.who {
            Who::Device { .. } => "device",
            Who::Secret { kind: silo_proto::access::SecretKind::Token, .. } => "token",
            Who::Secret { kind: silo_proto::access::SecretKind::Link, .. } => "link",
            Who::Account { .. } => "account",
            _ => "unknown",
        };
        let rights = serde_json::to_value(g.rights).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        let scope = match &g.scope {
            Scope::Server => "server".to_owned(),
            Scope::Session { session_id } => format!("session {session_id}"),
            Scope::Block { block_id } => format!("block {block_id}"),
            Scope::Unknown => "unknown".to_owned(),
        };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let expires = match g.expires_at {
            Some(at) => format!(", expires in {}", until(at.saturating_sub(now))),
            None => String::new(),
        };
        let label = if g.label.is_empty() { String::new() } else { format!("  {}", g.label) };
        out.push_str(&format!("{}  {who}  {rights} on {scope}{label}{expires}\n", g.id));
    }
    out
}

fn until(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
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
    fn only_an_unknown_device_is_retried_with_the_device_key() {
        let unauthorized = |message: &str| anyhow::Error::from(Unauthorized { message: message.into() });
        let refused = |code: &str, reason: &str| anyhow::Error::from(silo_client::upgrade::Refused { code: code.into(), reason: reason.into() });
        assert!(unknown_device(&unauthorized(UNKNOWN_DEVICE).context("hello")));
        assert!(unknown_device(&refused("unauthorized", NOT_PAIRED)));
        assert!(!unknown_device(&unauthorized("the device proof does not verify")));
        assert!(!unknown_device(&unauthorized("invalid, expired or already used invite")));
        assert!(!unknown_device(&refused("unauthorized", "invalid signature length")));
        assert!(!unknown_device(&refused("busy", NOT_PAIRED)));
        assert!(!unknown_device(&anyhow::anyhow!(UNKNOWN_DEVICE)));
    }

    #[test]
    fn idle_default_session_is_not_worth_asking() {
        let summary = |windows: usize, attached_clients: usize| SessionSummary {
            id: "s1".into(),
            name: "halka-pebble".into(),
            windows,
            blocks: 1,
            attached_clients,
            created_at: 0,
            attention: Default::default(),
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

    /// Only another protocol or build reads as one; a server's own failure
    /// is reported as it is.
    #[test]
    fn explained_errors() {
        let mismatch = explain(IncompatibleProtocol { message: "protocol 2".into() }.into()).to_string();
        assert!(mismatch.contains("different silo build"), "{mismatch}");
        let internal = explain(silo_proto::control::RpcError::internal("io error: No such file or directory (os error 2)").into()).to_string();
        assert_eq!(internal, "internal: io error: No such file or directory (os error 2)");
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
}
