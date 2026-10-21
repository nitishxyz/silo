//! Approvals (Phase 7 §9): owner actions marked `approval` in the need
//! table, and upgrade streams, wait for a human when the caller is not a
//! Silo app. Approvers are the owner's apps on this host and, for a call
//! from a paired device, that device's own app connected here; with none
//! connected, a caller outside every pane of this server that has a
//! terminal may confirm on it instead (`confirm`), else the call fails.
//!
//! A guardrail, not a boundary: any same-uid process can claim to be an
//! app.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use silo_proto::{
    access::{Grant, Rights, Scope, SecretKind, Who},
    control::{Caller, ClientRole, Event, GrantKind, InvitePurpose, Method, RpcError},
    model::{BlockId, ClientId, Lifecycle},
};
use tokio::sync::oneshot;

use crate::{
    Server,
    access::{On, rights_on},
    grants::new_id,
    registry::Registry,
};

/// How long a call waits for an approver.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a `confirm` stays answerable and, once allowed, usable.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

struct Pending {
    approvers: Vec<ClientId>,
    reply: oneshot::Sender<bool>,
}

/// A confirmation the caller answers itself on its terminal.
struct Confirm {
    client: ClientId,
    action: String,
    expires: Instant,
    allowed: bool,
}

#[derive(Default)]
pub struct Approvals {
    pending: Mutex<HashMap<String, Pending>>,
    confirms: Mutex<HashMap<String, Confirm>>,
}

impl Approvals {
    /// Answer approval `id` as `client`.
    ///
    /// # Errors
    /// `not_found` for an id `client` may not answer.
    pub fn answer(&self, client: ClientId, id: &str, allow: bool) -> Result<(), RpcError> {
        let mut pending = self.pending.lock().unwrap();
        if pending.get(id).is_some_and(|p| p.approvers.contains(&client)) {
            let p = pending.remove(id).expect("present");
            let _ = p.reply.send(allow);
            return Ok(());
        }
        drop(pending);
        let mut confirms = self.confirms.lock().unwrap();
        confirms.retain(|_, c| c.expires > Instant::now());
        match confirms.get_mut(id) {
            Some(confirm) if confirm.client == client && !confirm.allowed => {
                if allow {
                    confirm.allowed = true;
                    confirm.expires = Instant::now() + CONFIRM_TIMEOUT;
                } else {
                    confirms.remove(id);
                }
                Ok(())
            }
            _ => Err(RpcError::not_found(format!("approval {id}"))),
        }
    }

    /// Consume an allowed confirmation of `action` by `client`.
    fn take_confirmed(&self, client: ClientId, action: &str) -> bool {
        let mut confirms = self.confirms.lock().unwrap();
        confirms.retain(|_, c| c.expires > Instant::now());
        let id = confirms.iter().find(|(_, c)| c.client == client && c.action == action && c.allowed).map(|(id, _)| id.clone());
        id.is_some_and(|id| confirms.remove(&id).is_some())
    }

    fn confirm(&self, client: ClientId, action: &str) -> String {
        let id = new_id();
        let confirm = Confirm { client, action: action.to_owned(), expires: Instant::now() + CONFIRM_TIMEOUT, allowed: false };
        self.confirms.lock().unwrap().insert(id.clone(), confirm);
        id
    }
}

/// An approval shown to its approvers.
struct Asked {
    id: String,
    approvers: Vec<ClientId>,
    answer: oneshot::Receiver<bool>,
    deadline: Instant,
}

/// Who answers an approval (see [`Registry::approvers`]).
#[derive(Default)]
struct Approvers {
    /// The owner's apps on this host: every card.
    local: Vec<ClientId>,
    /// The calling device's own apps, connected with the device key:
    /// only that device's cards.
    device: Vec<ClientId>,
}

impl Approvers {
    fn is_empty(&self) -> bool {
        self.local.is_empty() && self.device.is_empty()
    }
}

