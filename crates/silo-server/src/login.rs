//! The account's server's side of the login server
//! (`docs/LOGIN_SERVER.md` §6): `login.admit` and `login.revoke`, callable
//! by [`Principal::Login`] only, and `grant.create` of `kind: account`.
//!
//! What `login.admit` makes is marked by `created_by` =
//! [`LOGIN_CREATOR`] (only the server sets `created_by`; `silo pair`
//! devices have it empty), so `login.revoke` never touches a paired
//! device. A device admitted as another account also carries
//! `created_by_grant` = that account grant, and goes with it
//! ([`crate::grants::Grants::remove_where`]).

use std::time::Duration;

use serde_json::Value;
use silo_proto::{
    access::{Grant, MAX_GRANT_TTL_SECS, Rights, Scope, Who},
    api::method_name,
    control::{Empty, Event, GrantCreated, Method, RpcError},
    login::{LoginAdmitParams, LoginAdmitted, LoginRevokeParams},
    model::ClientId,
};

use crate::{
    Server,
    access::Principal,
    grants::{new_id, unix_secs},
};

/// `created_by` of the grants `login.admit` makes.
pub const LOGIN_CREATOR: &str = "login server";
/// How long `login.admit` waits for the endpoint to learn an address.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(4);

/// Whether `grant` is a device grant `login.admit` made.
#[must_use]
pub fn by_login(grant: &Grant) -> bool {
    matches!(grant.who, Who::Device { .. }) && grant.created_by == LOGIN_CREATOR
}

/// Whether `grant` is a device admitted as another account, through an
/// account grant: it holds only what that grant covers.
#[must_use]
pub fn derived(grant: &Grant) -> bool {
    by_login(grant) && grant.created_by_grant.is_some()
}

/// The user name of `uid`, from the password database.
#[must_use]
pub fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    loop {
        #[allow(unsafe_code)]
        // SAFETY: `passwd` is plain data, filled by getpwuid_r below.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        #[allow(unsafe_code)]
        // SAFETY: every pointer is valid for the call; `buf` outlives the
        // strings it holds, which are read before it is dropped.
        let rc = unsafe { libc::getpwuid_r(uid, &raw mut pwd, buf.as_mut_ptr().cast(), buf.len(), &raw mut found) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || found.is_null() || pwd.pw_name.is_null() {
            return None;
        }
        #[allow(unsafe_code)]
        // SAFETY: getpwuid_r succeeded: `pw_name` is a NUL-terminated
        // string in `buf`.
        let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
        return name.to_str().ok().map(str::to_owned);
    }
}

/// This server's account: its process's user.
#[must_use]
pub fn own_account() -> Option<String> {
    user_name(rustix::process::getuid().as_raw())
}

/// Handle a `login.*` call from `client`.
///
/// # Errors
/// `forbidden` for anyone but the login server; see [`admit`], [`revoke`].
pub async fn handle(server: &Server, client: ClientId, principal: &Principal, method: Method) -> Result<Value, RpcError> {
    let _ = client;
    if *principal != Principal::Login {
        return Err(RpcError::denied(format!("{} is only for the login server", method_name(&method))));
    }
    match method {
        Method::LoginAdmit(params) => serde_json::to_value(admit(server, params).await?).map_err(RpcError::internal),
        Method::LoginRevoke(params) => {
            revoke(server, &params)?;
            serde_json::to_value(Empty {}).map_err(RpcError::internal)
        }
        other => Err(RpcError::method_not_found(&method_name(&other))),
    }
}

/// A device id in its canonical form.
fn device_id(text: &str) -> Result<String, RpcError> {
    text.parse::<silo_transport::EndpointId>().map(|id| id.to_string()).map_err(|_| RpcError::bad_request("device is not an endpoint id"))
}

/// This server's address for a device to dial, once it has one.
async fn address(server: &Server) -> Result<silo_transport::EndpointAddr, RpcError> {
    let net = server.net.lock().await;
    let net = net.as_ref().ok_or_else(|| RpcError::internal("remote access is not running"))?;
    if server.relays || net.addr().is_empty() {
        let _ = tokio::time::timeout(ONLINE_TIMEOUT, net.online()).await;
    }
    let addr = net.addr();
    if addr.is_empty() {
        return Err(RpcError::new("offline", "the server has no relay or direct address yet; retry"));
    }
    Ok(addr)
}

