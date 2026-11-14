//! The broker: answers devices on `silo/login/1` (see the crate docs for
//! the framing), redeems invites, keeps the accounts file and has
//! accounts' servers admit and revoke devices.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use data_encoding::BASE32_NOPAD_NOCASE;
use serde_json::Value;
use silo_proto::{
    Frame,
    control::{ClientHello, ClientRole, Method, Request, RpcError, ServerHello, ServerMessage},
    kind,
    login::{LOGIN_PROTOCOL, LoginAdmitParams, LoginAdmitted, LoginAnswer, LoginError, LoginReply, LoginRequest, LoginRevokeParams},
    write_frame,
};
use silo_transport::{BiStream, EndpointAddr, INVITE_SECRET_LEN, LoginIncoming, LoginTicket, RemoteConn, valid_account};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::{
    code,
    policy::Policy,
    spawner::Spawner,
    users::{self, User, Users},
};

/// Largest login request read.
pub const MAX_REQUEST: usize = 8 * 1024;
/// Largest reply frame read from an account's server.
const MAX_REPLY: usize = 1024 * 1024;
/// A device must send its request within this time.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A whole login, starting the account's server included.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(45);
/// How long a started server has to answer on its socket.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// One call on an account's server.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// Logins handled at once; more are closed at once.
const MAX_CONCURRENT: usize = 32;
/// Default and longest invite lifetimes.
pub const DEFAULT_INVITE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAX_INVITE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Failed attempts a device may make per [`FAILURE_WINDOW`].
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// Devices whose failures are remembered at once (the oldest go first).
const MAX_TRACKED: usize = 4096;
/// The client name the login server's hellos carry.
const CLIENT_NAME: &str = "silo login server";

fn error(code: &str, message: impl Into<String>) -> LoginError {
    LoginError { code: code.to_owned(), message: message.into() }
}

/// The text of an invite's secret in a [`LoginRequest`]: lowercase
/// unpadded base32.
#[must_use]
pub fn encode_invite(secret: &[u8; INVITE_SECRET_LEN]) -> String {
    BASE32_NOPAD_NOCASE.encode(secret).to_ascii_lowercase()
}

fn decode_invite(text: &str) -> Option<[u8; INVITE_SECRET_LEN]> {
    BASE32_NOPAD_NOCASE.decode(text.trim().as_bytes()).ok()?.try_into().ok()
}

struct Invite {
    account: String,
    expires: Instant,
}

struct Failures {
    count: u32,
    since: Instant,
}

/// A device a server admitted through this login server: which account's
/// server, as which account. Revoked there when the device loses it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Admitted {
    server: String,
    device: String,
    account: String,
}

/// Why a call on an account's server failed.
#[derive(Debug)]
pub enum CallError {
    /// Nothing answered, or not as that account's server.
    Unreachable(anyhow::Error),
    /// The server refused the hello or the call.
    Refused(RpcError),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(error) => write!(f, "{error:#}"),
            Self::Refused(error) => write!(f, "{}: {}", error.code, error.message),
        }
    }
}

async fn read_max<R: AsyncRead + Unpin>(reader: &mut R, max: usize) -> std::io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > max {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad frame length {len}")));
    }
    let mut rest = vec![0; len];
    reader.read_exact(&mut rest).await?;
    Ok(Some(Frame { kind: rest[0], payload: rest.split_off(1) }))
}

