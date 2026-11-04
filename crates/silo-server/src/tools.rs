//! Tools through the server (`tools.*`, `docs/AGENTS.md` §10): one list
//! for every agent. Silo's own tools (`silo-tools`), each working on a kind
//! of block (`browser_*` on browsers, `terminal_*` on terminals, …), and
//! tools clients provide for a block or the whole server. A call to one of
//! Silo's own runs here, on a connection acting as the caller (its rights,
//! pane and approvals: [`StandIns`]); any other is forwarded to its provider
//! as `tools.request` and answered with `tools.reply`.
//!
//! Access to a provided tool is per tool: listing and calling need view
//! rights on the tool's block (or the server) for a read-only tool, interact
//! on its block or control on the server otherwise; a tool marked `approval`
//! also waits for a human ([`crate::approval::gate`]) unless the Silo app
//! calls it. Silo's own are listed to everyone and checked method by method
//! as they run.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::Value;
use silo_proto::{
    access::Rights,
    control::{ClientRole, Event, Method, RpcError},
    model::{BlockId, BlockKind, ClientId},
    tools::{
        CALL_TIMEOUT_MAX_MS, CALL_TIMEOUT_MS, ProvidedTool, SILO_PROVIDER, TOOLS_MAX, ToolEntry, ToolList, ToolOutcome, ToolRequest, ToolsCallParams,
    },
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    Server,
    access::{On, Principal, rights_on},
    grants::new_id,
    registry::Registry,
};

/// Who a stand-in connection acts as: the caller it runs Silo's own tools
/// for.
#[derive(Clone, Debug)]
pub struct StandIn {
    pub principal: Principal,
    pub role: ClientRole,
    pub pane: Option<BlockId>,
    pub tty: bool,
    pub label: String,
}

/// Connections the server opens to itself to run Silo's own tools for a
/// caller, admitted with a one-time secret as that caller ([`StandIn`]),
/// and the tools each caller's stand-in runs (kept while it is connected,
/// so `terminal_run` keeps its shell).
#[derive(Default)]
pub struct StandIns {
    secrets: Mutex<HashMap<String, StandIn>>,
    tools: tokio::sync::Mutex<HashMap<ClientId, Arc<silo_tools::Tools>>>,
}

impl StandIns {
    /// The caller a hello's `secret` stands in for (once).
    pub fn take(&self, secret: &str) -> Option<StandIn> {
        self.secrets.lock().unwrap().remove(secret)
    }

    /// Forget `client`'s stand-in (it disconnected).
    pub async fn client_gone(&self, client: ClientId) {
        self.tools.lock().await.remove(&client);
    }
}

/// Silo's own tools as `tools.list` entries.
fn builtin_entries() -> Vec<ToolEntry> {
    silo_tools::descriptors()
        .into_iter()
        .map(|d| ToolEntry {
            block_kind: silo_tools::block_kind(&d.name),
            tool: ProvidedTool {
                read_only: d.effects.read && !d.effects.write && !d.effects.process,
                name: d.name,
                description: d.description,
                input_schema: d.input_schema,
                approval: false,
            },
            block_id: None,
            provider: SILO_PROVIDER.into(),
            provider_id: 0,
        })
        .collect()
}

/// One tool one client provides.
#[derive(Clone, Debug)]
pub struct Provided {
    pub client: ClientId,
    pub block: Option<BlockId>,
    pub tool: ProvidedTool,
}

/// A forwarded call waiting for its provider; dropped (failing the call)
/// when the provider disconnects.
pub struct Pending {
    pub client: ClientId,
    pub reply: oneshot::Sender<Result<Value, RpcError>>,
}

/// The rights calling `tool` of `block` needs.
fn needed(tool: &ProvidedTool, block: Option<&str>) -> Rights {
    match (tool.read_only, block) {
        (true, _) => Rights::View,
        (false, Some(_)) => Rights::Interact,
        (false, None) => Rights::Control,
    }
}

/// Whether `principal` may call `entry`.
fn allowed(reg: &Registry, principal: &Principal, entry: &Provided) -> bool {
    let session = entry.block.as_deref().and_then(|b| reg.block_session(b));
    let on = match &entry.block {
        Some(block) => On::Block { block, session: session.as_deref() },
        None => On::Server,
    };
    rights_on(principal, on).is_some_and(|held| held >= needed(&entry.tool, entry.block.as_deref()))
}

