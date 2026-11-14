//! `silo share`: share links to a session or a block, as the app's Share
//! sheet makes them (`grant.create` kind `link`), or sharing by name with
//! another account on this machine (`kind: account`,
//! `docs/LOGIN_SERVER.md` §7).

use std::{io::IsTerminal, time::Duration};

use anyhow::{Context, bail};
use silo_client::{
    Control,
    hosts::Hosts,
    transport::{LoginTicket, ShareTicket, valid_account},
};
use silo_proto::{
    access::{Grant, Rights, Scope, SecretKind, Who},
    control::{GrantCreated, GrantKind, Method},
};

/// What a share link is for.
#[derive(Clone, Debug, Default)]
pub struct Share {
    /// A session id or name; else `$SILO_SESSION` or `$SILO_BLOCK`'s.
    pub session: Option<String>,
    /// One block instead of a session.
    pub block: Option<String>,
    pub interact: bool,
    pub ttl: Option<Duration>,
    pub label: Option<String>,
    pub qr: bool,
    /// Share with this account (a user name) instead of making a link.
    pub account: Option<String>,
}

/// What `silo share` lists and revokes: links and account grants.
fn is_share(grant: &Grant) -> bool {
    matches!(grant.who, Who::Secret { kind: SecretKind::Link, .. } | Who::Account { .. })
}

/// The scope: `--block`, the session named, else (on this machine) where
/// this runs.
async fn scope(control: &Control, share: &Share, local: bool) -> anyhow::Result<Scope> {
    if let Some(block) = &share.block {
        return Ok(Scope::Block { block_id: block.clone() });
    }
    if let Some(session) = &share.session {
        return Ok(Scope::Session { session_id: crate::script::session_id(control, session).await? });
    }
    let env = |name: &str| local.then(|| std::env::var(name).ok()).flatten().filter(|v| !v.is_empty());
    if let Some(session) = env("SILO_SESSION") {
        return Ok(Scope::Session { session_id: session });
    }
    if let Some(block) = env("SILO_BLOCK") {
        return Ok(Scope::Session { session_id: crate::script::session_of_block(control, &block).await? });
    }
    bail!("name a session to share: silo share SESSION (`silo ls` lists them), or --block BLOCK")
}

/// Create a link and print it (and a QR code with `qr`).
///
/// # Errors
/// No session to share, RPC failure, a declined approval, or a server that
/// makes no links.
pub async fn create(control: &Control, share: &Share, local: bool) -> anyhow::Result<i32> {
    let scope = scope(control, share, local).await?;
    let rights = if share.interact { Rights::Interact } else { Rights::View };
    let method = || Method::GrantCreate {
        scope: scope.clone(),
        rights,
        kind: GrantKind::Link,
        ttl_secs: share.ttl.map(|t| t.as_secs()),
        label: share.label.clone().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()),
    };
    let created: GrantCreated = crate::confirmed(control, || control.call(method())).await.map_err(crate::explain)?;
    let ticket = created.ticket.as_deref().context("this server can't make share links")?;
    let link = ShareTicket::decode(ticket).map(|t| (t.link(), t.has_relay())).context("the server's share ticket")?;
    if share.qr {
        println!("{}", crate::qr(&link.0)?);
    }
    println!("{}", link.0);
    if !link.1 {
        eprintln!("silo: no relay: only Silo on the same network can open this link");
    }
    if std::io::stderr().is_terminal() {
        let what = if share.interact { "Watch and type" } else { "Watch only" };
        let expires = created.grant.expires_at.map(|at| format!(", expires in {}", left(at))).unwrap_or_default();
        eprintln!("{what}{expires}. Stop sharing: silo share --revoke {}", created.grant.id);
    }
    Ok(0)
}

/// Share with `share.account` by name: an account grant (`label` names
/// the account, as the server expects). On `host` (else this machine).
/// Prints what that account does next.
///
/// # Errors
/// Not a user name, this server's own account, no session to share, RPC
/// failure or a declined approval.
pub async fn create_account(control: &Control, share: &Share, host: Option<&str>) -> anyhow::Result<i32> {
    let account = share.account.as_deref().map(str::trim).unwrap_or_default();
    if !valid_account(account) {
        bail!("{account:?} is not a user name");
    }
    let (owner, ticket) = owner(host, account).await;
    if owner.as_deref() == Some(account) {
        bail!("{account} is this server's own account: its devices log in to it already");
    }
    let scope = scope(control, share, host.is_none()).await?;
    let rights = if share.interact { Rights::Interact } else { Rights::View };
    let method = || Method::GrantCreate {
        scope: scope.clone(),
        rights,
        kind: GrantKind::Account,
        ttl_secs: share.ttl.map(|t| t.as_secs()),
        label: Some(account.to_owned()),
    };
    let created: GrantCreated = crate::confirmed(control, || control.call(method())).await.map_err(crate::explain)?;
    println!("{}", next_step(account, owner.as_deref(), ticket.as_deref()));
    if std::io::stderr().is_terminal() {
        let what = if share.interact { "Watch and type" } else { "Watch only" };
        let expires = created.grant.expires_at.map_or_else(|| ", until revoked".to_owned(), |at| format!(", expires in {}", left(at)));
        eprintln!("{what}{expires}. Stop sharing: silo share --revoke {}", created.grant.id);
    }
    Ok(0)
}