/// The approvers of a call from the paired device `device` (`None`: not a
/// device). A device's remote app counts only on a connection bound with
/// the device key itself: an endpoint merely speaking for it (each CLI
/// run) never approves.
fn approvers(server: &Server, device: Option<&Grant>) -> Approvers {
    let (local, remote) = server.registry.lock().unwrap().approvers(device.map(|g| g.id.as_str()));
    let key = device.and_then(|g| match &g.who {
        Who::Device { id } => Some(id.as_str()),
        _ => None,
    });
    let device = match key {
        Some(key) => {
            let auth = server.auth();
            remote.into_iter().filter(|(_, conn)| auth.endpoint(*conn) == Some(key)).map(|(id, _)| id).collect()
        }
        None => Vec::new(),
    };
    Approvers { local, device }
}

/// How a card on the calling device's own app names that device for an
/// app that does not read [`Caller::this_device`].
const THIS_DEVICE: &str = "this device";

/// Whether `method` from the paired device `device` reaches past what the
/// device itself holds or acts on another principal, so only the host's
/// owner may approve it (never the device's own app): revoking anything
/// but the device itself or a share link (another device, an agent
/// token), creating a grant beyond the device's own rights or scope, and
/// changing the host owner's app (`app.hosts_*`).
fn host_owner_only(method: &Method, device: &Grant, auth: &crate::auth::Auth) -> bool {
    let own_key = match &device.who {
        Who::Device { id } => id.as_str(),
        _ => return true,
    };
    match method {
        Method::PairRevoke { id } => id != own_key,
        Method::GrantRevoke { id } => id != &device.id && auth.get(id).is_none_or(|g| !matches!(g.who, Who::Secret { kind: SecretKind::Link, .. })),
        Method::GrantCreate { scope, rights, .. } => *rights > device.rights || (device.scope != Scope::Server && *scope != device.scope),
        Method::AppHostsAdd { .. } | Method::AppHostsRemove { .. } => true,
        _ => false,
    }
}

/// Whether the paired device's grant `grant` still stands after a wait.
fn still_paired(server: &Server, grant: &Grant) -> Result<(), RpcError> {
    match server.auth().get(&grant.id) {
        Some(g) if !g.expired(crate::grants::unix_secs()) => Ok(()),
        _ => Err(RpcError::denied("this device was removed from the host")),
    }
}

/// Show `approvers` the request to allow `action` for `caller`: the
/// calling device's own apps see the call as from [`THIS_DEVICE`], marked
/// [`Caller::this_device`].
fn start(server: &Server, approvers: Approvers, action: &str, summary: String, caller: Caller, kind: Option<&str>) -> Asked {
    let id = new_id();
    let (reply, answer) = oneshot::channel();
    let all: Vec<ClientId> = approvers.local.iter().chain(&approvers.device).copied().collect();
    server.approvals.pending.lock().unwrap().insert(id.clone(), Pending { approvers: all.clone(), reply });
    let requested = |caller: Caller| Event::ApprovalRequested {
        id: id.clone(),
        action: action.to_owned(),
        summary: summary.clone(),
        caller,
        expires_in_ms: APPROVAL_TIMEOUT.as_millis() as u64,
    };
    let own = Caller { from: Some(device_from(THIS_DEVICE, kind)), this_device: true, ..caller.clone() };
    let mut reg = server.registry.lock().unwrap();
    if !approvers.local.is_empty() {
        reg.emit_clients(requested(caller), approvers.local);
    }
    if !approvers.device.is_empty() {
        reg.emit_clients(requested(own), approvers.device);
    }
    drop(reg);
    Asked { id, approvers: all, answer, deadline: Instant::now() + APPROVAL_TIMEOUT }
}

/// Close `asked` as answered `allowed` and dismiss its cards.
fn settle(server: &Server, asked: &Asked, allowed: bool) {
    server.approvals.pending.lock().unwrap().remove(&asked.id);
    server.registry.lock().unwrap().emit_clients(Event::ApprovalResolved { id: asked.id.clone(), allowed }, asked.approvers.clone());
}

