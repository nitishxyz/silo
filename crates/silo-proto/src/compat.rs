//! Peers of any build work together when they speak the same
//! [`crate::PROTOCOL`] and [`crate::VT`]. Within a protocol the wire only
//! grows:
//!
//! - JSON types never `deny_unknown_fields`; new fields are
//!   `#[serde(default)]`;
//! - enums that may grow (events, block kinds, browser commands, offer
//!   sources) end in an `Unknown` variant that receivers skip;
//! - unknown frame kinds on established streams are skipped;
//! - an unknown method is answered with [`RpcError::METHOD_NOT_FOUND`].
//!
//! New behaviour is announced as a [`feature`] in the hello; each side
//! gates on the other's. `protocol.json` pins the schema: a change fails a
//! test until the file is updated (or [`crate::PROTOCOL`] bumped).

use crate::control::RpcError;

/// Capability names a control hello may advertise: behaviour added after
/// protocol 1 (everything in protocol 1 every peer has). Gate on them with
/// `Control::peer_supports` (client) and the server's per-client event
/// filter; the app greys out commands that need one.
pub mod feature {
    /// Grants, scoped principals, approvals and their events
    /// (`grants.changed`, `approval.*`, `session.watchers`).
    pub const ACCESS: &str = "access";
    /// The app runs forwarded `app.*` requests (`app.request`).
    pub const APP_REQUESTS: &str = "app_requests";
    /// The owner runs forwarded browser actions.
    pub const BROWSER_ACTIONS: &str = "browser_actions";
    /// The app hosts browsers no pane shows: actions on unowned browser
    /// blocks and `browser.fetch` (opt-in).
    pub const BROWSER_HOST: &str = "browser_host";
    /// Block status events (`block.status`, `block.attention`,
    /// `block.command_finished`).
    pub const STATUS: &str = "status";
    /// The client handles `busy` refusals of closes (and sends `force`).
    /// Opt-in ([`OPT_IN`]): only clients that do advertise it.
    pub const CLOSE_GUARD: &str = "close_guard";
    /// The app shows approval cards (`approval.requested`) and answers
    /// them: only such apps are asked to approve.
    pub const APPROVALS: &str = "approvals";
    /// `block.spawn` takes `kind: browser` (and `url`): a parked browser.
    /// Older servers ignore the kind and spawn a shell, so gate on it.
    pub const PARKED_BROWSERS: &str = "parked_browsers";
    /// Files blocks (`kind: files`), the `files.*` methods and
    /// `files.changed` (docs/FILES_BLOCK.md).
    pub const FILES: &str = "files";
    /// `block.move` docks tiled blocks too, and `Dock.before` docks on the
    /// left or above. Older servers dock floats only, always right or
    /// below (ignoring `before`), so gate on it.
    pub const DOCK_SIDES: &str = "dock_sides";
    /// Files blocks can be edited: `files.write` (with conflict checks
    /// against a base), `files.create`, and `files.read`'s `sha256`. Older
    /// servers answer `method_not_found`, so the app keeps them read-only.
    pub const FILES_WRITE: &str = "files_write";
    /// Changes blocks (`kind: changes`), the `changes.*` methods and
    /// `changes.changed` (docs/CHANGES_BLOCK.md). Older servers answer
    /// `method_not_found`; the app hides Changes there.
    pub const CHANGES: &str = "changes";
    /// The server tunnels to its otto daemon (`TunnelOpen.service`
    /// `"otto"`). Sent only by servers that registered that service, so
    /// it is in neither [`super::FEATURES`] nor [`super::OPT_IN`].
    pub const OTTO: &str = "otto";
}

/// Everything this build supports beyond protocol 1.
pub const FEATURES: &[&str] = &[
    feature::ACCESS,
    feature::APP_REQUESTS,
    feature::STATUS,
    feature::APPROVALS,
    feature::PARKED_BROWSERS,
    feature::FILES,
    feature::DOCK_SIDES,
    feature::FILES_WRITE,
];

/// Features this build's server supports that a client advertises only
/// once it handles them (`silo_client::advertise`), so no client of this
/// build gets them by default.
pub const OPT_IN: &[&str] = &[feature::CLOSE_GUARD, feature::BROWSER_ACTIONS, feature::BROWSER_HOST];

/// [`FEATURES`] as sent in a hello.
#[must_use]
pub fn features() -> Vec<String> {
    FEATURES.iter().map(|&f| f.to_owned()).collect()
}

/// [`FEATURES`] and [`OPT_IN`], as a server's hello sends them.
#[must_use]
pub fn server_features() -> Vec<String> {
    FEATURES.iter().chain(OPT_IN).map(|&f| f.to_owned()).collect()
}