/// Call `method` on the server at `socket`, which must belong to `uid`,
/// as the login server (`role: login`).
///
/// # Errors
/// See [`CallError`].
pub async fn call(socket: &Path, uid: u32, method: Method) -> Result<Value, CallError> {
    let unreachable = |e: anyhow::Error| CallError::Unreachable(e);
    let work = async {
        let mut stream =
            tokio::net::UnixStream::connect(socket).await.with_context(|| format!("connect {}", socket.display())).map_err(unreachable)?;
        // A socket path is the account's to point anywhere: only its own
        // server counts.
        let owner = stream.peer_cred().context("peer credentials").map_err(unreachable)?.uid();
        if owner != uid {
            return Err(unreachable(anyhow::anyhow!("{} is served by uid {owner}, not the account's", socket.display())));
        }
        let hello = ClientHello { role: ClientRole::Login, ..ClientHello::ours(CLIENT_NAME, true, None) };
        let io = |e: std::io::Error| unreachable(e.into());
        write_frame(&mut stream, &Frame::json(kind::CONTROL_HELLO, &hello).map_err(io)?).await.map_err(io)?;
        let first = read_max(&mut stream, MAX_REPLY).await.map_err(io)?.ok_or_else(|| unreachable(anyhow::anyhow!("the server hung up")))?;
        match first.kind {
            kind::CONTROL_HELLO => {
                let hello: ServerHello = first.parse().map_err(io)?;
                if !hello.features.iter().any(|f| f == silo_proto::compat::feature::LOGIN) {
                    return Err(CallError::Refused(RpcError::method_not_found(&silo_proto::api::method_name(&method))));
                }
            }
            _ => {
                return Err(match first.parse::<ServerMessage>() {
                    Ok(ServerMessage::Response { error: Some(error), .. }) => CallError::Refused(error),
                    _ => unreachable(anyhow::anyhow!("unexpected reply to the hello")),
                });
            }
        }
        write_frame(&mut stream, &Frame::json(kind::CONTROL, &Request { id: 1, method }).map_err(io)?).await.map_err(io)?;
        loop {
            let frame = read_max(&mut stream, MAX_REPLY).await.map_err(io)?.ok_or_else(|| unreachable(anyhow::anyhow!("the server hung up")))?;
            if frame.kind != kind::CONTROL {
                continue;
            }
            if let Ok(ServerMessage::Response { id: 1, result, error }) = frame.parse::<ServerMessage>() {
                let _ = stream.shutdown().await;
                return match error {
                    Some(error) => Err(CallError::Refused(error)),
                    None => Ok(result.unwrap_or(Value::Null)),
                };
            }
        }
    };
    tokio::time::timeout(CALL_TIMEOUT, work).await.unwrap_or_else(|_| Err(unreachable(anyhow::anyhow!("the server did not answer in time"))))
}

pub struct Broker {
    accounts: PathBuf,
    policy: tokio::sync::Mutex<Policy>,
    users: Arc<dyn Users>,
    spawner: Arc<dyn Spawner>,
    /// `--unprivileged`: the one account served.
    only: Option<String>,
    invites: Mutex<HashMap<[u8; INVITE_SECRET_LEN], Invite>>,
    failures: Mutex<HashMap<String, Failures>>,
    admitted: Mutex<HashSet<Admitted>>,
    /// Starts are rare: one at a time, so two logins never start one
    /// account's server twice.
    starting: tokio::sync::Mutex<()>,
}

impl Broker {
    /// A broker over the accounts file at `accounts` (read now; a missing
    /// file lets nobody in). `only`: serve that account alone.
    ///
    /// # Errors
    /// An unreadable or invalid accounts file.
    pub fn new(accounts: PathBuf, users: Arc<dyn Users>, spawner: Arc<dyn Spawner>, only: Option<String>) -> anyhow::Result<Self> {
        let policy = Policy::load(&accounts)?;
        Ok(Self {
            accounts,
            policy: tokio::sync::Mutex::new(policy),
            users,
            spawner,
            only,
            invites: Mutex::default(),
            failures: Mutex::default(),
            admitted: Mutex::default(),
            starting: tokio::sync::Mutex::new(()),
        })
    }

    #[must_use]
    pub fn accounts_path(&self) -> &Path {
        &self.accounts
    }

    /// A copy of the policy in force.
    pub async fn policy(&self) -> Policy {
        self.policy.lock().await.clone()
    }