/// Wait for the answer to `asked`.
///
/// # Errors
/// `forbidden` "Denied in Silo" or `timeout`.
async fn finish(server: &Server, mut asked: Asked) -> Result<(), RpcError> {
    let wait = asked.deadline.saturating_duration_since(Instant::now());
    let answer = tokio::time::timeout(wait, &mut asked.answer).await;
    let allowed = matches!(answer, Ok(Ok(true)));
    settle(server, &asked, allowed);
    match answer {
        Ok(Ok(true)) => Ok(()),
        Ok(_) => Err(RpcError::denied("Denied in Silo")),
        Err(_) => Err(RpcError::new(RpcError::TIMEOUT, "Nobody answered the approval in Silo")),
    }
}

/// Ask `approvers` to allow `action` for `caller` and wait.
///
/// # Errors
/// See [`finish`].
async fn ask(server: &Server, approvers: Approvers, action: &str, summary: String, caller: Caller, kind: Option<&str>) -> Result<(), RpcError> {
    let asked = start(server, approvers, action, summary, caller, kind);
    finish(server, asked).await
}

/// What kind of client a remote caller is, for "from <device> (<kind>)".
fn client_kind(role: ClientRole) -> &'static str {
    match role {
        ClientRole::App => "Silo app",
        _ => "CLI",
    }
}

/// "<device> (<kind>)": where a paired device's call comes from.
fn device_from(device: &str, kind: Option<&str>) -> String {
    match kind {
        Some(kind) => format!("{device} ({kind})"),
        None => device.to_owned(),
    }
}

/// A paired device's name, as written on a card.
fn device_name(grant: &Grant) -> String {
    let name = silo_proto::text::untrusted(&grant.label, silo_proto::text::MAX_TITLE);
    if name.trim().is_empty() { "a paired device".to_owned() } else { name }
}

/// Who `client` is, for an approval card: from its pane on this host, or
/// from the paired device it calls from ("mac (CLI)"), never another
/// machine's panes.
pub(crate) fn caller(reg: &Registry, client: ClientId) -> Caller {
    let entry = reg.clients.get(&client);
    let device = entry.filter(|c| c.origin.is_some()).and_then(|c| Some((c.principal.device()?, c.role)));
    let from = match device {
        Some((grant, role)) => Some(device_from(&device_name(grant), Some(client_kind(role)))),
        None => entry.and_then(|c| c.pane.as_deref()).and_then(|pane| pane_label(reg, pane)),
    };
    Caller { client_id: client, label: entry.map(|c| c.label.clone()).unwrap_or_default(), from, this_device: false }
}

/// `session › block title` of block `pane`.
fn pane_label(reg: &Registry, pane: &str) -> Option<String> {
    let session = reg.block_session(pane)?;
    let name = reg.sessions.iter().find(|s| s.id == session).map(|s| s.name.clone())?;
    let title = reg.block_info(pane).map(|b| silo_proto::text::untrusted(&b.title, silo_proto::text::MAX_TITLE)).unwrap_or_default();
    Some(format!("{name} › {title}"))
}

/// Gate an owner call marked for approval by `client`.
///
/// `rights` is what the call needs. A paired device holding less (control)
/// passed `access::check` only to be approved here: by the owner's apps on
/// this host or by that device's own app, never without approval. What
/// [`host_owner_only`] names is asked of the owner's apps alone, and an
/// approved device call fails if the device was removed meanwhile.
///
/// # Errors
/// Denied, timed out, `confirm` (the caller may confirm on its terminal
/// and repeat the call) or `forbidden` with no Silo app to approve.
pub async fn gate(server: &Server, client: ClientId, method: &Method, rights: Rights) -> Result<(), RpcError> {
    let action = silo_proto::api::method_name(method);
    let (device, role, caller, alone, short) = {
        let reg = server.registry.lock().unwrap();
        let Some(entry) = reg.clients.get(&client) else { return Err(RpcError::forbidden(&action)) };
        let short = rights_on(&entry.principal, On::Server).is_none_or(|held| held < rights);
        if !server.require_approval {
            return if short { Err(RpcError::forbidden(&action)) } else { Ok(()) };
        }
        // The Silo app's own calls are the user's; anything else that says
        // it is an app (a token, a remote, a process in a pane) is asked.
        if crate::app_bridge::owner_app(entry) {
            return Ok(());
        }
        let device = entry.origin.and(entry.principal.device().cloned());
        let alone = entry.pane.is_none() && entry.tty && entry.origin.is_none();
        (device, entry.role, caller(&reg, client), alone, short)
    };
    if short && device.is_none() {
        return Err(RpcError::forbidden(&action));
    }
    if server.approvals.take_confirmed(client, &action) {
        return Ok(());
    }
    let (summary, host_only) = {
        let auth = server.auth();
        (summary(method, &auth.devices()), device.as_ref().is_some_and(|g| host_owner_only(method, g, &auth)))
    };
    let mut approvers = approvers(server, device.as_ref());
    if host_only {
        approvers.device.clear();
        if approvers.is_empty() {
            return Err(RpcError::denied("Approve in Silo on the host: only its owner may allow this"));
        }
    }
    if !approvers.is_empty() {
        ask(server, approvers, &action, summary, caller, Some(client_kind(role))).await?;
        return device.as_ref().map_or(Ok(()), |g| still_paired(server, g));
    }
    if alone {
        let id = server.approvals.confirm(client, &action);
        return Err(RpcError::new(RpcError::CONFIRM, summary.clone()).with_data(serde_json::json!({ "approval": id, "summary": summary })));
    }
    Err(RpcError::denied("Approve in Silo: no Silo app is connected"))
}