impl Registry {
    /// Withdraw every tool `client` provides and fail its pending calls.
    pub(crate) fn tools_client_gone(&mut self, client: ClientId) {
        let before = self.provided.len();
        self.provided.retain(|p| p.client != client);
        self.tool_calls.retain(|_, p| p.client != client);
        if self.provided.len() != before {
            self.emit(Event::ToolsChanged {});
        }
    }

    /// Withdraw the tools of closed block `block`.
    pub(crate) fn tools_block_gone(&mut self, block: &str) {
        let before = self.provided.len();
        self.provided.retain(|p| p.block.as_deref() != Some(block));
        if self.provided.len() != before {
            self.emit(Event::ToolsChanged {});
        }
    }
}

/// `tools.*` for `client`.
///
/// # Errors
/// See each method in `docs/AGENTS.md` §10.
pub async fn handle(server: &Server, client: ClientId, principal: &Principal, method: Method) -> Result<Value, RpcError> {
    match method {
        Method::ToolsProvide(params) => {
            provide(&mut server.registry.lock().unwrap(), client, principal, params.block_id, params.tools)?;
            Ok(serde_json::json!({}))
        }
        Method::ToolsList {} => Ok(serde_json::to_value(list(&server.registry.lock().unwrap(), principal)).map_err(RpcError::internal)?),
        Method::ToolsCall(params) => call(server, client, principal, params).await,
        Method::ToolsReply(params) => {
            let pending = {
                let mut reg = server.registry.lock().unwrap();
                match reg.tool_calls.get(&params.request_id) {
                    Some(p) if p.client == client => reg.tool_calls.remove(&params.request_id),
                    _ => None,
                }
            };
            let pending = pending.ok_or_else(|| RpcError::not_found(format!("request {}", params.request_id)))?;
            let _ = pending.reply.send(params.error.map_or_else(|| Ok(params.result.unwrap_or(Value::Null)), Err));
            Ok(serde_json::json!({}))
        }
        other => Err(RpcError::internal(format!("not a tools method: {}", silo_proto::api::method_name(&other)))),
    }
}

/// Replace what `client` provides for `block` with `tools`.
///
/// # Errors
/// `bad_request` for an invalid or duplicate tool, or one another client
/// provides for the same scope; `not_found`/`forbidden` without interact on
/// the block (control on the server without one).
pub(crate) fn provide(
    reg: &mut Registry,
    client: ClientId,
    principal: &Principal,
    block: Option<BlockId>,
    tools: Vec<ProvidedTool>,
) -> Result<(), RpcError> {
    match &block {
        Some(id) => {
            let session = reg.block_session(id);
            if session.is_none() && !reg.blocks.contains_key(id) {
                return Err(RpcError::not_found(format!("block {id}")));
            }
            match rights_on(principal, On::Block { block: id, session: session.as_deref() }) {
                Some(held) if held >= Rights::Interact => {}
                Some(_) => return Err(RpcError::forbidden("tools.provide")),
                None => return Err(RpcError::not_found(format!("block {id}"))),
            }
        }
        None => {
            if rights_on(principal, On::Server).is_none_or(|held| held < Rights::Control) {
                return Err(RpcError::forbidden("tools.provide"));
            }
        }
    }
    if tools.len() > TOOLS_MAX {
        return Err(RpcError::bad_request(format!("at most {TOOLS_MAX} tools at once")));
    }
    for (i, tool) in tools.iter().enumerate() {
        tool.validate().map_err(RpcError::bad_request)?;
        if silo_tools::is_builtin(&tool.name) {
            return Err(RpcError::bad_request(format!("tool {} is one of Silo's own", tool.name)));
        }
        if tools[..i].iter().any(|t| t.name == tool.name) {
            return Err(RpcError::bad_request(format!("tool {} is listed twice", tool.name)));
        }
        if let Some(other) = reg.provided.iter().find(|p| p.client != client && p.block == block && p.tool.name == tool.name) {
            let label = reg.client_label(other.client);
            return Err(RpcError::bad_request(format!("tool {} is already provided here by {label}", tool.name)));
        }
    }
    let before: Vec<&ProvidedTool> = reg.provided.iter().filter(|p| p.client == client && p.block == block).map(|p| &p.tool).collect();
    let same = before.len() == tools.len() && before.iter().zip(&tools).all(|(a, b)| *a == b);
    if same {
        return Ok(());
    }
    reg.provided.retain(|p| !(p.client == client && p.block == block));
    reg.provided.extend(tools.into_iter().map(|tool| Provided { client, block: block.clone(), tool }));
    reg.emit(Event::ToolsChanged {});
    Ok(())
}

