//! `app.*` methods, forwarded to a connected Silo app (Phase 7 §4.2): the
//! request goes to one app as `app.request`, and its `app.reply` answers
//! the call.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use serde_json::{Value, json};
use silo_proto::{
    api::method_name,
    compat::feature,
    control::{AppCall, ClientRole, Event, Method, RpcError},
    model::ClientId,
};
use tokio::sync::oneshot;

use crate::{
    Server,
    access::Principal,
    grants::new_id,
    registry::{ClientEntry, Registry},
};

/// How long a forwarded request waits for the app's reply.
pub const APP_TIMEOUT: Duration = Duration::from_secs(10);

/// A browser action with no engine: no app runs the block's page.
pub const NO_BROWSER_APP: &str = "No Silo app is running this browser block: its page runs only while the Silo app is open on this Mac";

/// `browser.fetch` without an app to load the page.
pub const NO_FETCH_APP: &str = "No Silo app is running on this Mac to load pages";

/// How long a forwarded `browser.fetch` may take, queue included.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

struct Pending {
    /// The app the request went to: only it may reply.
    client: ClientId,
    reply: oneshot::Sender<Result<Value, RpcError>>,
}

/// Forwarded requests waiting for their reply.
#[derive(Default)]
pub struct Requests {
    pending: Mutex<HashMap<String, Pending>>,
}

/// Record user activity through `client` (see [`target`]).
pub fn active(reg: &mut Registry, client: ClientId) {
    if let Some(entry) = reg.clients.get_mut(&client) {
        entry.active_at = Some(std::time::Instant::now());
    }
}

/// Whether `client` is the user's own Silo app: it says `role: app`, on
/// this server's unix socket, as the user (no token), and not from inside
/// one of this server's panes. Its calls need no approval, and only it is
/// sent forwarded requests: a remote server never drives the user's local
/// app, and a token's holder or a program in a pane never sees other
/// callers' requests (or approves its own) by claiming to be an app.
pub fn owner_app(client: &ClientEntry) -> bool {
    client.role == ClientRole::App && client.origin.is_none() && client.principal == Principal::Owner && client.pane.is_none()
}

/// Whether `client` can run forwarded requests: the [`owner_app`],
/// subscribed, speaking them.
fn eligible(client: &ClientEntry) -> bool {
    owner_app(client) && client.events.is_some() && client.supports(Some(feature::APP_REQUESTS))
}

/// Whether `client` is the user's app on another machine: `role: app` over
/// a remote connection as a paired device with control at server scope
/// (the grant a tunnel runs under), never a shared link's holder.
pub fn trusted_remote_app(client: &ClientEntry) -> bool {
    let Principal::Grant(grant) = &client.principal else { return false };
    client.role == ClientRole::App
        && client.origin.is_some()
        && client.pane.is_none()
        && matches!(grant.who, silo_proto::access::Who::Device { .. })
        && grant.scope == silo_proto::access::Scope::Server
        && grant.rights >= silo_proto::access::Rights::Control
        && !grant.expired(crate::grants::unix_secs())
}

/// The app that hosts browsers no pane shows (and `browser.fetch`): a
/// subscribed local [`owner_app`] always wins over a [`trusted_remote_app`];
/// within each, the most recently active. Picked per request, so when the
/// host leaves the next one takes over.
fn browser_host(reg: &Registry) -> Option<ClientId> {
    reg.clients
        .iter()
        .filter(|(_, c)| c.events.is_some() && c.supports(Some(feature::APP_REQUESTS)) && c.supports(Some(feature::BROWSER_HOST)))
        .filter_map(|(id, c)| {
            let local = if owner_app(c) {
                true
            } else if trusted_remote_app(c) {
                false
            } else {
                return None;
            };
            Some((local, c.active_at, *id))
        })
        .max()
        .map(|(_, _, id)| id)
}

/// The app a request goes to: `explicit` if given (it must be eligible),
/// else the most recently active eligible app, ties going to the most
/// recently connected.
///
/// # Errors
/// `not_found` for an `explicit` client that is not an eligible app,
/// `no_app` when none is connected.
pub fn target(reg: &Registry, explicit: Option<ClientId>) -> Result<ClientId, RpcError> {
    if let Some(id) = explicit {
        return reg.clients.get(&id).filter(|c| eligible(c)).map(|_| id).ok_or_else(|| RpcError::not_found(format!("app client {id}")));
    }
    reg.clients
        .iter()
        .filter(|(_, c)| eligible(c))
        .max_by_key(|(id, c)| (c.active_at, **id))
        .map(|(id, _)| *id)
        .ok_or_else(|| RpcError::new(RpcError::NO_APP, "No Silo app is connected to this server"))
}

