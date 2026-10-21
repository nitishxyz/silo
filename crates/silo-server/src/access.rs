//! The one place a call's access is checked: who the caller is (a
//! [`Principal`], resolved once at the hello) against what the method
//! needs ([`silo_proto::access::need`]).

use serde_json::Value;
use silo_proto::{
    access::{Grant, Need, Rights, Scope, Target, Who, need},
    api::method_name,
    control::{ClientRole, Method, RpcError},
    model::WatcherKind,
};

use crate::{grants::unix_secs, registry::Registry};

/// Who a connection acts as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    /// The unix socket without a token: the user.
    Owner,
    Grant(Box<Grant>),
    /// A redeemed join invite: `host.offer`, once, and nothing else.
    Offer {
        invite_id: String,
    },
}

impl Principal {
    pub fn grant_id(&self) -> Option<&str> {
        match self {
            Self::Grant(grant) => Some(&grant.id),
            _ => None,
        }
    }

    /// How a watcher with this principal is shown.
    pub fn watcher_kind(&self) -> WatcherKind {
        match self {
            Self::Owner => WatcherKind::Owner,
            Self::Grant(grant) => match &grant.who {
                Who::Device { .. } => WatcherKind::Device,
                Who::Secret { kind: silo_proto::access::SecretKind::Token, .. } => WatcherKind::Token,
                Who::Secret { kind: silo_proto::access::SecretKind::Link, .. } => WatcherKind::Link,
                _ => WatcherKind::Unknown,
            },
            Self::Offer { .. } => WatcherKind::Unknown,
        }
    }
}

/// What a right is checked on.
#[derive(Clone, Copy, Debug)]
pub enum On<'a> {
    Server,
    Session(&'a str),
    /// A block, with its session when it is in the tree.
    Block {
        block: &'a str,
        session: Option<&'a str>,
    },
}

/// The rights `principal` holds on `on`; `None` when it does not cover it.
pub fn rights_on(principal: &Principal, on: On<'_>) -> Option<Rights> {
    let grant = match principal {
        Principal::Owner => return Some(Rights::Owner),
        Principal::Offer { .. } => return None,
        Principal::Grant(grant) => grant,
    };
    if grant.expired(unix_secs()) {
        return None;
    }
    let covered = match (&grant.scope, on) {
        (Scope::Server, _) => true,
        (Scope::Session { session_id }, On::Session(s) | On::Block { session: Some(s), .. }) => session_id == s,
        (Scope::Block { block_id }, On::Block { block, .. }) => block_id == block,
        _ => false,
    };
    covered.then_some(grant.rights)
}

/// Whether `principal` sees a session's tree and events.
pub fn sees_session(principal: &Principal, session: &str) -> bool {
    rights_on(principal, On::Session(session)).is_some()
}

/// Whether `principal` sees a block's events.
pub fn sees_block(principal: &Principal, block: &str, session: Option<&str>) -> bool {
    rights_on(principal, On::Block { block, session }).is_some()
}

/// What passing the check leaves to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gate {
    /// A human must approve the call first (see [`crate::approval`]).
    pub approval: bool,
}