/// Silo's own tools, then every provided tool `principal` may call, by
/// name then block.
pub(crate) fn list(reg: &Registry, principal: &Principal) -> ToolList {
    let kind = |block: &str| reg.blocks.get(block).map(crate::browser::Block::kind).filter(|k| *k != BlockKind::Unknown);
    let mut provided: Vec<ToolEntry> = reg
        .provided
        .iter()
        .filter(|p| allowed(reg, principal, p))
        .map(|p| ToolEntry {
            tool: p.tool.clone(),
            block_id: p.block.clone(),
            block_kind: p.block.as_deref().and_then(kind),
            provider: reg.client_label(p.client),
            provider_id: p.client,
        })
        .collect();
    provided.sort_by(|a, b| (&a.tool.name, &a.block_id).cmp(&(&b.tool.name, &b.block_id)));
    let mut tools = builtin_entries();
    tools.extend(provided);
    ToolList { tools }
}

/// The entry a call means: `block`'s tool `name`, else the server's; with
/// no block, the only one so named.
fn resolve(reg: &Registry, principal: &Principal, name: &str, block: Option<&str>) -> Result<Provided, RpcError> {
    let named: Vec<&Provided> = reg.provided.iter().filter(|p| p.tool.name == name && allowed(reg, principal, p)).collect();
    let found = match block {
        Some(block) => named.iter().find(|p| p.block.as_deref() == Some(block)).or_else(|| named.iter().find(|p| p.block.is_none())),
        None if named.len() > 1 => {
            let blocks: Vec<&str> = named.iter().filter_map(|p| p.block.as_deref()).collect();
            return Err(RpcError::bad_request(format!("several blocks provide {name}: give block_id ({})", blocks.join(", "))));
        }
        None => named.first(),
    };
    found.map(|p| (*p).clone()).ok_or_else(|| RpcError::not_found(format!("tool {name}")))
}

async fn call(server: &Server, client: ClientId, principal: &Principal, params: ToolsCallParams) -> Result<Value, RpcError> {
    if !params.input.is_object() {
        return Err(RpcError::bad_request("input must be an object"));
    }
    if silo_tools::is_builtin(&params.name) {
        return builtin(server, client, params).await;
    }
    let entry = resolve(&server.registry.lock().unwrap(), principal, &params.name, params.block_id.as_deref())?;
    if entry.tool.approval {
        let method = Method::ToolsCall(params.clone());
        crate::approval::gate(server, client, &method, Rights::Control).await?;
    }
    let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(CALL_TIMEOUT_MS).clamp(1, CALL_TIMEOUT_MAX_MS));
    let request_id = new_id();
    let (reply, answer) = oneshot::channel();
    {
        let mut reg = server.registry.lock().unwrap();
        // The provider may have left during the approval.
        if !reg.provided.iter().any(|p| p.client == entry.client && p.block == entry.block && p.tool.name == entry.tool.name) {
            return Err(RpcError::not_found(format!("tool {}", entry.tool.name)));
        }
        reg.tool_calls.insert(request_id.clone(), Pending { client: entry.client, reply });
        let caller = crate::approval::caller(&reg, client);
        let request =
            ToolRequest { request_id: request_id.clone(), name: entry.tool.name.clone(), block_id: entry.block.clone(), input: params.input, caller };
        reg.emit_clients(Event::ToolRequest(request), vec![entry.client]);
        tracing::debug!(tool = entry.tool.name, provider = entry.client, "tool call forwarded");
    }
    let answered = tokio::time::timeout(timeout, answer).await;
    server.registry.lock().unwrap().tool_calls.remove(&request_id);
    let label = || server.registry.lock().unwrap().client_label(entry.client);
    match answered {
        Ok(Ok(Ok(value))) => serde_json::to_value(ToolOutcome::from_value(value)).map_err(RpcError::internal),
        Ok(Ok(Err(error))) => Err(error),
        Ok(Err(_)) => Err(RpcError::new(RpcError::NO_APP, format!("{} stopped providing {}", label(), entry.tool.name))),
        Err(_) => Err(RpcError::new(RpcError::TIMEOUT, format!("{} did not answer {} in time", label(), entry.tool.name))),
    }
}

