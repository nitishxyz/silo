//! Access rules: grants `{who, scope, rights}` and the one table saying
//! what every control method needs (see `docs/PHASE7.md` §3). The server
//! checks [`need`] in one place; `api.describe` publishes it as [`Access`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    control::Method,
    model::{BlockId, SessionId},
};

/// What a principal may do on what a grant covers. Ordered: each level
/// includes the ones below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Rights {
    /// See output and layout.
    View,
    /// Type, resize, focus, drive browsers.
    Interact,
    /// Create, close and rearrange.
    Control,
    /// Everything, including pairing, grants and stopping the server.
    Owner,
}

/// What a grant covers: the server (everything), one session with every
/// window and block in it (including later ones), or one block.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Scope {
    Server,
    Session {
        session_id: SessionId,
    },
    Block {
        block_id: BlockId,
    },
    /// A newer scope: covers nothing here.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SecretKind {
    /// An agent token (`silot1…`), for the unix socket.
    Token,
    /// A share link's secret.
    Link,
    #[serde(other)]
    Unknown,
}

/// Who a grant is for.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Who {
    /// The iroh EndpointId of a paired device key.
    Device { id: String },
    /// Whoever presents a bearer secret; `hash` is its lowercase hex
    /// SHA-256 (see [`secret_hash`]).
    Secret { kind: SecretKind, hash: String },
    /// An account of a login server (reserved; not admitted yet).
    Account { name: String },
    #[serde(other)]
    Unknown,
}

/// A grant. Immutable: changing one is revoke + create.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Grant {
    /// 16 lowercase hex digits.
    pub id: String,
    pub who: Who,
    pub scope: Scope,
    pub rights: Rights,
    /// "work laptop", "share with Ana", "claude-code".
    #[serde(default)]
    pub label: String,
    /// Unix seconds.
    pub created_at: u64,
    /// The creating client's label.
    #[serde(default)]
    pub created_by: String,
    /// The grant whose principal created this one (a paired device that
    /// minted a link or token, or whose invite paired this device):
    /// removing or expiring that grant removes this one too. `None` for
    /// the local owner's grants and for grants from before it was kept.
    #[serde(default)]
    pub created_by_grant: Option<String>,
    /// Unix seconds; `None` until revoked.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Unix seconds of the last admitted hello.
    #[serde(default)]
    pub last_seen: u64,
}

impl Grant {
    /// Whether the grant has expired at `now` (unix seconds).
    #[must_use]
    pub fn expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

/// What a connection admitted by a grant may do: its hello says so, so a
/// shared link's client knows what it sees (a block share lists no
/// sessions) and whether it may type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Granted {
    pub scope: Scope,
    pub rights: Rights,
    /// Unix seconds; `None` until revoked.
    #[serde(default)]
    pub expires_at: Option<u64>,
}

impl From<&Grant> for Granted {
    fn from(grant: &Grant) -> Self {
        Self { scope: grant.scope.clone(), rights: grant.rights, expires_at: grant.expires_at }
    }
}

/// Longest grant lifetime `grant.create` accepts.
pub const MAX_GRANT_TTL_SECS: u64 = 30 * 24 * 60 * 60;
/// A link's lifetime when `grant.create` names none.
pub const DEFAULT_LINK_TTL_SECS: u64 = 24 * 60 * 60;
/// Bytes of a bearer secret.
pub const SECRET_LEN: usize = 32;
/// Prefix of an agent token's text.
pub const TOKEN_PREFIX: &str = "silot1";

const BASE32: data_encoding::Encoding = data_encoding::BASE32_NOPAD;

/// The text form of a secret: `silot1…` for a token, bare lowercase
/// base32 for a link.
#[must_use]
pub fn encode_secret(kind: SecretKind, secret: &[u8; SECRET_LEN]) -> String {
    let body = BASE32.encode(secret).to_ascii_lowercase();
    match kind {
        SecretKind::Token => format!("{TOKEN_PREFIX}{body}"),
        _ => body,
    }
}

/// The kind and bytes of a secret's text form (case-insensitive).
#[must_use]
pub fn decode_secret(text: &str) -> Option<(SecretKind, [u8; SECRET_LEN])> {
    let text = text.trim();
    let (kind, body) = match text.get(..TOKEN_PREFIX.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(TOKEN_PREFIX) => (SecretKind::Token, &text[TOKEN_PREFIX.len()..]),
        _ => (SecretKind::Link, text),
    };
    let bytes = BASE32.decode(body.to_ascii_uppercase().as_bytes()).ok()?;
    Some((kind, bytes.try_into().ok()?))
}

/// Lowercase hex SHA-256 of a secret: all a server stores.
#[must_use]
pub fn secret_hash(secret: &[u8; SECRET_LEN]) -> String {
    Sha256::digest(secret).iter().map(|b| format!("{b:02x}")).collect()
}