/// Whether `principal`, as a client of `role`, may call `method`.
///
/// # Errors
/// `unauthorized` for a join invite calling anything but `host.offer`,
/// `not_found` for ids outside the principal's coverage, `forbidden` for
/// too few rights.
pub fn check(principal: &Principal, role: ClientRole, method: &Method, reg: &Registry) -> Result<Gate, RpcError> {
    let name = || method_name(method);
    let open = Gate { approval: false };
    let need = need(method);
    match (need, principal) {
        (Need::Offer, Principal::Offer { .. }) => return Ok(open),
        (Need::Offer, _) => return Err(RpcError::forbidden(&name())),
        (_, Principal::Offer { .. }) => return Err(RpcError::unauthorized("a join invite only permits host.offer")),
        _ => {}
    }
    let (on, wanted, approval) = match need {
        Need::Open => return Ok(open),
        Need::AppReply if role == ClientRole::App => return Ok(open),
        Need::AppReply => return Err(RpcError::forbidden(&name())),
        Need::Rights { on, rights, approval } => (on, rights, approval),
        Need::Offer => unreachable!("handled above"),
    };
    let params = serde_json::to_value(method).map_err(RpcError::internal)?.get_mut("params").map(Value::take).unwrap_or_default();
    let id = |param: &str| -> Result<String, RpcError> {
        params.get(param).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| RpcError::bad_request(format!("{param} is required")))
    };
    // Ids that do not exist pass for a principal covering the server (the
    // method reports them); for anyone else they are outside coverage.
    let server_wide = rights_on(principal, On::Server).is_some();
    let missing = |what: String| if server_wide { Ok(()) } else { Err(RpcError::not_found(what)) };
    let mut targets: Vec<(Option<String>, Option<String>)> = Vec::new(); // (block, session)
    match on {
        Target::Server => {}
        Target::Session(p) => {
            let id = id(p)?;
            match reg.session_id(&id) {
                Some(s) => targets.push((None, Some(s))),
                None => missing(format!("session {id}"))?,
            }
        }
        Target::Window(p) => {
            let id = id(p)?;
            match reg.window_session(&id) {
                Some(s) => targets.push((None, Some(s))),
                None => missing(format!("window {id}"))?,
            }
        }
        Target::BlockSession(p) => {
            let id = id(p)?;
            match reg.block_session(&id) {
                Some(s) => targets.push((None, Some(s))),
                None => missing(format!("block {id}"))?,
            }
        }
        Target::Block(p) => {
            let id = id(p)?;
            let session = reg.block_session(&id);
            if session.is_none() && !reg.blocks.contains_key(&id) {
                missing(format!("block {id}"))?;
            } else {
                targets.push((Some(id), session));
            }
        }
        Target::Blocks(p) => {
            let ids: Vec<String> =
                params.get(p).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
            for id in ids {
                let session = reg.block_session(&id);
                if session.is_none() && !reg.blocks.contains_key(&id) {
                    missing(format!("block {id}"))?;
                } else {
                    targets.push((Some(id), session));
                }
            }
        }
    }
    let ons: Vec<On<'_>> = if matches!(on, Target::Server) {
        vec![On::Server]
    } else {
        targets
            .iter()
            .map(|(block, session)| match block {
                Some(block) => On::Block { block, session: session.as_deref() },
                None => On::Session(session.as_deref().unwrap_or_default()),
            })
            .collect()
    };
    for target in ons {
        match rights_on(principal, target) {
            Some(held) if held >= wanted => {}
            Some(_) => return Err(RpcError::forbidden(&name())),
            None => {
                return Err(match target {
                    On::Server => RpcError::forbidden(&name()),
                    On::Session(s) => RpcError::not_found(format!("session {s}")),
                    On::Block { block, .. } => RpcError::not_found(format!("block {block}")),
                });
            }
        }
    }
    Ok(Gate { approval: approval && role != ClientRole::App })
}

#[cfg(test)]
mod tests {
    use silo_proto::{
        access::{SecretKind, Who},
        control::{InvitePurpose, SpawnSpec},
        data::ClipKind,
        model::{Axis, BlockKind, Layout},
    };

    use super::*;
    use crate::registry::{Session, Window};

    /// Session `s` (window `w`: blocks `a | b`) and session `t` (window
    /// `x`: block `c`).
    fn registry() -> Registry {
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        let mut reg = Registry::new(notes);
        let mut layout = Layout::block("a");
        layout.split("a", "b".into(), Axis::Horizontal);
        let session = |id: &str, window: Window| Session {
            id: id.into(),
            name: format!("{id}-name"),
            active_window: window.id.clone(),
            windows: vec![window],
            revision: 1,
            created_at: 0,
        };
        reg.sessions.push(session("s", Window { layout, ..Window::new("w".into(), "a") }));
        reg.sessions.push(session("t", Window::new("x".into(), "c")));
        reg
    }

    fn grant(who: Who, scope: Scope, rights: Rights) -> Principal {
        Principal::Grant(Box::new(Grant {
            id: "g".into(),
            who,
            scope,
            rights,
            label: String::new(),
            created_at: 0,
            created_by: String::new(),
            expires_at: None,
            last_seen: 0,
        }))
    }

    fn link(scope: Scope, rights: Rights) -> Principal {
        grant(Who::Secret { kind: SecretKind::Link, hash: "h".into() }, scope, rights)
    }