/// `login.admit`: make sure `device` holds a device grant here, as this
/// server's own account (control on the server) or as another account an
/// account grant covers (that grant's scope and rights, tied to it).
///
/// # Errors
/// A bad device or account, `forbidden` when no account grant covers
/// another account, storage failures.
pub async fn admit(server: &Server, params: LoginAdmitParams) -> Result<LoginAdmitted, RpcError> {
    let device = device_id(&params.device)?;
    if !silo_transport::valid_account(&params.account) {
        return Err(RpcError::bad_request("account is not a user name"));
    }
    let own = own_account().ok_or_else(|| RpcError::internal("this server's user has no name"))?;
    let name = silo_proto::text::untrusted(&params.name, silo_proto::text::MAX_TITLE);
    let ticket = silo_transport::encode_addr(&address(server).await?);
    let now = unix_secs();
    let (grant_id, changed) = {
        let mut auth = server.auth();
        let existing = auth.device_grant(&device);
        let covering = if params.account == own {
            None
        } else {
            let mut covering: Vec<Grant> =
                auth.grants().into_iter().filter(|g| matches!(&g.who, Who::Account { name } if *name == params.account) && !g.expired(now)).collect();
            // The widest: server scope first, then the most rights.
            covering.sort_by_key(|g| (g.scope != Scope::Server, std::cmp::Reverse(g.rights)));
            let grant = covering.into_iter().next();
            Some(grant.ok_or_else(|| RpcError::denied(format!("no grant here for the account {}", params.account)))?)
        };
        let keep = match (&existing, &covering) {
            // Its own pairing or own-account grant, or one already made
            // through this account grant, stands.
            (Some(found), None) if !derived(found) => Some(found.id.clone()),
            (Some(found), Some(by)) if !derived(found) || found.created_by_grant.as_deref() == Some(&by.id) => Some(found.id.clone()),
            _ => None,
        };
        match keep {
            Some(id) => (id, false),
            None => {
                // A grant made through another (or no longer the widest)
                // account grant is replaced.
                if let Some(old) = existing.filter(derived) {
                    auth.remove_where(|g| g.id == old.id)?;
                }
                let (scope, rights, by, expires_at) = match &covering {
                    None => (Scope::Server, Rights::Control, None, None),
                    Some(by) => (by.scope.clone(), by.rights.min(Rights::Control), Some(by.id.clone()), by.expires_at),
                };
                let grant = Grant {
                    id: new_id(),
                    who: Who::Device { id: device.clone() },
                    scope,
                    rights,
                    label: name,
                    created_at: now,
                    created_by: LOGIN_CREATOR.to_owned(),
                    created_by_grant: by,
                    expires_at,
                    last_seen: 0,
                };
                auth.add_grant(grant.clone())?;
                (grant.id, true)
            }
        }
    };
    if changed {
        let grants = server.auth().grants();
        server.registry.lock().unwrap().emit_owner(Event::GrantsChanged { grants });
    }
    tracing::info!(device, account = params.account, grant = grant_id, new = changed, "login server admitted a device");
    Ok(LoginAdmitted { ticket, grant_id })
}

/// `login.revoke`: remove what `login.admit` made for `device` (as
/// `account` only, when given), never a `silo pair` device, and close its
/// connections.
///
/// # Errors
/// A bad device, storage failures.
pub fn revoke(server: &Server, params: &LoginRevokeParams) -> Result<(), RpcError> {
    let device = device_id(&params.device)?;
    let own = own_account();
    let gone = {
        let mut auth = server.auth();
        let accounts: Vec<String> = auth
            .grants()
            .into_iter()
            .filter(|g| matches!(&g.who, Who::Account { name } if Some(name) == params.account.as_ref()))
            .map(|g| g.id)
            .collect();
        auth.remove_where(|g| {
            let this = by_login(g) && matches!(&g.who, Who::Device { id } if *id == device);
            this && match &params.account {
                None => true,
                Some(account) if Some(account) == own.as_ref() => !derived(g),
                Some(_) => g.created_by_grant.as_ref().is_some_and(|by| accounts.contains(by)),
            }
        })?
    };
    tracing::info!(device, account = ?params.account, grants = gone.len(), "login server revoked a device");
    // Closes their connections and tells the owner.
    server.revoked(&gone);
    Ok(())
}

/// `grant.create` with `kind: account`: `label` names the account (a unix
/// user name). The grant does nothing without a login server on the
/// machine: it lets that account's devices in through `login.admit`.
///
/// # Errors
/// A bad account name or ttl, owner rights, a scope not found.
pub fn grant_account(
    server: &Server,
    client: ClientId,
    principal: &Principal,
    scope: Scope,
    rights: Rights,
    ttl: Option<u64>,
    label: Option<String>,
) -> Result<Value, RpcError> {
    let account = label.as_deref().map(str::trim).unwrap_or_default().to_owned();
    if !silo_transport::valid_account(&account) {
        return Err(RpcError::bad_request("an account grant's label is the account's user name"));
    }
    if rights == Rights::Owner {
        return Err(RpcError::bad_request("account grants cannot hold owner rights"));
    }
    if ttl.is_some_and(|t| t == 0 || t > MAX_GRANT_TTL_SECS) {
        return Err(RpcError::bad_request(format!("ttl_secs must be between 1 and {MAX_GRANT_TTL_SECS}")));
    }
    let (scope, created_by) = {
        let reg = server.registry.lock().unwrap();
        let scope = match scope {
            Scope::Server => Scope::Server,
            Scope::Session { session_id } => {
                Scope::Session { session_id: reg.session_id(&session_id).ok_or_else(|| RpcError::not_found(format!("session {session_id}")))? }
            }
            Scope::Block { block_id } if reg.blocks.contains_key(&block_id) => Scope::Block { block_id },
            Scope::Block { block_id } => return Err(RpcError::not_found(format!("block {block_id}"))),
            Scope::Unknown => return Err(RpcError::bad_request("unsupported scope")),
        };
        (scope, reg.client_label(client))
    };
    let now = unix_secs();
    let grant = Grant {
        id: new_id(),
        who: Who::Account { name: account.clone() },
        scope,
        rights,
        label: account,
        created_at: now,
        created_by,
        created_by_grant: principal.grant_id().map(str::to_owned),
        expires_at: ttl.map(|t| now + t),
        last_seen: 0,
    };
    let grants = {
        let mut auth = server.auth();
        auth.add_grant(grant.clone())?;
        auth.grants()
    };
    server.registry.lock().unwrap().emit_owner(Event::GrantsChanged { grants });
    tracing::info!(grant = grant.id, "account grant created");
    serde_json::to_value(GrantCreated { grant, secret: String::new(), ticket: None }).map_err(RpcError::internal)
}