/// Run Silo's own tool for `client` on its stand-in connection, opened on
/// first use: new blocks open beside the caller's pane, if it has one.
async fn builtin(server: &Server, client: ClientId, params: ToolsCallParams) -> Result<Value, RpcError> {
    let tools = {
        let mut cache = server.stand_ins.tools.lock().await;
        match cache.get(&client).filter(|t| !t.is_closed()) {
            Some(tools) => tools.clone(),
            None => {
                let stand_in = {
                    let reg = server.registry.lock().unwrap();
                    let entry = reg.clients.get(&client).ok_or_else(|| RpcError::not_found("client"))?;
                    StandIn {
                        principal: entry.principal.clone(),
                        role: entry.role,
                        pane: entry.pane.clone(),
                        tty: entry.tty,
                        label: entry.label.clone(),
                    }
                };
                let pane = stand_in.pane.clone();
                let secret = format!("standin-{}{}", new_id(), new_id());
                server.stand_ins.secrets.lock().unwrap().insert(secret.clone(), stand_in);
                let socket = silo_proto::paths::socket(&server.home);
                let connected = silo_client::Control::connect_with_secret(&socket, "silo tools", secret.clone()).await;
                server.stand_ins.secrets.lock().unwrap().remove(&secret);
                let (control, events) = connected.map_err(RpcError::internal)?;
                let cx = silo_tools::Context { agent_block: pane, ..silo_tools::Context::default() };
                let tools = Arc::new(silo_tools::Tools::new(control, events, cx));
                cache.insert(client, tools.clone());
                tools
            }
        }
    };
    let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(CALL_TIMEOUT_MS).clamp(1, CALL_TIMEOUT_MAX_MS));
    let cancel = CancellationToken::new();
    let call = tools.call(&params.name, params.input, cancel.clone());
    match tokio::time::timeout(timeout, call).await {
        Ok(Ok(result)) => serde_json::to_value(ToolOutcome::from(result)).map_err(RpcError::internal),
        // A refused inner call keeps its code (forbidden, not_found, no_app, …).
        Ok(Err(error)) => Err(match error.downcast::<RpcError>() {
            Ok(rpc) => rpc,
            Err(other) => RpcError::bad_request(format!("{other:#}")),
        }),
        Err(_) => {
            cancel.cancel();
            Err(RpcError::new(RpcError::TIMEOUT, format!("{} did not finish in time", params.name)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_proto::{
        access::{Grant, Scope, SecretKind, Who},
        control::SpawnSpec,
    };

    /// `list` without Silo's own tools.
    fn provided_only(reg: &Registry, principal: &Principal) -> Vec<ToolEntry> {
        list(reg, principal).tools.into_iter().filter(|t| t.provider_id != 0).collect()
    }

    #[test]
    fn silos_own_tools_are_listed_by_block_kind_and_their_names_kept() {
        let mut reg = registry();
        let all = list(&reg, &Principal::Owner).tools;
        let kind = |name: &str| all.iter().find(|t| t.tool.name == name).map(|t| t.block_kind);
        assert_eq!(kind("browser_click"), Some(Some(BlockKind::Browser)));
        assert_eq!(kind("terminal_run"), Some(Some(BlockKind::Terminal)));
        assert_eq!(kind("files_show"), Some(Some(BlockKind::Files)));
        assert_eq!(kind("changes_show"), Some(Some(BlockKind::Changes)));
        assert_eq!(kind("block_list"), Some(None));
        assert!(all.iter().filter(|t| t.provider_id == 0).all(|t| t.provider == SILO_PROVIDER));
        let a = reg.add_client("a".into(), false);
        let error = provide(&mut reg, a, &Principal::Owner, None, vec![tool("browser_click", false)]).unwrap_err();
        assert!(error.message.contains("Silo's own"), "{error:?}");
    }

    fn registry() -> Registry {
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        Registry::new(notes)
    }

    fn tool(name: &str, read_only: bool) -> ProvidedTool {
        ProvidedTool { name: name.into(), description: "d".into(), input_schema: serde_json::json!({"type": "object"}), read_only, approval: false }
    }

    fn token(scope: Scope, rights: Rights) -> Principal {
        Principal::Grant(Box::new(Grant {
            id: "g".into(),
            who: Who::Secret { kind: SecretKind::Token, hash: "h".into() },
            scope,
            rights,
            label: String::new(),
            created_at: 0,
            created_by: String::new(),
            created_by_grant: None,
            expires_at: None,
            last_seen: 0,
        }))
    }

    fn block(reg: &mut Registry) -> String {
        let spawn = SpawnSpec::default();
        reg.create_session(None, &crate::registry::New { spawn: &spawn, kind: BlockKind::Files, url: None, agent: None }).unwrap().block_id
    }

    #[test]
    fn providing_replaces_and_conflicts_are_refused() {
        let mut reg = registry();
        let a = reg.add_client("a".into(), false);
        let b = reg.add_client("b".into(), false);
        provide(&mut reg, a, &Principal::Owner, None, vec![tool("one", true), tool("two", false)]).unwrap();
        assert_eq!(provided_only(&reg, &Principal::Owner).len(), 2);
        provide(&mut reg, a, &Principal::Owner, None, vec![tool("one", true)]).unwrap();
        assert_eq!(provided_only(&reg, &Principal::Owner).len(), 1, "replaced");
        let error = provide(&mut reg, b, &Principal::Owner, None, vec![tool("one", true)]).unwrap_err();
        assert!(error.message.contains("already provided"), "{error:?}");
        assert!(provide(&mut reg, a, &Principal::Owner, None, vec![tool("x", true), tool("x", true)]).is_err(), "twice");
        assert!(provide(&mut reg, a, &Principal::Owner, None, vec![tool("Bad", true)]).is_err(), "invalid");
        reg.tools_client_gone(a);
        assert!(provided_only(&reg, &Principal::Owner).is_empty());
    }

    #[test]
    fn block_tools_go_with_their_block_and_need_its_rights() {
        let mut reg = registry();
        let id = block(&mut reg);
        let other = block(&mut reg);
        let a = reg.add_client("a".into(), false);
        let b = reg.add_client("b".into(), false);
        provide(&mut reg, a, &Principal::Owner, Some(id.clone()), vec![tool("peek", true), tool("poke", false)]).unwrap();
        // Two blocks may offer the same name; a call then names the block.
        provide(&mut reg, b, &Principal::Owner, Some(other.clone()), vec![tool("peek", true)]).unwrap();
        assert!(resolve(&reg, &Principal::Owner, "peek", None).unwrap_err().message.contains("give block_id"));
        assert_eq!(resolve(&reg, &Principal::Owner, "peek", Some(&other)).unwrap().client, b);
        assert_eq!(resolve(&reg, &Principal::Owner, "poke", None).unwrap().client, a);

        let viewer = token(Scope::Block { block_id: id.clone() }, Rights::View);
        let names: Vec<String> = provided_only(&reg, &viewer).into_iter().map(|t| t.tool.name).collect();
        assert_eq!(names, ["peek"], "only what it may call, and only its block's");
        assert!(resolve(&reg, &viewer, "poke", None).is_err());
        let actor = token(Scope::Block { block_id: id.clone() }, Rights::Interact);
        assert_eq!(provided_only(&reg, &actor).len(), 2);
        // Providing needs interact on the block.
        assert!(provide(&mut reg, b, &viewer, Some(id.clone()), vec![tool("x", true)]).is_err());
        assert!(provide(&mut reg, b, &actor, Some(other.clone()), vec![tool("x", true)]).is_err(), "another block");
        assert!(provide(&mut reg, b, &actor, None, vec![tool("x", true)]).is_err(), "the server");

        reg.tools_block_gone(&id);
        let left: Vec<Option<String>> = provided_only(&reg, &Principal::Owner).into_iter().map(|t| t.block_id).collect();
        assert_eq!(left, [Some(other)]);
    }

    #[test]
    fn a_gone_provider_fails_its_pending_calls() {
        let mut reg = registry();
        let a = reg.add_client("a".into(), false);
        let (reply, mut answer) = oneshot::channel();
        reg.tool_calls.insert("r".into(), Pending { client: a, reply });
        reg.tools_client_gone(a);
        assert!(answer.try_recv().is_err());
        assert!(reg.tool_calls.is_empty());
    }
}
