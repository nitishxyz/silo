//! MCP server settings (`mcp.*` methods, the `mcp.status` and `mcp.changed` events).
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Where a server is configured: the user's `mcp.json` (every project) or the project's `.mcp.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpScope {
    Global,
    Project,
}

/// How otto reaches a server. Values are as written: `${VAR}` and `${VAR:-default}` expand when it starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransport {
    /// A local process speaking line-delimited JSON-RPC on stdin/stdout.
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    /// Streamable HTTP; servers that need OAuth sign in with `mcp.login`.
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        /// An OAuth client registered with the server by hand, for servers
        /// that do not let otto register itself (no dynamic client
        /// registration). Its redirect URL is [`McpOAuth::callback_url`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<McpOAuth>,
    },
}

/// A pre-registered OAuth client of an HTTP server (`.mcp.json` `oauth`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct McpOAuth {
    pub client_id: String,
    /// For a confidential client; `${VAR}` keeps it out of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// The loopback port the redirect URL was registered with (default
    /// [`MCP_CALLBACK_PORT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    /// Scopes to ask for, space separated (default: the server's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// The redirect URL port of a pre-registered client by default.
pub const MCP_CALLBACK_PORT: u16 = 33418;

impl McpOAuth {
    /// The redirect (callback) URL to register with the server's OAuth app.
    #[must_use]
    pub fn callback_url(port: Option<u16>) -> String {
        format!("http://127.0.0.1:{}/callback", port.unwrap_or(MCP_CALLBACK_PORT))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum McpStatus {
    Connected,
    Connecting,
    Error {
        message: String,
    },
    /// An HTTP server that wants an OAuth sign-in: `mcp.login`.
    NeedsSignIn,
    Disabled,
}

/// One configured server (`mcp.list`). A project server overrides a global one of the same name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct McpServer {
    pub name: String,
    pub scope: McpScope,
    pub transport: McpTransport,
    pub enabled: bool,
    pub status: McpStatus,
    /// The server's own tools (its resources and prompts tools not counted); 0 until connected.
    pub tools: u32,
}

/// A started sign-in (`mcp.login`): open `url`. The browser returns to the daemon's loopback
/// `callback` (a remote client forwards that port); `mcp.login_complete` waits for it, or takes
/// the callback URL the user pastes when the loopback can't be reached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct McpLoginStarted {
    pub login_id: String,
    pub url: String,
    pub callback: crate::LoginCallback,
}
