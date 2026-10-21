//! Confirm before closing busy blocks (Phase 7 §7.5): `blocks.busy` and
//! the check run before every close.

use std::time::Duration;

use silo_proto::{
    compat::feature,
    control::{BusyBlock, BusyBlocks, Method, RpcError},
    model::{BlockId, SessionId, WindowId},
    text::{MAX_TITLE, untrusted},
};

use crate::{
    Server,
    access::{On, Principal, rights_on, sees_block, sees_session},
    block::BlockMsg,
    browser::Block,
    registry::Registry,
};

/// Programs named in a refusal's message; the rest are counted.
const NAMED: usize = 3;

/// How long a close waits for the fresh probes. A block whose actor has
/// not answered by then counts as its last published status says.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A block a close would end, located in the tree.
struct Target {
    block_id: BlockId,
    session_id: SessionId,
    window_id: WindowId,
    session_name: String,
}

/// Whether `method` is one of the closes the guard checks.
pub fn is_close(method: &Method) -> bool {
    matches!(
        method,
        Method::BlockClose { .. }
            | Method::WindowClose { .. }
            | Method::SessionClose { .. }
            | Method::ServerShutdown { .. }
            | Method::ServerRestart { .. }
    )
}

/// May `method` (a close) from `client_id` go ahead? Only clients that
/// advertise `close_guard` are checked; `force` skips the check. All or
/// nothing: one busy block refuses the whole close.
///
/// # Errors
/// `busy` when it would end running programs, `data` = [`BusyBlocks`].
pub async fn check(server: &Server, client_id: u64, method: &Method) -> Result<(), RpcError> {
    let targets = {
        let reg = server.registry.lock().unwrap();
        if !reg.clients.get(&client_id).is_some_and(|c| c.supports(Some(feature::CLOSE_GUARD))) {
            return Ok(());
        }
        match method {
            Method::BlockClose { force: false, block_id } => tree(&reg).into_iter().filter(|t| &t.block_id == block_id).collect(),
            Method::WindowClose { force: false, window_id } => tree(&reg).into_iter().filter(|t| &t.window_id == window_id).collect(),
            Method::SessionClose { force: false, session_id } => {
                // Unknown sessions are the handler's to report.
                let Some(id) = reg.session_id(session_id) else { return Ok(()) };
                tree(&reg).into_iter().filter(|t| t.session_id == id).collect()
            }
            Method::ServerShutdown { force: false } | Method::ServerRestart { force: false } => tree(&reg),
            _ => return Ok(()),
        }
    };
    let busy = probe(server, targets).await;
    if busy.is_empty() {
        return Ok(());
    }
    let data = serde_json::to_value(BusyBlocks { busy: busy.clone() }).map_err(RpcError::internal)?;
    Err(RpcError::new(RpcError::BUSY, message(&busy)).with_data(data))
}

/// `blocks.busy`: the busy blocks among those named (none named: the
/// whole server), limited to those `principal` can view.
///
/// # Errors
/// `not_found` for a named session, window or block that does not exist
/// or, for a principal not covering the server, of which it sees nothing.
pub async fn busy_blocks(
    server: &Server,
    principal: &Principal,
    session_id: Option<SessionId>,
    window_id: Option<WindowId>,
    block_ids: Option<Vec<BlockId>>,
) -> Result<BusyBlocks, RpcError> {
    let targets = {
        let reg = server.registry.lock().unwrap();
        let server_wide = rights_on(principal, On::Server).is_some();
        let visible = |t: &Target| sees_block(principal, &t.block_id, Some(&t.session_id));
        let all = tree(&reg);
        let mut wanted: Vec<bool> = vec![session_id.is_none() && window_id.is_none() && block_ids.is_none(); all.len()];
        let mut pick = |what: String, matches: &dyn Fn(&Target) -> bool, session_seen: bool| -> Result<(), RpcError> {
            let mut seen = session_seen || server_wide;
            for (i, t) in all.iter().enumerate().filter(|(_, t)| matches(t)) {
                seen |= visible(t);
                wanted[i] = true;
            }
            if seen { Ok(()) } else { Err(RpcError::not_found(what)) }
        };
        if let Some(id) = &session_id {
            let sid = reg.session_id(id).ok_or_else(|| RpcError::not_found(format!("session {id}")))?;
            pick(format!("session {id}"), &|t| t.session_id == sid, sees_session(principal, &sid))?;
        }
        if let Some(id) = &window_id {
            let sid = reg.window_session(id).ok_or_else(|| RpcError::not_found(format!("window {id}")))?;
            pick(format!("window {id}"), &|t| &t.window_id == id, sees_session(principal, &sid))?;
        }
        for id in block_ids.iter().flatten() {
            if !all.iter().any(|t| &t.block_id == id) {
                return Err(RpcError::not_found(format!("block {id}")));
            }
            pick(format!("block {id}"), &|t| &t.block_id == id, false)?;
        }
        all.into_iter().zip(wanted).filter(|(t, wanted)| *wanted && visible(t)).map(|(t, _)| t).collect()
    };
    Ok(BusyBlocks { busy: probe(server, targets).await })
}