fn human(secs: u64) -> String {
    match secs {
        0..120 => format!("{secs} s"),
        120..7200 => format!("{} min", secs / 60),
        7200..172_800 => format!("{} h", secs / 3600),
        _ => format!("{} days", secs / 86_400),
    }
}

fn scope_text(scope: &Scope) -> String {
    match scope {
        Scope::Server => "the whole server".into(),
        Scope::Session { session_id } => format!("session {session_id}"),
        Scope::Block { block_id } => format!("block {block_id}"),
        Scope::Unknown => "an unknown scope".into(),
    }
}

/// What allowing `method` does, in words.
fn summary(method: &Method, devices: &[silo_proto::control::PairedDevice]) -> String {
    match method {
        Method::ServerShutdown { .. } => "Stop the server and end every session".into(),
        Method::ServerRestart { .. } => "Restart the server and end every session".into(),
        Method::PairCreate { ttl_secs, purpose: InvitePurpose::Device } => format!("Create a device invite valid for {}", human(*ttl_secs)),
        Method::PairCreate { ttl_secs, purpose: InvitePurpose::Join } => format!("Create a join invite valid for {}", human(*ttl_secs)),
        Method::PairRevoke { id } => {
            let name = devices.iter().find(|d| &d.id == id).map_or_else(|| id.chars().take(8).collect(), |d| d.name.clone());
            format!("Remove the paired device {name}")
        }
        Method::GrantCreate { scope, rights, kind, .. } => {
            let kind = match kind {
                GrantKind::Token => "an agent token",
                GrantKind::Link => "a share link",
                GrantKind::Account => "an account grant",
            };
            let rights = serde_json::to_value(rights).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
            format!("Create {kind} with {rights} access to {}", scope_text(scope))
        }
        Method::GrantRevoke { id } => format!("Revoke grant {id}"),
        Method::AppHostsAdd { .. } => "Add a host to Silo".into(),
        Method::AppHostsRemove { host, .. } => format!("Remove the host {host} from Silo"),
        other => format!("Run {}", silo_proto::api::method_name(other)),
    }
}

/// An upgrade stream of `grant` (`None`: the owner) asks to install
/// another build. The owner's apps on this host are asked and, for a
/// paired device, that device's own app (`silo remote upgrade` beside
/// it). With no app able to approve, it goes ahead as before approvals.
///
/// # Errors
/// Denied or timed out.
pub async fn gate_upgrade(server: &Server, grant: Option<&Grant>) -> Result<(), RpcError> {
    if !server.require_approval {
        return Ok(());
    }
    let device = grant.filter(|g| matches!(g.who, Who::Device { .. }));
    let approvers = approvers(server, device);
    if approvers.is_empty() {
        return Ok(());
    }
    let caller = match grant {
        None => Caller { client_id: 0, label: "silo".into(), from: None, this_device: false },
        Some(g) if device.is_some() => {
            Caller { client_id: 0, label: "silo cli".into(), from: Some(device_from(&device_name(g), Some("CLI"))), this_device: false }
        }
        Some(g) => Caller { client_id: 0, label: g.label.clone(), from: None, this_device: false },
    };
    let kind = device.map(|_| "CLI");
    ask(server, approvers, "upgrade", "Install another build of the server".into(), caller, kind).await?;
    device.map_or(Ok(()), |g| still_paired(server, g))
}