/// Where a method's right is checked, and which param holds the id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Server,
    /// A session id (or name).
    Session(&'static str),
    /// A window id, checked on its session.
    Window(&'static str),
    /// A block id, checked on the block.
    Block(&'static str),
    /// A block id, checked on its session.
    BlockSession(&'static str),
    /// A list of block ids, checked on each block.
    Blocks(&'static str),
}

impl Target {
    /// The level the right is checked at, and the param naming it.
    #[must_use]
    pub fn describe(self) -> (TargetKind, Option<&'static str>) {
        match self {
            Self::Server => (TargetKind::Server, None),
            Self::Session(p) | Self::Window(p) | Self::BlockSession(p) => (TargetKind::Session, Some(p)),
            Self::Block(p) => (TargetKind::Block, Some(p)),
            Self::Blocks(p) => (TargetKind::Blocks, Some(p)),
        }
    }
}

/// What a method needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// Any principal except a join invite's.
    Open,
    /// Only a join invite's principal.
    Offer,
    /// `rights` on `on`; with `approval`, a human approves a call that is
    /// not from a Silo app.
    Rights { on: Target, rights: Rights, approval: bool },
    /// Only the app client the request was forwarded to.
    AppReply,
}

const fn rights(on: Target, rights: Rights) -> Need {
    Need::Rights { on, rights, approval: false }
}

const fn approved(rights: Rights) -> Need {
    Need::Rights { on: Target::Server, rights, approval: true }
}

/// What `method` needs. Exhaustive: a new method without an entry does
/// not compile.
#[must_use]
pub fn need(method: &Method) -> Need {
    use Method as M;
    use Rights::{Control, Interact, Owner, View};
    use Target::{Block, BlockSession, Blocks, Server, Session, Window};
    const BLOCK: &str = "block_id";
    const SESSION: &str = "session_id";
    const WINDOW: &str = "window_id";
    match method {
        M::BrowserSnapshot(_) | M::BrowserFind(_) | M::BrowserWaitFor(_) | M::BrowserConsole(_) | M::BrowserNetwork(_) | M::BrowserScreenshot(_) => {
            rights(Block(BLOCK), View)
        }
        M::BlockReport(_)
        | M::BlockNotify(_)
        | M::BrowserClick(_)
        | M::BrowserHover(_)
        | M::BrowserFill(_)
        | M::BrowserFillForm(_)
        | M::BrowserType(_)
        | M::BrowserPress(_)
        | M::BrowserScroll(_)
        | M::BrowserDialog(_) => rights(Block(BLOCK), Interact),
        M::BrowserEval(_) => rights(Block(BLOCK), Control),
        // Runs pages on the user's Mac with the agent profile's cookies.
        M::BrowserFetch(_) => rights(Server, Control),
        M::ServerInfo {}
        | M::ApiList {}
        | M::ApiDescribe { .. }
        | M::Ping {}
        | M::EventsSubscribe {}
        | M::SessionList {}
        | M::BlocksBusy { .. }
        | M::ApprovalAnswer { .. } => Need::Open,
        M::HostOffer { .. } => Need::Offer,
        M::AppReply { .. } => Need::AppReply,

        M::BlockView { .. }
        | M::TerminalCapture { .. }
        | M::TerminalSize { .. }
        | M::DataConnect { .. }
        | M::BlockCommands { .. }
        | M::TerminalCommandOutput { .. } => rights(Block(BLOCK), View),
        M::SessionView { .. } => rights(Session(SESSION), View),

        M::TerminalSend { .. }
        | M::TerminalResize { .. }
        | M::ClipboardRead { .. }
        | M::ClipboardWrite { .. }
        | M::BrowserNavigate { .. }
        | M::BrowserBack { .. }
        | M::BrowserForward { .. }
        | M::BrowserReload { .. }
        | M::BrowserZoom { .. }
        | M::BrowserReport { .. }
        | M::AgentSet { .. }
        | M::BlockFocus { .. }
        | M::BlockFocusStep { .. }
        | M::BlockFocusDir { .. }
        | M::BlockStatusSeen { .. } => rights(Block(BLOCK), Interact),
        M::OwnBlock { .. } | M::DisownBlock { .. } => rights(Blocks("block_ids"), Interact),
        M::WindowSelect { .. } | M::WindowStep { .. } | M::UiOpenSession { .. } => rights(Session(SESSION), Interact),

        M::SessionRename { .. } | M::SessionClose { .. } | M::WindowCreate { .. } => rights(Session(SESSION), Control),
        M::WindowClose { .. } | M::WindowRename { .. } | M::WindowMove { .. } | M::LayoutRatio { .. } | M::LayoutEqualize { .. } => {
            rights(Window(WINDOW), Control)
        }
        M::BlockSplit { .. }
        | M::BlockSpawn { .. }
        | M::BlockPark { .. }
        | M::BlockReveal { .. }
        | M::BlockClose { .. }
        | M::BlockZoom { .. }
        | M::BlockSwap { .. }
        | M::BlockFloat { .. }
        | M::BlockMove { .. }
        | M::LayoutGrow { .. }
        | M::LayoutEdge { .. }
        | M::TerminalClear { .. }
        | M::TerminalReset { .. } => rights(BlockSession(BLOCK), Control),

        M::SessionCreate { .. }
        | M::AppWindowNew { .. }
        | M::AppWindowCycle { .. }
        | M::AppSwitcher { .. }
        | M::AppSessionStep { .. }
        | M::AppHostsRename { .. }
        | M::AppHostsList { .. }
        | M::AppCopy { .. }
        | M::AppPaste { .. }
        | M::AppSelectAll { .. }
        | M::AppAppearance { .. }
        | M::AppPaneHeaders { .. }
        | M::AppFullScreen { .. }
        | M::AppSettingsOpen { .. }
        | M::AppSettingsReload { .. }
        | M::AppSettingsOpenFile { .. }
        | M::AppSettingsGet { .. }
        | M::AppSettingsSet { .. }
        | M::AppUpdateCheck { .. }
        | M::AppPalette { .. }
        | M::AppHide { .. }
        | M::AppHideOthers { .. }
        | M::AppShowAll { .. }
        | M::AppQuit { .. }
        | M::AppJumpPrompt { .. }
        | M::AppSelectOutput { .. }
        | M::AppCopyOutput { .. }
        | M::AppOpenShare { .. } => rights(Server, Control),

        M::ServerLog { .. } | M::PairList {} | M::GrantList {} => rights(Server, Owner),
        M::ServerShutdown { .. }
        | M::ServerRestart { .. }
        | M::PairCreate { .. }
        | M::PairRevoke { .. }
        | M::GrantCreate { .. }
        | M::GrantRevoke { .. }
        | M::AppHostsAdd { .. }
        | M::AppHostsRemove { .. } => approved(Owner),
    }
}

/// How a method's need reads in `api.describe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccessKind {
    Open,
    Offer,
    Rights,
    AppReply,
    #[serde(other)]
    Unknown,
}

/// The level a right is checked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Server,
    Session,
    Block,
    /// Each block of a list.
    Blocks,
    #[serde(other)]
    Unknown,
}

/// `api.describe`'s `access`: what calling the method needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Access {
    pub kind: AccessKind,
    /// Where the right is checked (`kind: rights`).
    #[serde(default)]
    pub target: Option<TargetKind>,
    /// The param holding the target's id.
    #[serde(default)]
    pub param: Option<String>,
    #[serde(default)]
    pub rights: Option<Rights>,
    /// A call not from a Silo app waits for a human to approve it.
    #[serde(default)]
    pub approval: bool,
    /// Forwarded to a connected Silo app.
    #[serde(default)]
    pub app: bool,
}

