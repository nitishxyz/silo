//! How `silo login-server invite|allow|deny|ticket|reload` reach the
//! running daemon: a unix socket in its state directory (`admin.sock`,
//! `0600` in a `0700` directory, and the peer must be root or the
//! daemon's own uid). One JSON line each way: an [`AdminRequest`], then
//! an [`AdminReply`]. Invites live only in the daemon's memory, so they
//! need it running; the other commands fall back to working on the file.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use silo_transport::{EndpointAddr, LoginTicket};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::Broker;

/// The admin socket's file name in the state directory.
pub const SOCKET: &str = "admin.sock";
/// Largest request line.
const MAX_LINE: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum AdminRequest {
    /// A single-use invite: `{"ticket": "silol1…"}`.
    Invite {
        account: String,
        ttl_secs: u64,
    },
    Allow {
        account: String,
        device: String,
    },
    Deny {
        account: String,
        device: String,
    },
    /// Re-read the accounts file.
    Reload,
    /// The machine's login ticket, without a secret: `{"ticket": …}`.
    Ticket {
        #[serde(default)]
        account: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdminReply {
    Ok(Value),
    Error(String),
}

/// No daemon answers on the admin socket.
#[derive(Debug)]
pub struct NotRunning;

impl std::fmt::Display for NotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the login server is not running (systemctl start silo-login)")
    }
}

impl std::error::Error for NotRunning {}

#[must_use]
pub fn socket(state_dir: &Path) -> PathBuf {
    state_dir.join(SOCKET)
}

/// Send `request` to the daemon with state in `state_dir`.
///
/// # Errors
/// [`NotRunning`], or the daemon's refusal.
pub async fn request(state_dir: &Path, request: &AdminRequest) -> anyhow::Result<Value> {
    let path = socket(state_dir);
    let stream = tokio::net::UnixStream::connect(&path).await.map_err(|_| NotRunning)?;
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    let mut reply = String::new();
    let mut reader = BufReader::new(reader);
    let read = reader.read_line(&mut reply);
    tokio::time::timeout(Duration::from_secs(60), read).await.context("the login server did not answer")??;
    match serde_json::from_str::<AdminReply>(&reply).context("bad reply from the login server")? {
        AdminReply::Ok(value) => Ok(value),
        AdminReply::Error(message) => anyhow::bail!("{message}"),
    }
}

/// Bind the admin socket in `state_dir` (created `0700`).
///
/// # Errors
/// The directory or socket could not be made.
pub fn bind(state_dir: &Path) -> anyhow::Result<tokio::net::UnixListener> {
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
        .create(state_dir)
        .with_context(|| format!("create {}", state_dir.display()))?;
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))?;
    let path = socket(state_dir);
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The daemon's current address, for tickets.
pub type Addr = Arc<dyn Fn() -> EndpointAddr + Send + Sync>;

/// Answer admin requests on `listener` for `broker`.
pub async fn serve(listener: tokio::net::UnixListener, broker: Arc<Broker>, addr: Addr) {
    let own = rustix::process::getuid().as_raw();
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == 0 || cred.uid() == own => {}
            Ok(cred) => {
                tracing::warn!(uid = cred.uid(), "admin request from another user refused");
                continue;
            }
            Err(_) => continue,
        }
        let (broker, addr) = (broker.clone(), addr.clone());
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut line = String::new();
            let mut reader = BufReader::new(tokio::io::AsyncReadExt::take(reader, MAX_LINE));
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            let reply = match serde_json::from_str::<AdminRequest>(&line) {
                Ok(request) => match handle(&broker, &addr, request).await {
                    Ok(value) => AdminReply::Ok(value),
                    Err(error) => AdminReply::Error(format!("{error:#}")),
                },
                Err(error) => AdminReply::Error(format!("bad request: {error}")),
            };
            if let Ok(mut bytes) = serde_json::to_vec(&reply) {
                bytes.push(b'\n');
                let _ = writer.write_all(&bytes).await;
            }
        });
    }
}

async fn handle(broker: &Broker, addr: &Addr, request: AdminRequest) -> anyhow::Result<Value> {
    Ok(match request {
        AdminRequest::Invite { account, ttl_secs } => {
            json!({"ticket": broker.invite_ticket(addr(), &account, Duration::from_secs(ttl_secs))?})
        }
        AdminRequest::Allow { account, device } => json!({"changed": broker.allow(&account, &device).await?}),
        AdminRequest::Deny { account, device } => json!({"changed": broker.deny(&account, &device).await?}),
        AdminRequest::Reload => {
            broker.reload().await?;
            json!({})
        }
        AdminRequest::Ticket { account } => {
            if account.as_deref().is_some_and(|a| !silo_transport::valid_account(a)) {
                anyhow::bail!("not a user name");
            }
            json!({"ticket": LoginTicket { addr: addr(), secret: None, account }.encode()})
        }
    })
}