    /// One method per row of the need table, each principal's outcome.
    #[test]
    fn rights_matrix() {
        let reg = registry();
        let session = |s: &str| Scope::Session { session_id: s.into() };
        let principals = [
            ("owner", Principal::Owner),
            ("device", grant(Who::Device { id: "d".into() }, Scope::Server, Rights::Control)),
            ("session view link", link(session("s"), Rights::View)),
            ("session interact link", link(session("s"), Rights::Interact)),
            ("block view link", link(Scope::Block { block_id: "a".into() }, Rights::View)),
            ("token", grant(Who::Secret { kind: SecretKind::Token, hash: "t".into() }, session("s"), Rights::Control)),
        ];
        let b = || "a".to_owned();
        let rows: Vec<(&str, Method)> = vec![
            ("open", Method::SessionList {}),
            ("offer", Method::HostOffer { ticket: "t".into(), name: "n".into() }),
            ("app reply", Method::AppReply { request_id: "r".into(), result: None, error: None }),
            ("view block", Method::TerminalCapture { block_id: b(), history: false }),
            ("view session", Method::SessionView { session_id: "s".into() }),
            ("interact block", Method::TerminalSend { block_id: b(), text: "x".into() }),
            ("interact session", Method::WindowSelect { session_id: "s".into(), window_id: "w".into() }),
            ("control session", Method::BlockClose { block_id: b() }),
            ("control server", Method::SessionCreate { name: None, spawn: SpawnSpec::default(), kind: BlockKind::Terminal, url: None }),
            ("owner", Method::PairList {}),
            ("owner approval", Method::PairCreate { ttl_secs: 60, purpose: InvitePurpose::Device }),
            ("owner approval clipboard", Method::ClipboardRead { block_id: b(), kind: ClipKind::Text }),
            ("other session", Method::TerminalCapture { block_id: "c".into(), history: false }),
        ];
        const OK: &str = "ok";
        const APPROVE: &str = "approve";
        const F: &str = RpcError::FORBIDDEN;
        const N: &str = "not_found";
        // owner, device, session view link, session interact link, block view link, token
        let expected: [[&str; 6]; 13] = [
            [OK, OK, OK, OK, OK, OK],
            [F, F, F, F, F, F],
            [F, F, F, F, F, F],
            [OK, OK, OK, OK, OK, OK],
            [OK, OK, OK, OK, N, OK],
            [OK, OK, F, OK, F, OK],
            [OK, OK, F, OK, N, OK],
            [OK, OK, F, F, N, OK],
            [OK, OK, F, F, F, F],
            [OK, F, F, F, F, F],
            [APPROVE, F, F, F, F, F],
            [APPROVE, F, F, F, F, F],
            [OK, OK, N, N, N, N],
        ];
        for ((row, method), expected) in rows.iter().zip(expected) {
            for ((who, principal), want) in principals.iter().zip(expected) {
                let got = match check(principal, ClientRole::Cli, method, &reg) {
                    Ok(Gate { approval: true }) => APPROVE.to_owned(),
                    Ok(_) => OK.to_owned(),
                    Err(e) => e.code,
                };
                assert_eq!(got, want, "{row} as {who}");
            }
        }
        // An app's own owner calls need no approval; app.reply is the app's.
        let create = Method::PairCreate { ttl_secs: 60, purpose: InvitePurpose::Device };
        assert_eq!(check(&Principal::Owner, ClientRole::App, &create, &reg), Ok(Gate { approval: false }));
        assert!(check(&Principal::Owner, ClientRole::App, &rows[2].1, &reg).is_ok());
        // A join invite may offer and nothing else.
        let offer = Principal::Offer { invite_id: "i".into() };
        assert!(check(&offer, ClientRole::Cli, &rows[1].1, &reg).is_ok());
        assert_eq!(check(&offer, ClientRole::Cli, &rows[0].1, &reg).unwrap_err().code, RpcError::UNAUTHORIZED);
        // Every block of a multi-block call must be covered.
        let own = |ids: &[&str]| Method::OwnBlock { block_ids: ids.iter().map(|&s| s.to_owned()).collect(), mode: Default::default() };
        let interact = link(session("s"), Rights::Interact);
        assert!(check(&interact, ClientRole::Cli, &own(&["a", "b"]), &reg).is_ok());
        assert_eq!(check(&interact, ClientRole::Cli, &own(&["a", "c"]), &reg).unwrap_err().code, N);
        // Unknown ids: the method reports them for server-wide principals.
        let ghost = Method::TerminalCapture { block_id: "zz".into(), history: false };
        assert!(check(&Principal::Owner, ClientRole::Cli, &ghost, &reg).is_ok());
        assert_eq!(check(&interact, ClientRole::Cli, &ghost, &reg).unwrap_err().code, N);
        // Sessions may be named.
        assert!(check(&interact, ClientRole::Cli, &Method::SessionView { session_id: "s-name".into() }, &reg).is_ok());
    }

    #[test]
    fn expired_grants_cover_nothing() {
        let Principal::Grant(mut g) = link(Scope::Server, Rights::View) else { unreachable!() };
        g.expires_at = Some(1);
        assert_eq!(rights_on(&Principal::Grant(g), On::Server), None);
    }
}
