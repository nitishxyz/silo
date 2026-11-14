//! The login server (`docs/LOGIN_SERVER.md`): a root daemon that lets a
//! device into its unix account's own Silo server. It identifies the
//! device by its iroh endpoint id, maps it to an account with
//! `/etc/silo/accounts.json`, makes sure that account's `silo server`
//! runs, has it admit the device (`login.admit` on its unix socket) and
//! tells the device where it is. It never carries a session's bytes.
//!
//! The exchange on `silo/login/1`: the device opens one bi stream and
//! writes one frame (`silo_proto` framing: `u32 LE length | kind | JSON`)
//! of kind `CONTROL_HELLO` holding a [`LoginRequest`]; the login server
//! answers one `CONTROL` frame holding a [`LoginAnswer`] and finishes the
//! stream.
//!
//! [`LoginRequest`]: silo_proto::login::LoginRequest
//! [`LoginAnswer`]: silo_proto::login::LoginAnswer

pub mod admin;
pub mod broker;
pub mod daemon;
pub mod install;
pub mod policy;
pub mod spawner;
pub mod users;

pub use broker::Broker;
pub use daemon::{RunOptions, run};

/// Where the policy lives.
pub const DEFAULT_ACCOUNTS: &str = "/etc/silo/accounts.json";
/// The daemon's state: its identity key and admin socket.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/silo-login";

/// [`LoginError`](silo_proto::login::LoginError) codes.
pub mod code {
    /// The device's protocol is not this one.
    pub const PROTOCOL: &str = "protocol";
    pub const BAD_REQUEST: &str = "bad_request";
    /// The device may not use the account (or any).
    pub const NOT_ALLOWED: &str = "not_allowed";
    /// The device has several accounts and no default: name one.
    pub const CHOOSE_ACCOUNT: &str = "choose_account";
    /// The account does not exist, is locked, has no login shell, or
    /// logins are off (`/etc/nologin`).
    pub const REFUSED: &str = "refused";
    /// An invite that is invalid, expired, used or for another account.
    pub const INVITE: &str = "invite";
    /// Too many failed attempts from this device; retry later.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// The account's server could not be started or reached.
    pub const UNAVAILABLE: &str = "unavailable";
    /// The account's server predates `login.admit`.
    pub const TOO_OLD: &str = "too_old";
    pub const INTERNAL: &str = "internal";
}