/// Every block in the tree, sessions and windows in order.
fn tree(reg: &Registry) -> Vec<Target> {
    reg.sessions
        .iter()
        .flat_map(|s| {
            s.windows.iter().flat_map(move |w| {
                w.blocks().into_iter().map(move |block_id| Target {
                    block_id,
                    session_id: s.id.clone(),
                    window_id: w.id.clone(),
                    session_name: s.name.clone(),
                })
            })
        })
        .collect()
}

/// Ask each terminal target's actor, which re-probes its foreground
/// first, whether it is busy. All are asked before any answer is awaited.
/// An actor that does not answer within [`PROBE_TIMEOUT`] (all share the
/// deadline) is taken at its last published status, so a stuck block
/// cannot hang the close.
async fn probe(server: &Server, targets: Vec<Target>) -> Vec<BusyBlock> {
    let asked: Vec<_> = {
        let reg = server.registry.lock().unwrap();
        targets
            .into_iter()
            .filter_map(|t| {
                let handle = reg.blocks.get(&t.block_id).and_then(Block::terminal)?;
                let title = handle.info.lock().unwrap().title.clone();
                let (reply, rx) = tokio::sync::oneshot::channel();
                handle.send(BlockMsg::Busy { reply });
                Some((t, title, handle.status.clone(), rx))
            })
            .collect()
    };
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    let mut busy = Vec::new();
    for (t, title, status, rx) in asked {
        let found = match tokio::time::timeout_at(deadline, rx).await {
            // An actor gone meanwhile ended its program.
            Ok(answer) => answer.ok().flatten(),
            Err(_) => status.lock().unwrap().busy.clone(),
        };
        if let Some(found) = found {
            busy.push(BusyBlock {
                block_id: t.block_id,
                session_id: t.session_id,
                window_id: t.window_id,
                session_name: t.session_name,
                block_title: untrusted(&title, MAX_TITLE),
                busy: found,
            });
        }
    }
    busy
}

/// `vim is running in 'api'`, or `3 programs are running: vim in 'api',
/// cargo in 'build', ssh in 'prod' and 2 more`.
pub fn message(busy: &[BusyBlock]) -> String {
    let one = |b: &BusyBlock| format!("{} in '{}'", b.busy.process, b.block_title);
    match busy {
        [b] => format!("{} is running in '{}'", b.busy.process, b.block_title),
        _ => {
            let named: Vec<String> = busy.iter().take(NAMED).map(one).collect();
            let mut text = format!("{} programs are running: {}", busy.len(), named.join(", "));
            if busy.len() > NAMED {
                text.push_str(&format!(" and {} more", busy.len() - NAMED));
            }
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::model::Busy;

    use super::*;

    fn busy(process: &str, title: &str) -> BusyBlock {
        BusyBlock {
            block_id: "b".into(),
            session_id: "s".into(),
            window_id: "w".into(),
            session_name: "main".into(),
            block_title: title.into(),
            busy: Busy { process: process.into(), command: None, since: 0 },
        }
    }

    #[test]
    fn the_message_names_at_most_three_programs() {
        assert_eq!(message(&[busy("vim", "api")]), "vim is running in 'api'");
        assert_eq!(message(&[busy("vim", "api"), busy("cargo", "build")]), "2 programs are running: vim in 'api', cargo in 'build'");
        let five = [busy("vim", "api"), busy("cargo", "build"), busy("ssh", "prod"), busy("less", "log"), busy("top", "top")];
        assert_eq!(message(&five), "5 programs are running: vim in 'api', cargo in 'build', ssh in 'prod' and 2 more");
    }
}