/// Our features a peer advertising `peer` lacks.
#[must_use]
pub fn missing(peer: &[String]) -> Vec<&'static str> {
    missing_of(FEATURES, peer)
}

fn missing_of(ours: &[&'static str], peer: &[String]) -> Vec<&'static str> {
    ours.iter().copied().filter(|f| !peer.iter().any(|p| p == f)).collect()
}

/// Whether a peer speaking `protocol` with snapshot format `vt` can work
/// with this build; the rejection otherwise.
///
/// # Errors
/// [`RpcError::INCOMPATIBLE`] naming what differs.
pub fn check(protocol: u32, vt: &str) -> Result<(), RpcError> {
    if protocol != crate::PROTOCOL {
        return Err(RpcError::incompatible(format!("protocol {protocol} (this side speaks {})", crate::PROTOCOL)));
    }
    if vt != crate::VT {
        return Err(RpcError::incompatible(format!("terminal format {vt} (this side uses {})", crate::VT)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        Frame,
        control::{BrowserCommand, Event, OfferSource, ServerHello, ServerMessage},
        data::{ClientData, DataHello, ServerData},
        model::BlockKind,
    };

    #[test]
    fn same_protocol_and_vt_are_compatible() {
        assert!(check(crate::PROTOCOL, crate::VT).is_ok());
        let old = check(0, crate::VT).unwrap_err();
        assert_eq!(old.code, RpcError::INCOMPATIBLE);
        assert!(old.message.contains("protocol 0"), "{}", old.message);
        assert!(check(crate::PROTOCOL, "ghostsnp1-other").unwrap_err().message.contains("terminal format"));
    }

    #[test]
    fn a_hello_from_before_protocol_numbers_is_rejected() {
        let ancient: DataHello = serde_json::from_value(json!({"build": "b", "ticket": "t"})).unwrap();
        assert!(check(ancient.protocol, &ancient.vt).is_err());
    }

    #[test]
    fn missing_features_are_ours_the_peer_lacks() {
        const SYNTHETIC: &str = "synthetic";
        assert_eq!(missing_of(&[SYNTHETIC], &[]), [SYNTHETIC]);
        assert!(missing_of(&[SYNTHETIC], &[SYNTHETIC.to_owned(), "newer".to_owned()]).is_empty());
        assert!(missing(&features()).is_empty());
    }

    #[test]
    fn a_newer_peer_s_extra_fields_and_variants_are_skipped() {
        let mut reply = serde_json::to_value(ServerHello::ours(
            7,
            serde_json::from_value(json!({
                "version": "9.9.9", "pid": 1, "server_id": "s", "started_at": 0, "endpoint_id": "e", "idle_exit_secs": 0,
                "from_the_future": true
            }))
            .unwrap(),
        ))
        .unwrap();
        reply["also_new"] = json!({"x": 1});
        let reply: ServerHello = serde_json::from_value(reply).unwrap();
        assert_eq!((reply.client_id, reply.server.hostname.as_str()), (7, ""));

        let event: ServerMessage = serde_json::from_value(json!({"seq": 3, "event": "thing.new", "data": {"a": 1}})).unwrap();
        assert!(matches!(event, ServerMessage::UnknownEvent { seq: 3 }));
        let known: ServerMessage = serde_json::from_value(json!({"seq": 4, "event": "block.bell", "data": {"block_id": "b", "new": 1}})).unwrap();
        assert!(matches!(known, ServerMessage::Event { seq: 4, event } if matches!(*event, Event::BlockBell { .. })));
        // A known event with unparsable data is not silently skipped.
        let bad: ServerMessage = serde_json::from_value(json!({"seq": 5, "event": "session.changed", "data": {"view": 1}})).unwrap();
        assert!(matches!(bad, ServerMessage::MalformedEvent { seq: 5, ref event, .. } if event == "session.changed"), "{bad:?}");
        let kind: BlockKind = serde_json::from_value(json!("hologram")).unwrap();
        assert_eq!(kind, BlockKind::Unknown);
        let command: BrowserCommand = serde_json::from_value(json!({"type": "print", "copies": 2})).unwrap();
        assert_eq!(command, BrowserCommand::Unknown);
        let source: OfferSource = serde_json::from_value(json!({"type": "qr"})).unwrap();
        assert_eq!(source, OfferSource::Unknown);
        let empty: crate::control::Empty = serde_json::from_value(json!({"later": 1})).unwrap();
        let _ = empty;
    }

    #[test]
    fn unknown_data_frames_are_skipped() {
        assert_eq!(ServerData::from_frame(Frame::new(0xee, vec![1, 2])).unwrap(), ServerData::Unknown(0xee));
        assert_eq!(ClientData::from_frame(Frame::new(0xef, vec![])).unwrap(), ClientData::Unknown(0xef));
    }
}