/// Where a local client's process runs: the pane holding it, and whether
/// it has a controlling terminal of its own (not one of this server's).
///
/// A process is in a pane when its parent chain reaches a block's process,
/// or when it or any process on that chain has a block's terminal as its
/// controlling terminal. The terminal survives a double fork (reparenting
/// to init ends the chain, not the terminal); `setsid` drops it, which
/// leaves no terminal to confirm on. A program that opens a terminal of
/// its own and detaches from the pane's is not seen (a guardrail, not a
/// boundary).
pub fn origin(pid: Option<i32>, reg: &Registry) -> (Option<BlockId>, bool) {
    let Some(pid) = pid else { return (None, false) };
    // Running blocks only: an exited block's pid may belong to anyone now.
    let panes: HashMap<i32, BlockId> = reg
        .blocks
        .iter()
        .filter_map(|(id, b)| {
            let info = b.terminal()?.info.lock().ok()?;
            Some((info.pid.filter(|_| info.lifecycle == Lifecycle::Running)?, id.clone()))
        })
        .collect();
    // Each block's terminal: its process's controlling terminal.
    let ttys: HashMap<u64, BlockId> = panes.iter().filter_map(|(pid, id)| Some((process(*pid)?.tty?, id.clone()))).collect();
    origin_in(pid, &panes, &ttys, process)
}

/// [`origin`] over a process table.
fn origin_in(
    pid: i32,
    panes: &HashMap<i32, BlockId>,
    ttys: &HashMap<u64, BlockId>,
    process: impl Fn(i32) -> Option<ProcessInfo>,
) -> (Option<BlockId>, bool) {
    let own = process(pid).and_then(|p| p.tty);
    if let Some(pane) = own.and_then(|t| ttys.get(&t)) {
        return (Some(pane.clone()), false);
    }
    let mut current = pid;
    for _ in 0..64 {
        if let Some(pane) = panes.get(&current) {
            return (Some(pane.clone()), false);
        }
        let Some(info) = process(current) else { break };
        if let Some(pane) = info.tty.and_then(|t| ttys.get(&t)) {
            return (Some(pane.clone()), false);
        }
        if info.ppid <= 1 || info.ppid == current {
            break;
        }
        current = info.ppid;
    }
    (None, own.is_some())
}

#[derive(Clone, Copy, Debug)]
struct ProcessInfo {
    ppid: i32,
    /// The controlling terminal's device, if any.
    tty: Option<u64>,
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn process(pid: i32) -> Option<ProcessInfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a `proc_bsdinfo` of `size` bytes.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    let tty = (info.e_tdev != 0 && info.e_tdev != u32::MAX).then_some(u64::from(info.e_tdev));
    (n == size).then_some(ProcessInfo { ppid: info.pbi_ppid as i32, tty })
}