/// [`need`] of `method` (named `name`) as published.
#[must_use]
pub fn access(name: &str, method: &Method) -> Access {
    let app = name.starts_with("app.") && name != "app.reply";
    let plain = |kind| Access { kind, target: None, param: None, rights: None, approval: false, app };
    match need(method) {
        Need::Open => plain(AccessKind::Open),
        Need::Offer => plain(AccessKind::Offer),
        Need::AppReply => plain(AccessKind::AppReply),
        Need::Rights { on, rights, approval } => {
            let (target, param) = on.describe();
            Access { kind: AccessKind::Rights, target: Some(target), param: param.map(str::to_owned), rights: Some(rights), approval, app }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights_are_ordered() {
        assert!(Rights::View < Rights::Interact && Rights::Interact < Rights::Control && Rights::Control < Rights::Owner);
        assert_eq!(serde_json::to_value(Rights::Interact).unwrap(), "interact");
    }

    #[test]
    fn secrets_round_trip_and_never_read_as_each_other() {
        let secret = [7u8; SECRET_LEN];
        let token = encode_secret(SecretKind::Token, &secret);
        assert!(token.starts_with(TOKEN_PREFIX), "{token}");
        assert_eq!(decode_secret(&token), Some((SecretKind::Token, secret)));
        assert_eq!(decode_secret(&token.to_ascii_uppercase()), Some((SecretKind::Token, secret)));
        let link = encode_secret(SecretKind::Link, &secret);
        assert_eq!(decode_secret(&link), Some((SecretKind::Link, secret)));
        assert!(decode_secret("silo1abc").is_none());
        assert!(decode_secret(&link[1..]).is_none());
        assert_eq!(secret_hash(&secret).len(), 64);
        assert_ne!(secret_hash(&secret), secret_hash(&[8; SECRET_LEN]));
    }

    #[test]
    fn newer_scopes_and_principals_parse() {
        let scope: Scope = serde_json::from_value(serde_json::json!({"type": "window", "window_id": "w"})).unwrap();
        assert_eq!(scope, Scope::Unknown);
        let who: Who = serde_json::from_value(serde_json::json!({"type": "group", "name": "x"})).unwrap();
        assert_eq!(who, Who::Unknown);
        let who: Who = serde_json::from_value(serde_json::json!({"type": "account", "name": "ana"})).unwrap();
        assert_eq!(who, Who::Account { name: "ana".into() });
    }
}
