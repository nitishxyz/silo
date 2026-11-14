//! The login server's exchange (`docs/LOGIN_SERVER.md` §5, §10, feature
//! `login`): what a device sends on the `silo/login/1` stream and what it
//! gets back, and the `login.*` methods the login server calls on an
//! account's server.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Version of the [`LoginRequest`] exchange.
pub const LOGIN_PROTOCOL: u32 = 1;

/// A device asking the login server for an account's server (one per
/// stream).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoginRequest {
    /// The device's [`LOGIN_PROTOCOL`].
    #[serde(default)]
    pub protocol: u32,
    /// The account to use; `None`: the device's default, else its only one.
    #[serde(default)]
    pub account: Option<String>,
    /// Another account's server to reach as `account` (sharing by name,
    /// `docs/LOGIN_SERVER.md` §7).
    #[serde(default)]
    pub server_of: Option<String>,
    /// An invite's secret, redeemed once.
    #[serde(default)]
    pub invite: Option<String>,
    /// The device's name, the label of the grant it gets.
    #[serde(default)]
    pub name: String,
    /// The device's Silo version, for display.
    #[serde(default)]
    pub version: String,
}

/// Where the account's server is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoginReply {
    pub account: String,
    /// The account's server's pairing-free ticket (its address).
    pub ticket: String,
    pub server_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoginError {
    pub code: String,
    pub message: String,
}

/// The login server's answer to a [`LoginRequest`]: `{"ok": …}` or
/// `{"error": …}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LoginAnswer {
    Ok(LoginReply),
    Error(LoginError),
}

/// Params of `login.admit`: give `device` a device grant on this server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoginAdmitParams {
    /// The device's endpoint id (64 hex).
    pub device: String,
    /// The device's name.
    #[serde(default)]
    pub name: String,
    /// The account the device logs in as: this server's user, or one an
    /// account grant covers.
    pub account: String,
}

/// Result of `login.admit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoginAdmitted {
    /// This server's pairing-free ticket (its address).
    pub ticket: String,
    /// The device grant made or found.
    pub grant_id: String,
}

/// Params of `login.revoke`: remove the grants `login.admit` made for
/// `device` (of `account` only, when given).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoginRevokeParams {
    pub device: String,
    #[serde(default)]
    pub account: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn answers_are_tagged_ok_or_error() {
        let ok = LoginAnswer::Ok(LoginReply { account: "alice".into(), ticket: "silo1x".into(), server_id: "s".into() });
        assert_eq!(serde_json::to_value(&ok).unwrap(), json!({"ok": {"account": "alice", "ticket": "silo1x", "server_id": "s"}}));
        let error: LoginAnswer = serde_json::from_value(json!({"error": {"code": "no_account", "message": "m"}})).unwrap();
        assert_eq!(error, LoginAnswer::Error(LoginError { code: "no_account".into(), message: "m".into() }));
        let request: LoginRequest = serde_json::from_value(json!({"protocol": 1, "name": "laptop"})).unwrap();
        assert_eq!((request.protocol, request.account, request.name.as_str()), (LOGIN_PROTOCOL, None, "laptop"));
    }
}