    /// Answer every login on `incoming` until the endpoint closes.
    pub async fn serve(self: Arc<Self>, mut incoming: LoginIncoming) {
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));
        while let Some((rc, stream)) = incoming.next().await {
            let Ok(slot) = slots.clone().try_acquire_owned() else {
                tracing::debug!(peer = %rc.remote_id(), "too many logins at once");
                rc.close(b"busy");
                continue;
            };
            let broker = self.clone();
            tokio::spawn(async move {
                broker.connection(rc, stream).await;
                drop(slot);
            });
        }
    }

    async fn connection(&self, rc: RemoteConn, mut stream: BiStream) {
        let device = rc.remote_id().to_string();
        let request = match tokio::time::timeout(REQUEST_TIMEOUT, read_max(&mut stream, MAX_REQUEST)).await {
            Ok(Ok(Some(frame))) if frame.kind == kind::CONTROL_HELLO => frame.parse::<LoginRequest>().ok(),
            _ => None,
        };
        let answer = match request {
            Some(request) => match tokio::time::timeout(LOGIN_TIMEOUT, self.answer(&device, request)).await {
                Ok(answer) => answer,
                Err(_) => LoginAnswer::Error(error(code::UNAVAILABLE, "the login took too long")),
            },
            None => LoginAnswer::Error(error(code::BAD_REQUEST, "expected one login request")),
        };
        if let Ok(frame) = Frame::json(kind::CONTROL, &answer) {
            let _ = write_frame(&mut stream, &frame).await;
            let _ = stream.shutdown().await;
        }
        // Let the answer reach the device before the connection goes.
        let _ = tokio::time::timeout(Duration::from_secs(5), rc.closed()).await;
        rc.close(b"done");
    }

    /// [`Self::login`] with rate limiting and logging: what the device
    /// gets back.
    pub async fn answer(&self, device: &str, request: LoginRequest) -> LoginAnswer {
        if self.limited(device) {
            tracing::warn!(device, "login refused: too many failed attempts");
            return LoginAnswer::Error(error(code::RATE_LIMITED, "too many failed attempts; retry in a minute"));
        }
        match self.login(device, request).await {
            Ok(reply) => {
                self.failures.lock().unwrap().remove(device);
                tracing::info!(device, account = reply.account, server = reply.server_id, "login admitted");
                LoginAnswer::Ok(reply)
            }
            Err(refusal) => {
                self.failed(device);
                tracing::warn!(device, code = refusal.code, "login refused: {}", refusal.message);
                LoginAnswer::Error(refusal)
            }
        }
    }

    fn limited(&self, device: &str) -> bool {
        let failures = self.failures.lock().unwrap();
        failures.get(device).is_some_and(|f| f.count >= MAX_FAILURES && f.since.elapsed() < FAILURE_WINDOW)
    }

    fn failed(&self, device: &str) {
        let mut failures = self.failures.lock().unwrap();
        if failures.len() >= MAX_TRACKED {
            failures.retain(|_, f| f.since.elapsed() < FAILURE_WINDOW);
            if failures.len() >= MAX_TRACKED
                && let Some(oldest) = failures.iter().min_by_key(|(_, f)| f.since).map(|(d, _)| d.clone())
            {
                failures.remove(&oldest);
            }
        }
        let entry = failures.entry(device.to_owned()).or_insert(Failures { count: 0, since: Instant::now() });
        if entry.since.elapsed() >= FAILURE_WINDOW {
            *entry = Failures { count: 0, since: Instant::now() };
        }
        entry.count += 1;
    }

    fn regular(&self) -> impl Fn(&str) -> bool + '_ {
        |name: &str| users::regular(self.users.as_ref(), name)
    }

    /// Log `device` in: redeem its invite, choose its account, start that
    /// account's server (or `server_of`'s) and have it admit the device.
    ///
    /// # Errors
    /// The [`LoginError`] the device gets.
    pub async fn login(&self, device: &str, request: LoginRequest) -> Result<LoginReply, LoginError> {
        if request.protocol != LOGIN_PROTOCOL {
            return Err(error(code::PROTOCOL, format!("this login server speaks protocol {LOGIN_PROTOCOL}, not {}", request.protocol)));
        }
        for name in [&request.account, &request.server_of].into_iter().flatten() {
            if !valid_account(name) {
                return Err(error(code::BAD_REQUEST, "an account is not a user name"));
            }
        }
        let name: String = request.name.chars().filter(|c| !c.is_control()).take(silo_proto::text::MAX_TITLE).collect();
        let mut asked = request.account.clone();
        if let Some(invite) = &request.invite {
            asked = Some(self.redeem(device, invite, request.account.as_deref(), &name).await?);
        }
        let account = {
            let policy = self.policy.lock().await;
            policy.choose(device, asked.as_deref(), &self.regular())?
        };
        let user = users::check(self.users.as_ref(), &account)?;
        let target = match request.server_of.as_deref() {
            Some(owner) if owner != account => users::check(self.users.as_ref(), owner)?,
            _ => user,
        };
        if self.only.as_ref().is_some_and(|only| *only != target.name || *only != account) {
            return Err(error(code::NOT_ALLOWED, "this login server serves only its own account"));
        }
        let params = LoginAdmitParams { device: device.to_owned(), name, account: account.clone() };
        let admitted = self.on_server(&target, Method::LoginAdmit(params)).await.map_err(|e| match e {
            CallError::Refused(e) if e.code == RpcError::METHOD_NOT_FOUND => {
                error(code::TOO_OLD, format!("{}'s Silo server is too old for the login server; upgrade it", target.name))
            }
            CallError::Refused(e) if e.code == RpcError::FORBIDDEN => error(code::NOT_ALLOWED, e.message),
            other => error(code::UNAVAILABLE, format!("{}'s Silo server: {other}", target.name)),
        })?;
        let admitted: LoginAdmitted = serde_json::from_value(admitted).map_err(|e| error(code::INTERNAL, format!("bad login.admit reply: {e}")))?;
        let addr = silo_transport::decode_addr(&admitted.ticket).map_err(|e| error(code::INTERNAL, format!("{e:#}")))?;
        self.admitted.lock().unwrap().insert(Admitted { server: target.name.clone(), device: device.to_owned(), account: account.clone() });
        Ok(LoginReply { account, ticket: admitted.ticket, server_id: addr.id.to_string() })
    }

    /// Redeem an invite for `device`: single use; its device is written
    /// into the accounts file. The invite's account.
    async fn redeem(&self, device: &str, invite: &str, asked: Option<&str>, name: &str) -> Result<String, LoginError> {
        let invalid = || error(code::INVITE, "invalid, expired or already used invite");
        let secret = decode_invite(invite).ok_or_else(invalid)?;
        let account = {
            let mut invites = self.invites.lock().unwrap();
            invites.retain(|_, i| i.expires > Instant::now());
            let account = invites.get(&secret).ok_or_else(invalid)?.account.clone();
            if asked.is_some_and(|asked| asked != account) {
                return Err(error(code::INVITE, "the invite is for another account"));
            }
            invites.remove(&secret);
            account
        };
        let written = self.edit(|policy| policy.allow(&account, device, name)).await;
        if let Err(e) = written {
            // Not consumed when the device could not be written down.
            self.invites.lock().unwrap().insert(secret, Invite { account, expires: Instant::now() + DEFAULT_INVITE_TTL });
            return Err(error(code::INTERNAL, format!("could not record the device: {e:#}")));
        }
        tracing::info!(device, account, "invite redeemed; device added");
        Ok(account)
    }

    /// Make sure `user`'s server runs, then call `method` on it.
    async fn on_server(&self, user: &User, method: Method) -> Result<Value, CallError> {
        let socket = self.spawner.socket(user);
        match call(&socket, user.uid, method.clone()).await {
            Err(CallError::Unreachable(first)) => {
                tracing::debug!(account = user.name, "server not answering ({first:#}); starting it");
                self.start(user, &socket).await.map_err(CallError::Unreachable)?;
                call(&socket, user.uid, method).await
            }
            other => other,
        }
    }

    async fn start(&self, user: &User, socket: &Path) -> anyhow::Result<()> {
        let _one = self.starting.lock().await;
        let answers = || async { tokio::net::UnixStream::connect(socket).await.is_ok() };
        if answers().await {
            return Ok(());
        }
        self.spawner.start(user)?;
        let deadline = Instant::now() + START_TIMEOUT;
        while Instant::now() < deadline {
            if answers().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("its server did not start in time")
    }

    /// A single-use invite for `account`, valid for `ttl`: its secret.
    ///
    /// # Errors
    /// A bad account or ttl, an account that may not log in.
    pub fn invite(&self, account: &str, ttl: Duration) -> anyhow::Result<[u8; INVITE_SECRET_LEN]> {
        if !valid_account(account) {
            anyhow::bail!("{account:?} is not a user name");
        }
        if ttl.is_zero() || ttl > MAX_INVITE_TTL {
            anyhow::bail!("an invite lasts between 1 s and {} days", MAX_INVITE_TTL.as_secs() / 86_400);
        }
        users::check(self.users.as_ref(), account).map_err(|e| anyhow::anyhow!("{}", e.message))?;
        if self.only.as_ref().is_some_and(|only| only != account) {
            anyhow::bail!("this login server serves only {}", self.only.as_deref().unwrap_or_default());
        }
        let secret = rand::random::<[u8; INVITE_SECRET_LEN]>();
        let mut invites = self.invites.lock().unwrap();
        invites.retain(|_, i| i.expires > Instant::now());
        invites.insert(secret, Invite { account: account.to_owned(), expires: Instant::now() + ttl });
        tracing::info!(account, ttl_secs = ttl.as_secs(), "invite created");
        Ok(secret)
    }

    /// The login ticket of an invite for `account` at `addr`.
    ///
    /// # Errors
    /// See [`Self::invite`].
    pub fn invite_ticket(&self, addr: EndpointAddr, account: &str, ttl: Duration) -> anyhow::Result<String> {
        let secret = self.invite(account, ttl)?;
        Ok(LoginTicket { addr, secret: Some(secret), account: Some(account.to_owned()) }.encode())
    }

    /// Let `device` use `account`.
    ///
    /// # Errors
    /// A bad name or id, the file could not be written.
    pub async fn allow(&self, account: &str, device: &str) -> anyhow::Result<bool> {
        self.edit(|policy| policy.allow(account, device, "")).await
    }

    /// Stop `device` using `account`; its servers revoke it.
    ///
    /// # Errors
    /// A bad id, the file could not be written.
    pub async fn deny(&self, account: &str, device: &str) -> anyhow::Result<bool> {
        self.edit(|policy| policy.deny(account, device)).await
    }

    /// Re-read the accounts file (`SIGHUP`, `silo login-server reload`);
    /// a bad file keeps the policy in force.
    ///
    /// # Errors
    /// An unreadable or invalid file.
    pub async fn reload(&self) -> anyhow::Result<()> {
        let new = Policy::load(&self.accounts)?;
        let old = std::mem::replace(&mut *self.policy.lock().await, new.clone());
        tracing::info!(path = %self.accounts.display(), "accounts reloaded");
        self.revoke_lost(&old, &new).await;
        Ok(())
    }

    /// Change the accounts file as it is on disk now (hand edits not yet
    /// reloaded are kept), write it and put it in force.
    async fn edit(&self, change: impl FnOnce(&mut Policy) -> anyhow::Result<bool>) -> anyhow::Result<bool> {
        let (old, new, changed) = {
            let mut policy = self.policy.lock().await;
            let mut new = Policy::load(&self.accounts)?;
            let changed = change(&mut new)?;
            if changed {
                new.save(&self.accounts)?;
            }
            let old = std::mem::replace(&mut *policy, new.clone());
            (old, new, changed)
        };
        self.revoke_lost(&old, &new).await;
        Ok(changed)
    }

    /// Have servers revoke the devices that lost an account between `old`
    /// and `new`: those the old file named for it, and those admitted
    /// since this daemon started (a `*` device's accounts are known only
    /// that way).
    async fn revoke_lost(&self, old: &Policy, new: &Policy) {
        let mut candidates: HashSet<Admitted> = self.admitted.lock().unwrap().clone();
        for (account, device) in old.pairs() {
            if account != crate::policy::ANY {
                candidates.insert(Admitted { server: account.clone(), device, account });
            }
        }
        let regular = self.regular();
        for lost in candidates.into_iter().filter(|c| !new.allows(&c.device, &c.account, &regular)) {
            self.admitted.lock().unwrap().remove(&lost);
            let Some(server) = self.users.user(&lost.server) else { continue };
            if self.only.as_ref().is_some_and(|only| *only != server.name) {
                continue;
            }
            let params = LoginRevokeParams { device: lost.device.clone(), account: Some(lost.account.clone()) };
            match self.on_server(&server, Method::LoginRevoke(params)).await {
                Ok(_) => tracing::info!(device = lost.device, account = lost.account, server = lost.server, "device revoked"),
                Err(e) => tracing::warn!(device = lost.device, account = lost.account, server = lost.server, "could not revoke: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_secrets_round_trip() {
        let secret = [7u8; INVITE_SECRET_LEN];
        assert_eq!(decode_invite(&encode_invite(&secret)), Some(secret));
        assert_eq!(decode_invite(&encode_invite(&secret).to_ascii_uppercase()), Some(secret));
        assert_eq!(decode_invite("abc"), None);
        assert_eq!(decode_invite(""), None);
    }
}