pub async fn handle(server: &Server, client_id: u64, method: Method) -> Result<Value, RpcError> {
    if let Method::BrowserFind(p) = &method
        && p.text.is_some() == p.regex.is_some()
    {
        return Err(RpcError::bad_request("exactly one of text or regex is required"));
    }
    if let Method::AppReply { request_id, result, error } = method {
        let pending = {
            let mut pending = server.app_requests.pending.lock().unwrap();
            match pending.get(&request_id) {
                Some(p) if p.client == client_id => pending.remove(&request_id),
                _ => None,
            }
        };
        let pending = pending.ok_or_else(|| RpcError::not_found(format!("request {request_id}")))?;
        let _ = pending.reply.send(error.map_or_else(|| Ok(result.unwrap_or_else(|| json!({}))), Err));
        return Ok(json!({}));
    }
    let name = method_name(&method);
    let mut params = serde_json::to_value(&method).map_err(RpcError::internal)?.get_mut("params").map(Value::take).unwrap_or_else(|| json!({}));
    // `client_id` only routes the request.
    let explicit = params.as_object_mut().and_then(|p| p.remove("client_id")).and_then(|v| v.as_u64());
    let fetch = matches!(method, Method::BrowserFetch(_));
    let browser = name.starts_with("browser.") && !fetch;
    let timeout = if fetch {
        FETCH_TIMEOUT
    } else if browser {
        params.get("timeout_ms").and_then(Value::as_u64).map_or(APP_TIMEOUT, |ms| Duration::from_millis(ms.saturating_add(5000).min(120_000)))
    } else {
        APP_TIMEOUT
    };
    let request_id = new_id();
    let (reply, answer) = oneshot::channel();
    {
        let mut reg = server.registry.lock().unwrap();
        let app = if fetch {
            browser_host(&reg).ok_or_else(|| RpcError::new(RpcError::NO_APP, NO_FETCH_APP))?
        } else if browser {
            let block_id = params["block_id"].as_str().ok_or_else(|| RpcError::bad_request("block_id is required"))?;
            browser_target(&reg, block_id)?
        } else {
            target(&reg, explicit)?
        };
        server.app_requests.pending.lock().unwrap().insert(request_id.clone(), Pending { client: app, reply });
        let caller = crate::approval::caller(&reg, client_id);
        let request = AppCall { method: name.clone(), params };
        reg.emit_clients(Event::AppRequest { request_id: request_id.clone(), request, caller }, vec![app]);
        tracing::debug!(method = name, app, "forwarded to an app");
    }
    let answered = tokio::time::timeout(timeout, answer).await;
    server.app_requests.pending.lock().unwrap().remove(&request_id);
    match answered {
        Ok(Ok(result)) => result,
        Ok(Err(_)) | Err(_) => Err(RpcError::new(RpcError::TIMEOUT, "The Silo app did not answer")),
    }
}

