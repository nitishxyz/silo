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
    grants::new_id,
    registry::{ClientEntry, Registry},
};

/// How long a forwarded request waits for the app's reply.
pub const APP_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Whether `client` can run forwarded requests: a subscribed app on this
/// server's unix socket that speaks them. A remote server never drives the
/// user's local app.
fn eligible(client: &ClientEntry) -> bool {
    client.role == ClientRole::App && client.origin.is_none() && client.events.is_some() && client.supports(Some(feature::APP_REQUESTS))
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
    let request_id = new_id();
    let (reply, answer) = oneshot::channel();
    {
        let mut reg = server.registry.lock().unwrap();
        let app = target(&reg, explicit)?;
        server.app_requests.pending.lock().unwrap().insert(request_id.clone(), Pending { client: app, reply });
        let caller = crate::approval::caller(&reg, client_id);
        let request = AppCall { method: name.clone(), params };
        reg.emit_clients(Event::AppRequest { request_id: request_id.clone(), request, caller }, vec![app]);
        tracing::debug!(method = name, app, "forwarded to an app");
    }
    let answered = tokio::time::timeout(APP_TIMEOUT, answer).await;
    server.app_requests.pending.lock().unwrap().remove(&request_id);
    match answered {
        Ok(Ok(result)) => result,
        Ok(Err(_)) | Err(_) => Err(RpcError::new(RpcError::TIMEOUT, "The Silo app did not answer")),
    }
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