/// Whose server this is and the login ticket that picks `account`, as
/// far as this process can tell: on this machine, its user and the login
/// server's ticket (its admin socket answers root and its own user only);
/// on a host added through a login server, what it was added with.
async fn owner(host: Option<&str>, account: &str) -> (Option<String>, Option<String>) {
    let Some(host) = host else {
        let request = silo_login::admin::AdminRequest::Ticket { account: Some(account.to_owned()) };
        let asked = silo_login::admin::request(std::path::Path::new(silo_login::DEFAULT_STATE_DIR), &request);
        let ticket = match tokio::time::timeout(Duration::from_secs(2), asked).await {
            Ok(Ok(reply)) => reply.get("ticket").and_then(serde_json::Value::as_str).map(str::to_owned),
            _ => None,
        };
        return (silo_server::login::own_account(), ticket);
    };
    let hosts = Hosts::load(&silo_proto::paths::home()).unwrap_or_default();
    let Some(via) = hosts.find(host).ok().and_then(|h| h.login.clone()) else { return (None, None) };
    let ticket = LoginTicket { addr: via.addr, secret: None, account: Some(account.to_owned()) }.encode();
    (via.server_of.or(via.account), Some(ticket))
}

/// The line telling `account` how to add the shared machine: with
/// `ticket` (else the admin's) and `--server-of owner`.
#[must_use]
pub fn next_step(account: &str, owner: Option<&str>, ticket: Option<&str>) -> String {
    let ticket = ticket.unwrap_or("<the login ticket from your admin>");
    let owner = owner.unwrap_or("<this server's account>");
    format!("{account} can now add this machine with: silo hosts add {ticket} --server-of {owner}")
}

/// `3h` until unix second `at`.
fn left(at: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    match at.saturating_sub(now) {
        secs @ 0..60 => format!("{secs}s"),
        secs @ 60..3600 => format!("{}m", secs / 60),
        secs @ 3600..86_400 => format!("{}h", secs / 3600),
        secs => format!("{}d", secs / 86_400),
    }
}

/// `silo share --list`: links and account grants (who, scope, rights,
/// expiry).
///
/// # Errors
/// RPC failure.
pub async fn list(control: &Control) -> anyhow::Result<i32> {
    let links: Vec<Grant> = crate::grants(control).await?.into_iter().filter(is_share).collect();
    if links.is_empty() {
        eprintln!("no share links or account grants");
    }
    print!("{}", crate::format_grants(&links));
    Ok(0)
}

/// `silo share --revoke ID`: a link or account grant by id or id prefix;
/// its viewers (the account's devices) are disconnected.
///
/// # Errors
/// No such share, an ambiguous prefix, RPC failure or a declined approval.
pub async fn revoke(control: &Control, key: &str) -> anyhow::Result<i32> {
    let links: Vec<Grant> = crate::grants(control).await?.into_iter().filter(is_share).collect();
    let mut matches = links.iter().filter(|g| g.id.starts_with(key));
    let link = match (matches.next(), matches.next()) {
        (Some(link), None) => link.clone(),
        (Some(_), Some(_)) => bail!("{key:?} matches several shares"),
        _ => bail!("no share {key:?}; `silo share --list` lists them"),
    };
    crate::confirmed(control, || control.call_value(Method::GrantRevoke { id: link.id.clone() })).await?;
    println!("revoked {}", link.id);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(who: Who) -> Grant {
        Grant {
            id: "g".into(),
            who,
            scope: Scope::Server,
            rights: Rights::View,
            label: String::new(),
            created_at: 0,
            created_by: String::new(),
            created_by_grant: None,
            expires_at: None,
            last_seen: 0,
        }
    }

    #[test]
    fn next_step_names_the_ticket_and_the_owner() {
        assert_eq!(
            next_step("ana", Some("alice"), Some("ana@silol1abc")),
            "ana can now add this machine with: silo hosts add ana@silol1abc --server-of alice"
        );
        let unknown = next_step("ana", None, None);
        assert!(unknown.contains("silo hosts add <the login ticket from your admin> --server-of <this server's account>"), "{unknown}");
    }

    #[test]
    fn links_and_account_grants_are_shares() {
        assert!(is_share(&grant(Who::Account { name: "ana".into() })));
        assert!(is_share(&grant(Who::Secret { kind: SecretKind::Link, hash: String::new() })));
        assert!(!is_share(&grant(Who::Secret { kind: SecretKind::Token, hash: String::new() })));
        assert!(!is_share(&grant(Who::Device { id: "d".into() })));
    }
}