/// The subscribed owner executing a browser block's actions, local or
/// remote; a block nobody owns goes to the [`browser_host`].
pub fn browser_target(reg: &Registry, block_id: &str) -> Result<ClientId, RpcError> {
    let block = match reg.blocks.get(block_id) {
        Some(crate::browser::Block::Browser(block)) => block,
        Some(crate::browser::Block::Terminal(_)) => return Err(RpcError::bad_request(format!("block {block_id} is a terminal"))),
        Some(crate::browser::Block::Agent(_)) => return Err(RpcError::bad_request(format!("block {block_id} is an agent"))),
        None => return Err(RpcError::not_found(format!("block {block_id}"))),
    };
    let Some(owner) = block.info().owner else {
        return browser_host(reg).ok_or_else(|| RpcError::new(RpcError::NO_APP, NO_BROWSER_APP));
    };
    Some(owner)
        .filter(|id| {
            reg.clients.get(id).is_some_and(|c| c.role == ClientRole::App && c.events.is_some() && c.supports(Some(feature::BROWSER_ACTIONS)))
        })
        .ok_or_else(|| RpcError::new(RpcError::NO_APP, NO_BROWSER_APP))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Sink;

    /// A subscribed client of `role` on the unix socket (`origin: None`)
    /// or a remote connection.
    fn client(reg: &mut Registry, role: ClientRole, remote: bool, features: &[&str]) -> ClientId {
        let id = reg.add_client("test".into(), false);
        let entry = reg.clients.get_mut(&id).unwrap();
        entry.role = role;
        entry.origin = remote.then_some(7);
        entry.features = features.iter().map(|&f| f.to_owned()).collect();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(rx);
        entry.events = Some(Sink::new(tx));
        id
    }

    /// A grant for `who` with `rights` at server scope.
    fn grant(who: silo_proto::access::Who, rights: silo_proto::access::Rights) -> Principal {
        Principal::Grant(Box::new(silo_proto::access::Grant {
            id: "g".into(),
            who,
            scope: silo_proto::access::Scope::Server,
            rights,
            label: String::new(),
            created_at: 0,
            created_by: String::new(),
            created_by_grant: None,
            expires_at: None,
            last_seen: 0,
        }))
    }

    /// A remote app of the user's (a paired device with control) hosting browsers.
    fn remote_host(reg: &mut Registry) -> ClientId {
        let id = client(reg, ClientRole::App, true, &[feature::APP_REQUESTS, feature::BROWSER_ACTIONS, feature::BROWSER_HOST]);
        reg.clients.get_mut(&id).unwrap().principal =
            grant(silo_proto::access::Who::Device { id: "device".into() }, silo_proto::access::Rights::Control);
        id
    }

    #[test]
    fn browser_owner_may_be_remote_without_app_requests() {
        use silo_proto::{control::SpawnSpec, model::BlockKind};
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        let mut reg = Registry::new(notes);
        let spawn = SpawnSpec::default();
        let created = reg.create_session(None, &crate::registry::New { spawn: &spawn, kind: BlockKind::Browser, url: None, agent: None }).unwrap();
        let remote = client(&mut reg, ClientRole::App, true, &[feature::BROWSER_ACTIONS]);
        reg.own(remote, std::slice::from_ref(&created.block_id), Some(silo_proto::control::OwnMode::Take)).unwrap();
        assert_eq!(browser_target(&reg, &created.block_id).unwrap(), remote);
        assert_eq!(target(&reg, None).unwrap_err().code, RpcError::NO_APP);
        reg.clients.get_mut(&remote).unwrap().features.clear();
        assert_eq!(browser_target(&reg, &created.block_id).unwrap_err().code, RpcError::NO_APP);
    }

    #[test]
    fn unowned_browsers_and_fetches_go_to_the_local_browser_host() {
        use silo_proto::{control::SpawnSpec, model::BlockKind};
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        let mut reg = Registry::new(notes);
        let spawn = SpawnSpec::default();
        let created = reg.create_session(None, &crate::registry::New { spawn: &spawn, kind: BlockKind::Browser, url: None, agent: None }).unwrap();
        assert_eq!(browser_target(&reg, &created.block_id).unwrap_err().code, RpcError::NO_APP);
        let plain = client(&mut reg, ClientRole::App, false, &[feature::APP_REQUESTS, feature::BROWSER_ACTIONS]);
        assert_eq!(browser_target(&reg, &created.block_id).unwrap_err().code, RpcError::NO_APP, "an app without a host");
        assert_eq!(browser_host(&reg), None);
        let remote = client(&mut reg, ClientRole::App, true, &[feature::APP_REQUESTS, feature::BROWSER_ACTIONS, feature::BROWSER_HOST]);
        assert_eq!(browser_host(&reg), None, "a remote app that is no paired device of the owner's");
        let paired = remote_host(&mut reg);
        assert_eq!(browser_host(&reg), Some(paired), "the owner's remote app hosts until a local one connects");
        let host = client(&mut reg, ClientRole::App, false, &[feature::APP_REQUESTS, feature::BROWSER_ACTIONS, feature::BROWSER_HOST]);
        assert_eq!(browser_target(&reg, &created.block_id).unwrap(), host);
        assert_eq!(browser_host(&reg), Some(host));
        reg.clients.get_mut(&plain).unwrap().active_at = Some(std::time::Instant::now());
        assert_eq!(browser_host(&reg), Some(host), "a more active app without a host does not hide it");
        // An owned block stays with its owner.
        reg.own(remote, std::slice::from_ref(&created.block_id), Some(silo_proto::control::OwnMode::Take)).unwrap();
        assert_eq!(browser_target(&reg, &created.block_id).unwrap(), remote);
    }

    #[test]
    fn an_owner_remote_app_hosts_browsers_and_fetches() {
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        let mut reg = Registry::new(notes);
        let remote = remote_host(&mut reg);
        assert_eq!(browser_host(&reg), Some(remote));
        assert_eq!(target(&reg, None).unwrap_err().code, RpcError::NO_APP, "other app.* requests stay local-only");
        // A view-only or expired device grant hosts nothing.
        reg.clients.get_mut(&remote).unwrap().principal =
            grant(silo_proto::access::Who::Device { id: "device".into() }, silo_proto::access::Rights::View);
        assert_eq!(browser_host(&reg), None, "view only");
        if let Principal::Grant(g) = &mut reg.clients.get_mut(&remote).unwrap().principal {
            g.rights = silo_proto::access::Rights::Control;
            g.expires_at = Some(1);
        }
        assert_eq!(browser_host(&reg), None, "expired");
    }

    #[test]
    fn a_local_app_wins_over_a_more_active_remote_one() {
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        let mut reg = Registry::new(notes);
        let local = client(&mut reg, ClientRole::App, false, &[feature::APP_REQUESTS, feature::BROWSER_HOST]);
        let remote = remote_host(&mut reg);
        active(&mut reg, remote);
        assert_eq!(browser_host(&reg), Some(local));
    }

    #[test]
    fn a_shared_link_never_hosts() {
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        let mut reg = Registry::new(notes);
        let link = remote_host(&mut reg);
        let secret = silo_proto::access::Who::Secret { kind: silo_proto::access::SecretKind::Link, hash: "h".into() };
        reg.clients.get_mut(&link).unwrap().principal = grant(secret, silo_proto::access::Rights::Owner);
        assert_eq!(browser_host(&reg), None);
    }

    #[test]
    fn the_host_hands_over_when_it_leaves() {
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        let mut reg = Registry::new(notes);
        let first = remote_host(&mut reg);
        let second = remote_host(&mut reg);
        active(&mut reg, first);
        let local = client(&mut reg, ClientRole::App, false, &[feature::APP_REQUESTS, feature::BROWSER_HOST]);
        assert_eq!(browser_host(&reg), Some(local));
        reg.clients.remove(&local);
        assert_eq!(browser_host(&reg), Some(first), "the most recently active remote app");
        reg.clients.remove(&first);
        assert_eq!(browser_host(&reg), Some(second));
        reg.clients.remove(&second);
        assert_eq!(browser_host(&reg), None, "no app: plain HTTP");
    }

    #[test]
    fn requests_go_to_the_most_recently_active_local_app() {
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        let mut reg = Registry::new(notes);
        assert_eq!(target(&reg, None).unwrap_err().code, RpcError::NO_APP);
        let cli = client(&mut reg, ClientRole::Cli, false, &[feature::APP_REQUESTS]);
        let old = client(&mut reg, ClientRole::App, false, &[]);
        assert_eq!(target(&reg, None).unwrap_err().code, RpcError::NO_APP, "a CLI, or an app without the feature, is no app");

        // A remote app is never targeted, even when it is the only one,
        // the most recently active, or named.
        let remote = client(&mut reg, ClientRole::App, true, &[feature::APP_REQUESTS]);
        active(&mut reg, remote);
        assert_eq!(target(&reg, None).unwrap_err().code, RpcError::NO_APP);
        assert_eq!(target(&reg, Some(remote)).unwrap_err().code, "not_found");

        // Nor is a local app speaking for a token.
        let token = client(&mut reg, ClientRole::App, false, &[feature::APP_REQUESTS]);
        reg.clients.get_mut(&token).unwrap().principal = Principal::Grant(Box::new(silo_proto::access::Grant {
            id: "g".into(),
            who: silo_proto::access::Who::Secret { kind: silo_proto::access::SecretKind::Token, hash: "h".into() },
            scope: silo_proto::access::Scope::Server,
            rights: silo_proto::access::Rights::Control,
            label: String::new(),
            created_at: 0,
            created_by: String::new(),
            created_by_grant: None,
            expires_at: None,
            last_seen: 0,
        }));
        active(&mut reg, token);
        assert_eq!(target(&reg, None).unwrap_err().code, RpcError::NO_APP);
        assert_eq!(target(&reg, Some(token)).unwrap_err().code, "not_found");

        // Nor a program in a pane that says it is an app.
        let pane = client(&mut reg, ClientRole::App, false, &[feature::APP_REQUESTS]);
        reg.clients.get_mut(&pane).unwrap().pane = Some("b1".into());
        active(&mut reg, pane);
        assert_eq!(target(&reg, None).unwrap_err().code, RpcError::NO_APP);
        assert!(!owner_app(&reg.clients[&pane]));

        let first = client(&mut reg, ClientRole::App, false, &[feature::APP_REQUESTS]);
        let second = client(&mut reg, ClientRole::App, false, &[feature::APP_REQUESTS]);
        assert_eq!(target(&reg, None).unwrap(), second, "no activity: the most recently connected");
        active(&mut reg, first);
        assert_eq!(target(&reg, None).unwrap(), first, "the most recently active");
        std::thread::sleep(Duration::from_millis(2));
        active(&mut reg, second);
        assert_eq!(target(&reg, None).unwrap(), second);
        assert_eq!(target(&reg, Some(first)).unwrap(), first, "named");
        assert_eq!(target(&reg, Some(cli)).unwrap_err().code, "not_found");
        assert_eq!(target(&reg, Some(old)).unwrap_err().code, "not_found");
        assert_eq!(target(&reg, Some(99)).unwrap_err().code, "not_found");
    }
}