#[cfg(target_os = "linux")]
fn process(pid: i32) -> Option<ProcessInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid pgrp session tty_nr …`; comm may hold spaces.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let tty = fields.get(4)?.parse::<u64>().ok().filter(|t| *t != 0);
    Some(ProcessInfo { ppid: fields.get(1)?.parse().ok()?, tty })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process(_pid: i32) -> Option<ProcessInfo> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_has_a_parent() {
        let me = process(std::process::id() as i32).expect("own process info");
        assert!(me.ppid > 0);
    }

    /// pid → (ppid, tty). Block `b` runs shell 100 on terminal 7.
    fn table(rows: &[(i32, i32, Option<u64>)]) -> impl Fn(i32) -> Option<ProcessInfo> + '_ {
        move |pid| rows.iter().find(|r| r.0 == pid).map(|r| ProcessInfo { ppid: r.1, tty: r.2 })
    }

    #[test]
    fn a_pane_is_found_through_forks_but_not_a_terminal_of_its_own() {
        let panes: HashMap<i32, BlockId> = [(100, "b".to_owned())].into();
        let ttys: HashMap<u64, BlockId> = [(7, "b".to_owned())].into();
        let rows = [
            (1, 0, None),
            (50, 1, Some(3)),    // Terminal.app's shell on another terminal
            (51, 50, Some(3)),   // `silo pair` there
            (100, 1, Some(7)),   // the pane's shell
            (101, 100, Some(7)), // `silo pair` in the pane
            (102, 1, Some(7)),   // double-forked: reparented to init, same terminal
            (103, 1, None),      // setsid: no terminal at all
            (104, 102, Some(9)), // `script`: a fresh terminal, run by the double-forked process
            (105, 104, Some(9)), // `silo pair` on it
            (106, 999, Some(9)), // parent gone
        ];
        let process = table(&rows);
        let at = |pid| origin_in(pid, &panes, &ttys, &process);
        assert_eq!(at(51), (None, true), "outside every pane, with a terminal: may confirm");
        assert_eq!(at(101), (Some("b".into()), false));
        assert_eq!(at(102), (Some("b".into()), false), "a double fork keeps the pane's terminal");
        assert_eq!(at(103), (None, false), "no terminal: nothing to confirm on");
        assert_eq!(at(105), (Some("b".into()), false), "a terminal of its own, opened from the pane's");
        assert_eq!(at(106), (None, true), "not seen: a guardrail");
    }

    /// A real double fork in a real terminal: the orphan's parent chain no
    /// longer reaches the pane, its terminal still does.
    #[test]
    fn a_double_forked_process_is_still_in_its_pane() {
        use std::io::Read as _;
        let config =
            silo_pty::SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), "(sleep 30 & echo pid:$!:) ; read x".into()], ..Default::default() };
        let size = silo_pty::PtySize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let pty = silo_pty::Pty::spawn(&config, size, |_| {}).unwrap();
        let mut reader = pty.reader().unwrap();
        let (mut out, mut buf) = (String::new(), [0u8; 256]);
        let orphan = loop {
            if let Some(pid) = out.split("pid:").nth(1).and_then(|s| s.split(':').next()).and_then(|s| s.trim().parse::<i32>().ok()) {
                break pid;
            }
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => panic!("no pid in {out:?}"),
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        };
        let shell = pty.pid();
        let (shell_info, orphan_info) = (process(shell).unwrap(), process(orphan).unwrap());
        let panes: HashMap<i32, BlockId> = [(shell, "b".to_owned())].into();
        let ttys: HashMap<u64, BlockId> = [(shell_info.tty.expect("the pane's terminal"), "b".to_owned())].into();
        let reaches = |mut pid: i32| {
            for _ in 0..64 {
                if pid == shell {
                    return true;
                }
                match process(pid) {
                    Some(p) if p.ppid > 1 => pid = p.ppid,
                    _ => return false,
                }
            }
            false
        };
        assert!(!reaches(orphan), "reparented away from the shell");
        assert_eq!(orphan_info.tty, shell_info.tty);
        assert_eq!(origin_in(orphan, &panes, &ttys, process), (Some("b".into()), false));
        // SAFETY: signalling the test's own children.
        unsafe {
            libc::kill(orphan, libc::SIGTERM);
        }
        pty.signal(libc::SIGTERM);
    }

    #[test]
    fn confirmations_are_single_use_and_the_caller_s_own() {
        let approvals = Approvals::default();
        let id = approvals.confirm(7, "pair.create");
        assert!(!approvals.take_confirmed(7, "pair.create"), "not answered yet");
        assert_eq!(approvals.answer(8, &id, true).unwrap_err().code, "not_found");
        approvals.answer(7, &id, true).unwrap();
        assert!(!approvals.take_confirmed(7, "server.shutdown"));
        assert!(approvals.take_confirmed(7, "pair.create"));
        assert!(!approvals.take_confirmed(7, "pair.create"), "single use");
        let denied = approvals.confirm(7, "pair.create");
        approvals.answer(7, &denied, false).unwrap();
        assert!(!approvals.take_confirmed(7, "pair.create"));
    }
}
